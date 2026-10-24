//! Shared lowering from the typed PyX IR to Cranelift IR.
//!
//! The same lowering feeds the JIT (`backend/jit.rs`) and the object-file
//! backend (`backend/aot.rs`); the only difference is the `Module`
//! implementation and the target flags. All runtime entry points are declared
//! as imports with the exact signatures from `docs/ARCHITECTURE.md` §3.

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    self, types, AbiParam, Block, FuncRef, InstBuilder, MemFlags, Signature, StackSlot,
    StackSlotData, StackSlotKind, Value,
};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use pyx_ir::{
    BinOp, BoolOp, Builtin, Callee, CompClause, CompTarget, Const, ExcKind, Expr, ExprKind,
    Function, FunctionId, Handler, LocalId, LocalInfo, Param, Program, Stmt, StmtKind,
};

use crate::CodegenError;

/// The `None` immediate, see `ARCHITECTURE.md` §3.1.
const NONE_TAG: i64 = 1;
const FALSE_TAG: i64 = 9;
const TRUE_TAG: i64 = 17;

/// Build the native target ISA with the flags appropriate for the backend.
///
/// The JIT requires position-dependent code (`is_pic=false`); the object file
/// is linked into a PIE and therefore uses `is_pic=true`.
pub(crate) fn build_isa(pic: bool) -> Result<OwnedTargetIsa, CodegenError> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", if pic { "true" } else { "false" })
        .map_err(|e| CodegenError::new(format!("invalid cranelift flag is_pic: {e}")))?;
    flag_builder
        .set("use_colocated_libcalls", "false")
        .map_err(|e| {
            CodegenError::new(format!(
                "invalid cranelift flag use_colocated_libcalls: {e}"
            ))
        })?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| CodegenError::new(format!("invalid cranelift flag opt_level: {e}")))?;
    let flags = settings::Flags::new(flag_builder);
    let isa = cranelift_native::builder()
        .map_err(|e| CodegenError::new(format!("native target not supported: {e}")))?
        .finish(flags)
        .map_err(|e| CodegenError::new(format!("failed to build target ISA: {e}")))?;
    Ok(isa)
}

fn module_error(context: &str, err: impl std::fmt::Display) -> CodegenError {
    CodegenError::new(format!("{context}: {err}"))
}

fn make_signature<M: Module>(module: &M, params: &[ir::Type], ret: Option<ir::Type>) -> Signature {
    let mut sig = module.make_signature();
    for &param in params {
        sig.params.push(AbiParam::new(param));
    }
    if let Some(ret) = ret {
        sig.returns.push(AbiParam::new(ret));
    }
    sig
}

fn declare_runtime<M: Module>(
    module: &mut M,
    name: &str,
    params: &[ir::Type],
    ret: Option<ir::Type>,
) -> Result<FuncId, CodegenError> {
    let sig = make_signature(module, params, ret);
    module
        .declare_function(name, Linkage::Import, &sig)
        .map_err(|e| module_error(&format!("cannot declare runtime symbol {name}"), e))
}

/// Declared runtime entry points used by the generated code.
struct Runtime {
    str_new: FuncId,
    bytes_new: FuncId,
    bytes_build: FuncId,
    float_new: FuncId,
    binop: FuncId,
    unop: FuncId,
    truthy: FuncId,
    print: FuncId,
    len: FuncId,
    to_str: FuncId,
    format: FuncId,
    to_int: FuncId,
    to_float: FuncId,
    abs: FuncId,
    list_new: FuncId,
    list_append: FuncId,
    tuple_new: FuncId,
    set_new: FuncId,
    set_add: FuncId,
    set_from: FuncId,
    range_new: FuncId,
    list_from: FuncId,
    dict_from: FuncId,
    function_new: FuncId,
    closure_new: FuncId,
    builtin_function: FuncId,
    call: FuncId,
    map: FuncId,
    filter: FuncId,
    sum: FuncId,
    enumerate: FuncId,
    zip: FuncId,
    unpack: FuncId,
    dict_new: FuncId,
    dict_set: FuncId,
    subscript: FuncId,
    subscript_set: FuncId,
    slice: FuncId,
    module_call: FuncId,
    module_get: FuncId,
    getattr: FuncId,
    setattr: FuncId,
    class_new: FuncId,
    method: FuncId,
    method_kw: FuncId,
    iter_new: FuncId,
    iter_next: FuncId,
    set_file: FuncId,
    set_line: FuncId,
    frame_enter: FuncId,
    safepoint: FuncId,
    frame_leave: FuncId,
    exc_pending: FuncId,
    exc_tag: FuncId,
    exc_value: FuncId,
    exc_clear: FuncId,
    raise: FuncId,
    raise_value: FuncId,
    uncaught: FuncId,
    runtime_init: FuncId,
    register_function: FuncId,
    spawn_child: FuncId,
}

impl Runtime {
    fn declare<M: Module>(module: &mut M) -> Result<Runtime, CodegenError> {
        let ptr = module.target_config().pointer_type();
        let i64t = types::I64;
        let i32t = types::I32;
        Ok(Runtime {
            str_new: declare_runtime(module, "pyx_str_new", &[ptr, ptr], Some(i64t))?,
            // Raw byte buffer plus length for `bytes` literals.
            bytes_new: declare_runtime(module, "pyx_bytes_new", &[ptr, ptr], Some(i64t))?,
            // `argc` is `uint64_t` in the ABI: pointer-width.
            bytes_build: declare_runtime(module, "pyx_bytes_build", &[ptr, ptr], Some(i64t))?,
            float_new: declare_runtime(module, "pyx_float_new", &[types::F64], Some(i64t))?,
            binop: declare_runtime(module, "pyx_binop", &[i32t, i64t, i64t], Some(i64t))?,
            unop: declare_runtime(module, "pyx_unop", &[i32t, i64t], Some(i64t))?,
            truthy: declare_runtime(module, "pyx_truthy", &[i64t], Some(i64t))?,
            print: declare_runtime(module, "pyx_print", &[i64t, ptr], Some(i64t))?,
            len: declare_runtime(module, "pyx_len", &[i64t], Some(i64t))?,
            to_str: declare_runtime(module, "pyx_to_str", &[i64t], Some(i64t))?,
            // One f-string field: value, static spec (ptr+len) and a `uint8_t`
            // conversion flag passed in a 32-bit register (C ABI).
            format: declare_runtime(
                module,
                "pyx_format_value",
                &[i64t, ptr, ptr, i32t],
                Some(i64t),
            )?,
            to_int: declare_runtime(module, "pyx_to_int", &[i64t], Some(i64t))?,
            to_float: declare_runtime(module, "pyx_to_float", &[i64t], Some(i64t))?,
            abs: declare_runtime(module, "pyx_abs", &[i64t], Some(i64t))?,
            // `argc`/lengths are `usize`/`uint64_t` in the ABI: pointer-width.
            list_new: declare_runtime(module, "pyx_list_new", &[ptr, ptr], Some(i64t))?,
            list_append: declare_runtime(module, "pyx_list_append", &[i64t, i64t], Some(i64t))?,
            tuple_new: declare_runtime(module, "pyx_tuple_new", &[ptr, ptr], Some(i64t))?,
            // `argc` is `uint64_t` in the ABI: pointer-width.
            set_new: declare_runtime(module, "pyx_set_new", &[ptr, ptr], Some(i64t))?,
            set_add: declare_runtime(module, "pyx_set_add", &[i64t, i64t], Some(i64t))?,
            set_from: declare_runtime(module, "pyx_set_from", &[i64t], Some(i64t))?,
            range_new: declare_runtime(module, "pyx_range_new", &[i64t, i64t, i64t], Some(i64t))?,
            list_from: declare_runtime(module, "pyx_list_from", &[i64t], Some(i64t))?,
            dict_from: declare_runtime(module, "pyx_dict_from", &[i64t], Some(i64t))?,
            // `wrapper` is an `i64` function pointer in the ABI; `arity` and
            // `name_len` are `uint64_t`/`size_t`: pointer-width.
            function_new: declare_runtime(
                module,
                "pyx_function_new",
                &[i64t, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // `wrapper` is an `i64`; `arity`, `name_len` and `capture_count`
            // are `size_t` and `captures` is a pointer to tagged values.
            closure_new: declare_runtime(
                module,
                "pyx_closure_new",
                &[i64t, ptr, ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // `name_len` is a `size_t` in the ABI: pointer-width.
            builtin_function: declare_runtime(
                module,
                "pyx_builtin_function",
                &[ptr, ptr],
                Some(i64t),
            )?,
            call: declare_runtime(module, "pyx_call", &[i64t, ptr, ptr], Some(i64t))?,
            map: declare_runtime(module, "pyx_map", &[i64t, i64t], Some(i64t))?,
            filter: declare_runtime(module, "pyx_filter", &[i64t, i64t], Some(i64t))?,
            sum: declare_runtime(module, "pyx_sum", &[i64t, i64t], Some(i64t))?,
            enumerate: declare_runtime(module, "pyx_enumerate", &[i64t, i64t], Some(i64t))?,
            // `argc` is `uint64_t` in the ABI: pointer-width.
            zip: declare_runtime(module, "pyx_zip", &[ptr, ptr], Some(i64t))?,
            // `expected` is a `size_t` in the ABI: pointer-width.
            unpack: declare_runtime(module, "pyx_unpack", &[i64t, ptr], Some(i64t))?,
            dict_new: declare_runtime(module, "pyx_dict_new", &[ptr, ptr, ptr], Some(i64t))?,
            dict_set: declare_runtime(module, "pyx_dict_set", &[i64t, i64t, i64t], Some(i64t))?,
            subscript: declare_runtime(module, "pyx_subscript", &[i64t, i64t], Some(i64t))?,
            subscript_set: declare_runtime(
                module,
                "pyx_subscript_set",
                &[i64t, i64t, i64t],
                Some(i64t),
            )?,
            slice: declare_runtime(module, "pyx_slice", &[i64t, i64t, i64t, i64t], Some(i64t))?,
            // `module_len`, `function_len` and `argc` are `size_t`/`uint64_t`
            // in the ABI: pointer-width.
            module_call: declare_runtime(
                module,
                "pyx_module_call",
                &[ptr, ptr, ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // `module_len`/`name_len` are `size_t` in the ABI: pointer-width.
            module_get: declare_runtime(
                module,
                "pyx_module_get",
                &[ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // `name_len` is a `size_t` in the ABI: pointer-width.
            getattr: declare_runtime(module, "pyx_getattr", &[i64t, ptr, ptr], Some(i64t))?,
            // `name_len` is a `size_t` in the ABI: pointer-width.
            setattr: declare_runtime(module, "pyx_setattr", &[i64t, ptr, ptr, i64t], Some(i64t))?,
            // `name_len` and `method_count` are `size_t`/`usize` in the ABI:
            // pointer-width. `names_ptr` holds tagged `str` values (method
            // names) and `funcs_ptr` the matching function values.
            class_new: declare_runtime(
                module,
                "pyx_class_new",
                &[ptr, ptr, i64t, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // `name_len`/`argc` are `size_t`/`uint64_t` in the ABI: pointer-width.
            method: declare_runtime(
                module,
                "pyx_method",
                &[i64t, ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // `name_len`, `argc` and `kwargc` are `size_t`/`uint64_t` in the
            // ABI: pointer-width. `kw_argv` holds `[name, value]` pairs.
            method_kw: declare_runtime(
                module,
                "pyx_method_kw",
                &[i64t, ptr, ptr, ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            iter_new: declare_runtime(module, "pyx_iter_new", &[i64t], Some(i64t))?,
            iter_next: declare_runtime(module, "pyx_iter_next", &[i64t, ptr], Some(i64t))?,
            set_file: declare_runtime(module, "pyx_set_file", &[ptr, ptr], None)?,
            set_line: declare_runtime(module, "pyx_set_line", &[i32t], None)?,
            frame_enter: declare_runtime(module, "pyx_frame_enter", &[ptr, ptr], None)?,
            // Cooperative GC safepoint: no arguments, no result. AOT resolves
            // it by name; the JIT binds it in `backend::jit::register_runtime_symbols`.
            safepoint: declare_runtime(module, "pyx_safepoint", &[], None)?,
            frame_leave: declare_runtime(module, "pyx_frame_leave", &[], None)?,
            exc_pending: declare_runtime(module, "pyx_exc_pending", &[], Some(i64t))?,
            exc_tag: declare_runtime(module, "pyx_exc_tag", &[], Some(i64t))?,
            exc_value: declare_runtime(module, "pyx_exc_value", &[], Some(i64t))?,
            exc_clear: declare_runtime(module, "pyx_exc_clear", &[], Some(i64t))?,
            raise: declare_runtime(module, "pyx_raise", &[i32t, i64t], Some(i64t))?,
            raise_value: declare_runtime(module, "pyx_raise_value", &[i64t], Some(i64t))?,
            uncaught: declare_runtime(module, "pyx_uncaught", &[], Some(i64t))?,
            runtime_init: declare_runtime(module, "pyx_runtime_init", &[], None)?,
            register_function: declare_runtime(
                module,
                "pyx_register_function",
                &[ptr, ptr, ptr, ptr],
                None,
            )?,
            spawn_child: declare_runtime(module, "pyx_spawn_child", &[], Some(i32t))?,
        })
    }
}

/// Lower the complete program into `module` and return the `pyx_main` symbol.
///
/// All program functions are declared first so that calls (including forward
/// references and recursion) can be emitted while the bodies are lowered.
pub(crate) fn lower_program<M: Module>(
    module: &mut M,
    program: &Program,
) -> Result<FuncId, CodegenError> {
    if program.functions.is_empty() {
        return Err(CodegenError::new(
            "program contains no functions (module body missing)",
        ));
    }
    if program.main as usize >= program.functions.len() {
        let main_id = program.main;
        return Err(CodegenError::new(format!(
            "program main function id {main_id} is out of range"
        )));
    }

    let runtime = Runtime::declare(module)?;
    let ptr_ty = module.target_config().pointer_type();

    let mut symbols: HashMap<FunctionId, FuncId> = HashMap::new();
    for (index, function) in program.functions.iter().enumerate() {
        let signature = make_signature(
            module,
            &vec![types::I64; function.params.len()],
            Some(types::I64),
        );
        let name = format!("__pyx_fn_{index}");
        let id = module
            .declare_function(&name, Linkage::Local, &signature)
            .map_err(|e| module_error(&format!("cannot declare function {name}"), e))?;
        symbols.insert(index as FunctionId, id);
    }

    // One trampoline per program function: `pyx_call` passes a uniform
    // `(argv)` pointer and expects an `i64` result, while the program
    // functions take their arguments individually. The wrappers live in the
    // same module, so both the JIT and the object backend resolve them.
    let mut wrappers: HashMap<FunctionId, FuncId> = HashMap::new();
    for index in 0..program.functions.len() {
        let signature = make_signature(module, &[ptr_ty], Some(types::I64));
        let name = format!("__pyx_wrap_{index}");
        let id = module
            .declare_function(&name, Linkage::Local, &signature)
            .map_err(|e| module_error(&format!("cannot declare wrapper {name}"), e))?;
        wrappers.insert(index as FunctionId, id);
    }

    let pyx_main_sig = make_signature(module, &[], Some(types::I32));
    let pyx_main = module
        .declare_function("pyx_main", Linkage::Export, &pyx_main_sig)
        .map_err(|e| module_error("cannot declare pyx_main", e))?;

    let main_sig = make_signature(module, &[types::I32, ptr_ty], Some(types::I32));
    let c_main = module
        .declare_function("main", Linkage::Export, &main_sig)
        .map_err(|e| module_error("cannot declare main", e))?;

    for (index, function) in program.functions.iter().enumerate() {
        lower_function(
            module,
            program,
            function,
            index as FunctionId,
            &symbols,
            &wrappers,
            &runtime,
        )?;
    }

    for (index, function) in program.functions.iter().enumerate() {
        define_wrapper(
            module,
            index as FunctionId,
            function.params.len(),
            *symbols.get(&(index as FunctionId)).ok_or_else(|| {
                CodegenError::new(format!("internal error: function {index} was not declared"))
            })?,
            *wrappers.get(&(index as FunctionId)).ok_or_else(|| {
                CodegenError::new(format!("internal error: wrapper {index} was not declared"))
            })?,
        )?;
    }

    build_pyx_main(module, program, pyx_main, &symbols, &wrappers, &runtime)?;
    build_c_main(module, c_main, pyx_main)?;

    Ok(pyx_main)
}

fn lower_function<M: Module>(
    module: &mut M,
    program: &Program,
    function: &Function,
    index: FunctionId,
    symbols: &HashMap<FunctionId, FuncId>,
    wrappers: &HashMap<FunctionId, FuncId>,
    runtime: &Runtime,
) -> Result<(), CodegenError> {
    let func_id = *symbols.get(&index).ok_or_else(|| {
        CodegenError::new(format!("internal error: function {index} was not declared"))
    })?;

    let mut ctx = module.make_context();
    let signature = make_signature(
        module,
        &vec![types::I64; function.params.len()],
        Some(types::I64),
    );
    ctx.func.signature = signature;

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let ptr_ty = module.target_config().pointer_type();
        let mut lower = Lower::new(
            module, program, function, symbols, wrappers, runtime, builder, ptr_ty,
        );
        lower.run()?;
        lower.finish();
    }

    module
        .define_function(func_id, &mut ctx)
        .map_err(|e| module_error(&format!("cannot define function {index}"), e))
}

/// Define the `(argv: ptr) -> i64` trampoline for program function `index`.
///
/// `pyx_call` invokes function values with a uniform array of tagged values;
/// the wrapper loads the arguments from `argv` and forwards them to the
/// concrete `__pyx_fn_N` symbol.
fn define_wrapper<M: Module>(
    module: &mut M,
    index: FunctionId,
    arity: usize,
    target: FuncId,
    wrapper: FuncId,
) -> Result<(), CodegenError> {
    let ptr_ty = module.target_config().pointer_type();
    let mut ctx = module.make_context();
    ctx.func.signature = make_signature(module, &[ptr_ty], Some(types::I64));

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);

        let argv = *builder.block_params(entry).first().ok_or_else(|| {
            CodegenError::new(format!(
                "internal error: wrapper {index} has no argv parameter"
            ))
        })?;
        let mut args = Vec::with_capacity(arity);
        for position in 0..arity {
            let offset = position
                .checked_mul(8)
                .and_then(|offset| i32::try_from(offset).ok())
                .ok_or_else(|| {
                    CodegenError::new(format!(
                        "internal error: wrapper {index} argument offset out of range"
                    ))
                })?;
            let value = builder
                .ins()
                .load(types::I64, MemFlags::new(), argv, offset);
            args.push(value);
        }

        let reference = declare_ref(module, &mut builder, target);
        let call = builder.ins().call(reference, &args);
        let result = builder.inst_results(call).first().copied().ok_or_else(|| {
            CodegenError::new(format!(
                "internal error: wrapper {index} target returned no value"
            ))
        })?;
        builder.ins().return_(&[result]);

        builder.seal_all_blocks();
        builder.finalize();
    }

    module
        .define_function(wrapper, &mut ctx)
        .map_err(|e| module_error(&format!("cannot define wrapper {index}"), e))
}

/// Loop targets for `break`/`continue`.
#[derive(Clone, Copy)]
struct LoopTargets {
    break_block: Block,
    continue_block: Block,
}

/// The payload a comprehension emits once all clauses have matched.
#[derive(Clone, Copy)]
enum CompEmit<'a> {
    List(&'a Expr),
    Set(&'a Expr),
    Dict(&'a Expr, &'a Expr),
}

/// Identifier for an active `finally` block.
type CleanupId = usize;

/// Where an in-flight exception is routed.
#[derive(Clone, Copy)]
enum ExcTarget {
    /// Jump into the handler dispatch block of an enclosing `try`.
    Handler(Block),
    /// Run the `finally` block of the cleanup, which then re-propagates.
    Cleanup(CleanupId),
    /// Leave the function through the epilogue.
    Function,
}

/// State of one active `finally` block.
#[derive(Clone, Copy)]
struct CleanupInfo {
    /// Entry block of the `finally` body.
    finally_entry: Block,
    /// Action selector (`0` normal, `1` return, `2` break, `3` continue,
    /// `4` exception).
    action_var: Variable,
    /// Index in `Lower::loops` of the loop enclosing the `try`, if any.
    break_loop: Option<usize>,
    /// Where exceptions raised by the `finally` body itself propagate.
    outer_exc: ExcTarget,
    /// Slot holding the exception object in flight when the cleanup is
    /// entered with action `4`.
    pending_slot: StackSlot,
}

struct Lower<'a, 'b, M: Module> {
    module: &'a mut M,
    program: &'a Program,
    locals: &'a [LocalInfo],
    params: &'a [Param],
    body: &'a [Stmt],
    name: &'a str,
    symbols: &'a HashMap<FunctionId, FuncId>,
    wrappers: &'a HashMap<FunctionId, FuncId>,
    runtime: &'a Runtime,
    builder: FunctionBuilder<'b>,
    ptr_ty: ir::Type,
    epilogue: Option<Block>,
    ret_var: Variable,
    /// Per-function counter for loop safepoint polling. Generated loops call
    /// the runtime only every 64th iteration instead of on every backedge.
    safepoint_counter: Variable,
    exc_target: ExcTarget,
    /// Exception object handled by the enclosing `except` arm, for bare `raise`.
    current_exc: Option<Variable>,
    loops: Vec<LoopTargets>,
    /// Active `finally` blocks, innermost last.
    cleanups: Vec<CleanupId>,
    /// Append-only table of cleanup metadata (ids stay stable).
    cleanup_infos: Vec<CleanupInfo>,
    /// Pending-exception slots of the `finally` bodies currently being
    /// lowered, innermost last. A bare `raise` inside a `finally` re-raises
    /// the exception parked here (CPython semantics).
    finally_slots: Vec<StackSlot>,
    /// Number of enclosing `finally` bodies currently being lowered; pending
    /// exceptions must be cleared before transferring control out of them.
    suppress_pending: usize,
    next_temp: u32,
    terminated: bool,
    func_refs: HashMap<FuncId, FuncRef>,
}

impl<'a, 'b, M: Module> Lower<'a, 'b, M> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        module: &'a mut M,
        program: &'a Program,
        function: &'a Function,
        symbols: &'a HashMap<FunctionId, FuncId>,
        wrappers: &'a HashMap<FunctionId, FuncId>,
        runtime: &'a Runtime,
        builder: FunctionBuilder<'b>,
        ptr_ty: ir::Type,
    ) -> Lower<'a, 'b, M> {
        let ret_var = Variable::from_u32(function.locals.len() as u32);
        let safepoint_counter = Variable::from_u32(function.locals.len() as u32 + 1);
        Lower {
            module,
            program,
            locals: &function.locals,
            params: &function.params,
            body: &function.body,
            name: &function.name,
            symbols,
            wrappers,
            runtime,
            builder,
            ptr_ty,
            epilogue: None,
            ret_var,
            safepoint_counter,
            exc_target: ExcTarget::Function,
            current_exc: None,
            loops: Vec::new(),
            cleanups: Vec::new(),
            cleanup_infos: Vec::new(),
            finally_slots: Vec::new(),
            suppress_pending: 0,
            next_temp: function.locals.len() as u32 + 2,
            terminated: false,
            func_refs: HashMap::new(),
        }
    }

    fn run(&mut self) -> Result<(), CodegenError> {
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);

        let epilogue = self.builder.create_block();
        self.epilogue = Some(epilogue);

        // All locals start out as None; parameters overwrite their slots.
        self.builder.declare_var(self.ret_var, types::I64);
        self.builder.declare_var(self.safepoint_counter, types::I64);
        for index in 0..self.locals.len() {
            self.builder
                .declare_var(Variable::from_u32(index as u32), types::I64);
        }
        let none = self.none();
        for index in 0..self.locals.len() {
            self.builder.def_var(Variable::from_u32(index as u32), none);
        }
        self.builder.def_var(self.ret_var, none);
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder.def_var(self.safepoint_counter, zero);

        let entry_params = self.builder.block_params(entry).to_vec();
        for (position, param) in self.params.iter().enumerate() {
            let var = self.local_var(param.local)?;
            let function_name = self.name;
            let value = *entry_params.get(position).ok_or_else(|| {
                CodegenError::new(format!(
                    "parameter {position} of function {function_name} has no Cranelift parameter"
                ))
            })?;
            self.builder.def_var(var, value);
        }

        self.emit_frame_enter()?;
        // Every function (including the module body) polls once on entry so a
        // thread that just started running can park before touching the heap.
        self.emit_safepoint();
        let body = self.body;
        self.lower_stmts(body)?;

        if !self.terminated {
            self.builder.ins().jump(epilogue, &[]);
        }
        self.builder.switch_to_block(epilogue);
        self.terminated = false;
        let id = self.runtime.frame_leave;
        self.call_void(id, &[]);
        let result = self.builder.use_var(self.ret_var);
        self.builder.ins().return_(&[result]);
        self.terminated = true;
        Ok(())
    }

    fn finish(self) {
        let mut builder = self.builder;
        builder.seal_all_blocks();
        builder.finalize();
    }

    fn none(&mut self) -> Value {
        self.builder.ins().iconst(types::I64, NONE_TAG)
    }

    fn tagged_int(&mut self, value: i64) -> Value {
        self.builder.ins().iconst(types::I64, value)
    }

    fn local_var(&self, id: LocalId) -> Result<Variable, CodegenError> {
        if id as usize >= self.locals.len() {
            let function_name = self.name;
            return Err(CodegenError::new(format!(
                "local id {id} out of range for function {function_name}"
            )));
        }
        Ok(Variable::from_u32(id))
    }

    fn temp_var(&mut self) -> Variable {
        let var = Variable::from_u32(self.next_temp);
        self.next_temp += 1;
        self.builder.declare_var(var, types::I64);
        var
    }

    #[allow(clippy::explicit_auto_deref, clippy::needless_borrow)]
    fn func_ref(&mut self, id: FuncId) -> FuncRef {
        if let Some(reference) = self.func_refs.get(&id) {
            return *reference;
        }
        let reference = self
            .module
            .declare_func_in_func(id, &mut *self.builder.func);
        self.func_refs.insert(id, reference);
        reference
    }

    fn call_rt(&mut self, id: FuncId, args: &[Value]) -> Result<Value, CodegenError> {
        let reference = self.func_ref(id);
        let inst = self.builder.ins().call(reference, args);
        self.builder
            .inst_results(inst)
            .first()
            .copied()
            .ok_or_else(|| CodegenError::new("internal error: runtime call returned no value"))
    }

    fn call_void(&mut self, id: FuncId, args: &[Value]) {
        let reference = self.func_ref(id);
        self.builder.ins().call(reference, args);
    }

    /// Emit a cooperative GC safepoint poll.
    ///
    /// This is a no-op when the current block is already terminated (the
    /// builder would otherwise reject instructions after a terminator). The
    /// call takes and returns nothing, so it never changes values, locals or
    /// the pending-exception state. Call sites must only use it at points
    /// where no runtime lock is held and no runtime call is in flight.
    fn emit_safepoint(&mut self) {
        if self.terminated {
            return;
        }
        let id = self.runtime.safepoint;
        self.call_void(id, &[]);
    }

    /// Polls the collector at a loop backedge, but only every 64th iteration.
    ///
    /// A thread that is stopped in a loop parks within at most 64 iterations;
    /// each iteration is bounded user code (allocations and blocking calls
    /// still poll on their own). This removes the dominant call/load overhead
    /// from hot integer loops while keeping collection latency bounded.
    fn emit_loop_safepoint(&mut self) {
        if self.terminated {
            return;
        }
        let counter = self.builder.use_var(self.safepoint_counter);
        let next = self.builder.ins().iadd_imm(counter, 1);
        self.builder.def_var(self.safepoint_counter, next);
        let masked = self.builder.ins().band_imm(next, 63);
        let not_due = self.builder.ins().icmp_imm(IntCC::NotEqual, masked, 0);
        let poll = self.builder.create_block();
        let after = self.builder.create_block();
        self.builder.ins().brif(not_due, after, &[], poll, &[]);
        self.switch_to(poll);
        self.emit_safepoint();
        if !self.terminated {
            self.builder.ins().jump(after, &[]);
        }
        self.switch_to(after);
    }

    fn switch_to(&mut self, block: Block) {
        self.builder.switch_to_block(block);
        self.terminated = false;
    }

    fn jump(&mut self, block: Block) {
        self.builder.ins().jump(block, &[]);
        self.terminated = true;
    }

    fn cleanup_info(&self, id: CleanupId) -> Result<CleanupInfo, CodegenError> {
        self.cleanup_infos
            .get(id)
            .copied()
            .ok_or_else(|| CodegenError::new(format!("internal error: unknown cleanup id {id}")))
    }

    /// Leave the current block for `target`.
    ///
    /// Cleanup targets park the pending exception in the cleanup's slot,
    /// clear it, select action `4` and enter the `finally` body; the action
    /// dispatch re-raises it afterwards. Clearing it first keeps runtime
    /// calls inside `finally` from observing the exception that is already
    /// being handled.
    fn emit_jump_exc_target(&mut self, target: ExcTarget) -> Result<(), CodegenError> {
        match target {
            ExcTarget::Function => {
                let epilogue = self.epilogue.ok_or_else(|| {
                    CodegenError::new(
                        "internal error: exception target before epilogue was created",
                    )
                })?;
                self.jump(epilogue);
            }
            ExcTarget::Handler(block) => self.jump(block),
            ExcTarget::Cleanup(id) => {
                let info = self.cleanup_info(id)?;
                let value_id = self.runtime.exc_value;
                let value = self.call_rt(value_id, &[])?;
                self.builder.ins().stack_store(value, info.pending_slot, 0);
                let clear_id = self.runtime.exc_clear;
                self.call_rt(clear_id, &[])?;
                let action = self.tagged_int(4);
                self.builder.def_var(info.action_var, action);
                self.jump(info.finally_entry);
            }
        }
        Ok(())
    }

    fn emit_exc_check(&mut self) -> Result<(), CodegenError> {
        if self.terminated {
            return Ok(());
        }
        let id = self.runtime.exc_pending;
        let pending = self.call_rt(id, &[])?;
        let has_exception = self.builder.ins().icmp_imm(IntCC::NotEqual, pending, 0);
        let continuation = self.builder.create_block();
        let exceptional = self.builder.create_block();
        self.builder
            .ins()
            .brif(has_exception, exceptional, &[], continuation, &[]);
        self.switch_to(exceptional);
        let target = self.exc_target;
        self.emit_jump_exc_target(target)?;
        self.switch_to(continuation);
        Ok(())
    }

    /// Clear the pending exception before transferring control out of a
    /// `finally` body (it suppresses an exception that was in flight).
    fn clear_pending(&mut self) -> Result<(), CodegenError> {
        if !self.terminated {
            let id = self.runtime.exc_clear;
            self.call_rt(id, &[])?;
        }
        Ok(())
    }

    /// Mark a cleanup as entered without an in-flight exception.
    ///
    /// The slot is a per-cleanup parking spot for the exception that forced
    /// the `finally` to run. Normal completion, `return`, `break` and
    /// `continue` all enter the `finally` without one, so the slot is reset
    /// to tagged `None`; only an exception entry parks a value via
    /// `pyx_exc_value`. Bare `raise` inside the `finally` body therefore
    /// re-raises exactly the in-flight exception, and raises `RuntimeError`
    /// when there is none (matching CPython).
    fn reset_finally_slot(&mut self, slot: StackSlot) {
        let none = self.none();
        self.builder.ins().stack_store(none, slot, 0);
    }

    /// Transfer control out of the current function, running every active
    /// `finally` on the way.
    fn emit_return(&mut self) -> Result<(), CodegenError> {
        if self.suppress_pending > 0 {
            self.clear_pending()?;
        }
        if let Some(id) = self.cleanups.last().copied() {
            let info = self.cleanup_info(id)?;
            // Entering for `return` without an in-flight exception.
            self.reset_finally_slot(info.pending_slot);
            let action = self.tagged_int(1);
            self.builder.def_var(info.action_var, action);
            self.jump(info.finally_entry);
        } else {
            let epilogue = self.epilogue.ok_or_else(|| {
                CodegenError::new("internal error: return before epilogue was created")
            })?;
            self.jump(epilogue);
        }
        Ok(())
    }

    /// Jump to the innermost loop's `break` block, running the `finally`
    /// blocks that enclose both the `break` and its target loop.
    fn emit_break(&mut self) -> Result<(), CodegenError> {
        let index = self
            .loops
            .len()
            .checked_sub(1)
            .ok_or_else(|| CodegenError::new("`break` outside of a loop"))?;
        let target = self.loops[index];
        if self.suppress_pending > 0 {
            self.clear_pending()?;
        }
        if let Some(id) = self.cleanups.last().copied() {
            let info = self.cleanup_info(id)?;
            if info.break_loop == Some(index) {
                self.reset_finally_slot(info.pending_slot);
                let action = self.tagged_int(2);
                self.builder.def_var(info.action_var, action);
                self.jump(info.finally_entry);
                return Ok(());
            }
        }
        self.jump(target.break_block);
        Ok(())
    }

    /// Jump to the innermost loop's `continue` block, running the `finally`
    /// blocks that enclose both the `continue` and its target loop.
    fn emit_continue(&mut self) -> Result<(), CodegenError> {
        let index = self
            .loops
            .len()
            .checked_sub(1)
            .ok_or_else(|| CodegenError::new("`continue` outside of a loop"))?;
        let target = self.loops[index];
        if self.suppress_pending > 0 {
            self.clear_pending()?;
        }
        if let Some(id) = self.cleanups.last().copied() {
            let info = self.cleanup_info(id)?;
            if info.break_loop == Some(index) {
                self.reset_finally_slot(info.pending_slot);
                let action = self.tagged_int(3);
                self.builder.def_var(info.action_var, action);
                self.jump(info.finally_entry);
                return Ok(());
            }
        }
        self.jump(target.continue_block);
        Ok(())
    }

    /// Finish a normally completed region: run the pending `finally` (if any)
    /// and continue at `after`.
    fn emit_finally_normal(
        &mut self,
        cleanup: Option<CleanupId>,
        after: Block,
    ) -> Result<(), CodegenError> {
        match cleanup {
            Some(id) => {
                let info = self.cleanup_info(id)?;
                // Normal completion enters the `finally` without an
                // exception in flight.
                self.reset_finally_slot(info.pending_slot);
                let action = self.tagged_int(0);
                self.builder.def_var(info.action_var, action);
                self.jump(info.finally_entry);
            }
            None => self.jump(after),
        }
        Ok(())
    }

    /// Dispatch the action selected in a `finally` block's action variable.
    fn emit_action_dispatch(
        &mut self,
        cleanup: CleanupId,
        after: Block,
    ) -> Result<(), CodegenError> {
        let info = self.cleanup_info(cleanup)?;
        let action = self.builder.use_var(info.action_var);

        let normal = self.builder.create_block();
        let check_return = self.builder.create_block();
        let check_break = self.builder.create_block();
        let check_continue = self.builder.create_block();
        let exceptional = self.builder.create_block();

        let is_zero = self.builder.ins().icmp_imm(IntCC::Equal, action, 0);
        self.builder
            .ins()
            .brif(is_zero, normal, &[], check_return, &[]);

        self.switch_to(check_return);
        let is_return = self.builder.ins().icmp_imm(IntCC::Equal, action, 1);
        let propagate_return = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_return, propagate_return, &[], check_break, &[]);

        self.switch_to(check_break);
        let is_break = self.builder.ins().icmp_imm(IntCC::Equal, action, 2);
        let propagate_break = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_break, propagate_break, &[], check_continue, &[]);

        self.switch_to(check_continue);
        let is_continue = self.builder.ins().icmp_imm(IntCC::Equal, action, 3);
        let propagate_continue = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_continue, propagate_continue, &[], exceptional, &[]);

        // Normal completion of the `finally` body.
        self.switch_to(normal);
        self.jump(after);

        // `return` propagates through the remaining cleanups.
        self.switch_to(propagate_return);
        if let Some(outer_id) = self.cleanups.last().copied() {
            let outer = self.cleanup_info(outer_id)?;
            // An outer cleanup entered for `return` has no exception in
            // flight.
            self.reset_finally_slot(outer.pending_slot);
            let action = self.tagged_int(1);
            self.builder.def_var(outer.action_var, action);
            self.jump(outer.finally_entry);
        } else {
            let epilogue = self.epilogue.ok_or_else(|| {
                CodegenError::new("internal error: return before epilogue was created")
            })?;
            self.jump(epilogue);
        }

        // `break`/`continue` only propagate through a cleanup that belongs to
        // the same loop; otherwise the loop itself handles them.
        self.switch_to(propagate_break);
        self.propagate_loop_action(&info, 2, true)?;

        self.switch_to(propagate_continue);
        self.propagate_loop_action(&info, 3, false)?;

        // An exception (or an unknown action) re-propagates to the enclosing
        // exception target, re-raising the parked exception object first.
        self.switch_to(exceptional);
        let value = self
            .builder
            .ins()
            .stack_load(types::I64, info.pending_slot, 0);
        let raise_id = self.runtime.raise_value;
        self.call_rt(raise_id, &[value])?;
        let target = info.outer_exc;
        self.emit_jump_exc_target(target)?;
        Ok(())
    }

    /// Emit the transfer for a `break` (`code == 2`) or `continue` (`code
    /// == 3`) that finished a `finally` body.
    fn propagate_loop_action(
        &mut self,
        info: &CleanupInfo,
        code: i64,
        is_break: bool,
    ) -> Result<(), CodegenError> {
        let Some(loop_index) = info.break_loop else {
            // A loop action can only be selected for a cleanup that recorded
            // an enclosing loop, so this block is unreachable; keep it
            // well-formed by leaving the function.
            let epilogue = self.epilogue.ok_or_else(|| {
                CodegenError::new("internal error: loop action before epilogue was created")
            })?;
            self.jump(epilogue);
            return Ok(());
        };
        if let Some(outer_id) = self.cleanups.last().copied() {
            let outer = self.cleanup_info(outer_id)?;
            if outer.break_loop == Some(loop_index) {
                // An outer cleanup entered for `break`/`continue` has no
                // exception in flight.
                self.reset_finally_slot(outer.pending_slot);
                let action = self.tagged_int(code);
                self.builder.def_var(outer.action_var, action);
                self.jump(outer.finally_entry);
                return Ok(());
            }
        }
        let target =
            self.loops.get(loop_index).copied().ok_or_else(|| {
                CodegenError::new("internal error: loop target is no longer active")
            })?;
        if is_break {
            self.jump(target.break_block);
        } else {
            self.jump(target.continue_block);
        }
        Ok(())
    }

    fn set_line(&mut self, line: u32) -> Result<(), CodegenError> {
        let value = self.builder.ins().iconst(types::I32, line as i64);
        self.call_void(self.runtime.set_line, &[value]);
        Ok(())
    }

    fn emit_frame_enter(&mut self) -> Result<(), CodegenError> {
        let (slot, len) = make_bytes_slot(&mut self.builder, self.name.as_bytes())?;
        let ptr = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let len_value = self.builder.ins().iconst(self.ptr_ty, len as i64);
        self.call_void(self.runtime.frame_enter, &[ptr, len_value]);
        Ok(())
    }

    fn str_const(&mut self, text: &str) -> Result<Value, CodegenError> {
        let (slot, len) = make_bytes_slot(&mut self.builder, text.as_bytes())?;
        let ptr = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let len_value = self.builder.ins().iconst(self.ptr_ty, len as i64);
        let id = self.runtime.str_new;
        self.call_rt(id, &[ptr, len_value])
    }

    fn bytes_const(&mut self, bytes: &[u8]) -> Result<Value, CodegenError> {
        let (slot, len) = make_bytes_slot(&mut self.builder, bytes)?;
        let ptr = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let len_value = self.builder.ins().iconst(self.ptr_ty, len as i64);
        let id = self.runtime.bytes_new;
        self.call_rt(id, &[ptr, len_value])
    }

    fn truthy_cond(&mut self, value: Value) -> Result<Value, CodegenError> {
        let id = self.runtime.truthy;
        let truth = self.call_rt(id, &[value])?;
        Ok(self.builder.ins().icmp_imm(IntCC::NotEqual, truth, 0))
    }

    fn lower_stmts(&mut self, body: &'a [Stmt]) -> Result<(), CodegenError> {
        for stmt in body {
            self.lower_stmt(stmt)?;
        }
        Ok(())
    }

    fn lower_stmt(&mut self, stmt: &'a Stmt) -> Result<(), CodegenError> {
        if self.terminated {
            return Ok(());
        }
        self.set_line(stmt.line)?;
        match &stmt.kind {
            StmtKind::Expr(expr) => {
                self.lower_expr(expr)?;
            }
            StmtKind::Assign { target, value } => {
                let var = self.local_var(*target)?;
                let value = self.lower_expr(value)?;
                self.builder.def_var(var, value);
            }
            StmtKind::AugAssign { target, op, value } => {
                let var = self.local_var(*target)?;
                let lhs = self.builder.use_var(var);
                let rhs = self.lower_expr(value)?;
                let result = self.lower_binop(*op, lhs, rhs)?;
                self.builder.def_var(var, result);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                let condition = self.lower_expr(cond)?;
                let branch = self.truthy_cond(condition)?;
                let then_block = self.builder.create_block();
                let else_block = self.builder.create_block();
                let after = self.builder.create_block();
                self.builder
                    .ins()
                    .brif(branch, then_block, &[], else_block, &[]);
                self.switch_to(then_block);
                self.lower_stmts(then_body.as_slice())?;
                if !self.terminated {
                    self.jump(after);
                }
                self.switch_to(else_block);
                self.lower_stmts(else_body.as_slice())?;
                if !self.terminated {
                    self.jump(after);
                }
                self.switch_to(after);
            }
            StmtKind::While { cond, body } => {
                let condition_block = self.builder.create_block();
                let body_block = self.builder.create_block();
                let after = self.builder.create_block();
                self.jump(condition_block);
                self.switch_to(condition_block);
                // Backedge landing pad: poll every 64th iteration before
                // re-checking the condition (or on first entry).
                self.emit_loop_safepoint();
                let condition = self.lower_expr(cond)?;
                let branch = self.truthy_cond(condition)?;
                self.builder.ins().brif(branch, body_block, &[], after, &[]);
                self.loops.push(LoopTargets {
                    break_block: after,
                    continue_block: condition_block,
                });
                self.switch_to(body_block);
                self.lower_stmts(body.as_slice())?;
                if !self.terminated {
                    self.jump(condition_block);
                }
                self.loops.pop();
                self.switch_to(after);
            }
            StmtKind::ForRange {
                var,
                start,
                stop,
                step,
                body,
            } => {
                self.lower_for_range(*var, start, stop, step.as_ref(), body.as_slice())?;
            }
            StmtKind::ForEach {
                var,
                iterable,
                body,
            } => {
                self.lower_for_each(*var, iterable, body.as_slice())?;
            }
            StmtKind::SetSubscript {
                object,
                index,
                value,
            } => {
                // Python evaluates the right-hand side first, then the object,
                // then the index.
                let value = self.lower_expr(value)?;
                let object = self.lower_expr(object)?;
                let index = self.lower_expr(index)?;
                let id = self.runtime.subscript_set;
                let _ = self.call_rt(id, &[object, index, value])?;
                self.emit_exc_check()?;
            }
            StmtKind::Closure {
                target,
                function,
                captures,
            } => {
                let func = self
                    .program
                    .functions
                    .get(*function as usize)
                    .ok_or_else(|| {
                        CodegenError::new(format!(
                            "closure refers to unknown function id {function}"
                        ))
                    })?;
                if func.capture_count != captures.len() {
                    return Err(CodegenError::new(format!(
                        "internal error: closure {} captures {} values but declares {}",
                        func.name,
                        captures.len(),
                        func.capture_count
                    )));
                }
                let user_arity = func
                    .params
                    .len()
                    .checked_sub(func.capture_count)
                    .ok_or_else(|| {
                        CodegenError::new(format!(
                            "internal error: function '{}' has more captures than parameters",
                            func.name
                        ))
                    })?;
                let name = func.name.clone();
                let wrapper = *self.wrappers.get(function).ok_or_else(|| {
                    CodegenError::new(format!(
                        "closure refers to undeclared wrapper for function id {function}"
                    ))
                })?;

                // Capture values are read at the definition site; only the
                // tagged values are copied into the function object.
                let captures_slot = self.stack_array_slot(captures.len(), "closure captures")?;
                for (position, capture) in captures.iter().enumerate() {
                    let var = self.local_var(*capture)?;
                    let value = self.builder.use_var(var);
                    self.builder
                        .ins()
                        .stack_store(value, captures_slot, (position * 8) as i32);
                }

                let reference = self.func_ref(wrapper);
                let wrapper_address = self.builder.ins().func_addr(self.ptr_ty, reference);
                let wrapper_address = self.pointer_to_i64(wrapper_address);
                let arity = self.builder.ins().iconst(self.ptr_ty, user_arity as i64);
                let (name_slot, name_len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
                let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
                let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
                let capture_count = self
                    .builder
                    .ins()
                    .iconst(self.ptr_ty, captures.len() as i64);
                let captures_ptr = self.builder.ins().stack_addr(self.ptr_ty, captures_slot, 0);
                let id = self.runtime.closure_new;
                let closure = self.call_rt(
                    id,
                    &[
                        wrapper_address,
                        arity,
                        name_ptr,
                        name_len,
                        capture_count,
                        captures_ptr,
                    ],
                )?;
                self.emit_exc_check()?;
                let var = self.local_var(*target)?;
                self.builder.def_var(var, closure);
            }
            StmtKind::ClassDef {
                target,
                name,
                base,
                methods,
            } => {
                // Python evaluates the base expression first, then creates the
                // method functions while executing the class body.
                let base = match base {
                    Some(base) => self.lower_expr(base)?,
                    None => self.none(),
                };
                // Kept in a variable: function creation may branch into a new
                // block on a pending exception.
                let base_holder = self.temp_var();
                self.builder.def_var(base_holder, base);

                let names_slot = self.stack_array_slot(methods.len(), "class method names")?;
                let funcs_slot = self.stack_array_slot(methods.len(), "class method functions")?;
                for (index, method) in methods.iter().enumerate() {
                    let (name_slot, name_len) =
                        make_bytes_slot(&mut self.builder, method.name.as_bytes())?;
                    let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
                    let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
                    let id = self.runtime.str_new;
                    let method_name = self.call_rt(id, &[name_ptr, name_len])?;
                    self.emit_exc_check()?;
                    self.builder
                        .ins()
                        .stack_store(method_name, names_slot, (index * 8) as i32);

                    let function = self.lower_function_ref(method.function)?;
                    self.builder
                        .ins()
                        .stack_store(function, funcs_slot, (index * 8) as i32);
                }

                let (class_slot, class_len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
                let class_ptr = self.builder.ins().stack_addr(self.ptr_ty, class_slot, 0);
                let class_len = self.builder.ins().iconst(self.ptr_ty, class_len as i64);
                let count = self.builder.ins().iconst(self.ptr_ty, methods.len() as i64);
                let names_ptr = self.builder.ins().stack_addr(self.ptr_ty, names_slot, 0);
                let funcs_ptr = self.builder.ins().stack_addr(self.ptr_ty, funcs_slot, 0);
                let base = self.builder.use_var(base_holder);
                let id = self.runtime.class_new;
                let class = self.call_rt(
                    id,
                    &[class_ptr, class_len, base, count, names_ptr, funcs_ptr],
                )?;
                self.emit_exc_check()?;
                let var = self.local_var(*target)?;
                self.builder.def_var(var, class);
            }
            StmtKind::SetAttr {
                object,
                name,
                value,
            } => {
                // Python evaluates the right-hand side first, then the target
                // object. Both intermediate values must survive the calls that
                // may raise, so they are parked in variables.
                let value = self.lower_expr(value)?;
                let value_holder = self.temp_var();
                self.builder.def_var(value_holder, value);
                let object = self.lower_expr(object)?;
                let object_holder = self.temp_var();
                self.builder.def_var(object_holder, object);

                let (name_slot, name_len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
                let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
                let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
                let object = self.builder.use_var(object_holder);
                let value = self.builder.use_var(value_holder);
                let id = self.runtime.setattr;
                let _ = self.call_rt(id, &[object, name_ptr, name_len, value])?;
                self.emit_exc_check()?;
            }
            StmtKind::Unpack { targets, value } => {
                // Evaluate the right-hand side once and park it in a variable
                // so that it stays available while the targets are read out.
                let items = self.lower_expr(value)?;
                let holder = self.temp_var();
                self.builder.def_var(holder, items);
                let expected = self.builder.ins().iconst(self.ptr_ty, targets.len() as i64);
                let id = self.runtime.unpack;
                let _ = self.call_rt(id, &[items, expected])?;
                self.emit_exc_check()?;
                for (index, target) in targets.iter().enumerate() {
                    let items = self.builder.use_var(holder);
                    let index = self.tagged_int((index as i64).wrapping_shl(3));
                    let id = self.runtime.subscript;
                    let item = self.call_rt(id, &[items, index])?;
                    self.emit_exc_check()?;
                    let var = self.local_var(*target)?;
                    self.builder.def_var(var, item);
                }
            }
            StmtKind::Return(value) => {
                let value = match value {
                    Some(expr) => self.lower_expr(expr)?,
                    None => self.none(),
                };
                self.builder.def_var(self.ret_var, value);
                self.emit_return()?;
            }
            StmtKind::Break => {
                self.emit_break()?;
            }
            StmtKind::Continue => {
                self.emit_continue()?;
            }
            StmtKind::Pass => {}
            StmtKind::Raise { exc, msg } => {
                self.lower_raise(*exc, msg.as_ref())?;
            }
            StmtKind::Try {
                body,
                handlers,
                else_body,
                finally_body,
            } => {
                self.lower_try(
                    body.as_slice(),
                    handlers.as_slice(),
                    else_body.as_slice(),
                    finally_body.as_slice(),
                )?;
            }
        }
        Ok(())
    }

    fn lower_for_range(
        &mut self,
        var: LocalId,
        start: &'a Expr,
        stop: &'a Expr,
        step: Option<&'a Expr>,
        body: &'a [Stmt],
    ) -> Result<(), CodegenError> {
        let local = self.local_var(var)?;
        let statically_zero = match step {
            Some(expr) => matches!(
                &expr.kind,
                ExprKind::Const(Const::Int(0)) | ExprKind::Const(Const::Bool(false))
            ),
            None => false,
        };

        let start_value = self.lower_expr(start)?;
        let stop_value = self.lower_expr(stop)?;
        let step_value = match step {
            Some(expr) => self.lower_expr(expr)?,
            None => self.tagged_int(8),
        };

        if statically_zero {
            let message = self.str_const("range() arg 3 must not be zero")?;
            let tag = self
                .builder
                .ins()
                .iconst(types::I32, ExcKind::ValueError.tag() as i64);
            let id = self.runtime.raise;
            self.call_rt(id, &[tag, message])?;
            let target = self.exc_target;
            self.emit_jump_exc_target(target)?;
            return Ok(());
        }

        // `range()` rejects a zero step before entering the loop.
        let is_zero = self.builder.ins().icmp_imm(IntCC::Equal, step_value, 0);
        let zero_block = self.builder.create_block();
        let ready_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_zero, zero_block, &[], ready_block, &[]);
        self.switch_to(zero_block);
        let message = self.str_const("range() arg 3 must not be zero")?;
        let tag = self
            .builder
            .ins()
            .iconst(types::I32, ExcKind::ValueError.tag() as i64);
        let id = self.runtime.raise;
        self.call_rt(id, &[tag, message])?;
        let target = self.exc_target;
        self.emit_jump_exc_target(target)?;
        self.switch_to(ready_block);

        let start_var = self.temp_var();
        let stop_var = self.temp_var();
        let step_var = self.temp_var();
        self.builder.def_var(start_var, start_value);
        self.builder.def_var(stop_var, stop_value);
        self.builder.def_var(step_var, step_value);
        self.builder.def_var(local, start_value);

        let condition = self.builder.create_block();
        let positive = self.builder.create_block();
        let negative = self.builder.create_block();
        let body_block = self.builder.create_block();
        let increment = self.builder.create_block();
        let after = self.builder.create_block();

        self.jump(condition);
        self.switch_to(condition);

        // Choose the comparison direction based on the sign of the step.
        let step_current = self.builder.use_var(step_var);
        let zero = self.tagged_int(0);
        let positive_step = self.lower_binop(BinOp::Gt, step_current, zero)?;
        let direction = self.truthy_cond(positive_step)?;
        self.builder
            .ins()
            .brif(direction, positive, &[], negative, &[]);

        self.switch_to(positive);
        let current = self.builder.use_var(local);
        let limit = self.builder.use_var(stop_var);
        let comparison = self.lower_binop(BinOp::Lt, current, limit)?;
        let branch = self.truthy_cond(comparison)?;
        self.builder.ins().brif(branch, body_block, &[], after, &[]);

        self.switch_to(negative);
        let current = self.builder.use_var(local);
        let limit = self.builder.use_var(stop_var);
        let comparison = self.lower_binop(BinOp::Gt, current, limit)?;
        let branch = self.truthy_cond(comparison)?;
        self.builder.ins().brif(branch, body_block, &[], after, &[]);

        self.loops.push(LoopTargets {
            break_block: after,
            // `continue` must still advance the loop variable.
            continue_block: increment,
        });
        self.switch_to(body_block);
        self.lower_stmts(body)?;
        if !self.terminated {
            self.jump(increment);
        }

        self.switch_to(increment);
        // Loop backedge (also the `continue` target): poll before advancing.
        self.emit_loop_safepoint();
        let current = self.builder.use_var(local);
        let step_current = self.builder.use_var(step_var);
        let next = self.lower_binop(BinOp::Add, current, step_current)?;
        self.builder.def_var(local, next);
        self.jump(condition);
        self.loops.pop();

        self.switch_to(after);
        Ok(())
    }

    /// Lower `for var in iterable` over a runtime iterable (list, str, dict).
    ///
    /// The iterator and the 8-byte output slot live in a block that dominates
    /// the loop; the iteration block calls `pyx_iter_next` once per item and a
    /// zero result terminates the loop. `continue` re-enters the iteration
    /// block, so the next item is fetched without running the loop tail again.
    fn lower_for_each(
        &mut self,
        var: LocalId,
        iterable: &'a Expr,
        body: &'a [Stmt],
    ) -> Result<(), CodegenError> {
        let local = self.local_var(var)?;

        let iterable = self.lower_expr(iterable)?;
        let iterator = self.call_rt(self.runtime.iter_new, &[iterable])?;
        self.emit_exc_check()?;
        let iter_var = self.temp_var();
        self.builder.def_var(iter_var, iterator);

        // Slot for the yielded value, created once before the loop starts.
        let out_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            3,
        ));
        let out_addr = self.builder.ins().stack_addr(self.ptr_ty, out_slot, 0);

        let next_block = self.builder.create_block();
        let body_block = self.builder.create_block();
        let after = self.builder.create_block();

        self.jump(next_block);
        self.switch_to(next_block);
        // Loop head / backedge (also the `continue` target).
        self.emit_loop_safepoint();
        let iterator = self.builder.use_var(iter_var);
        let id = self.runtime.iter_next;
        let produced = self.call_rt(id, &[iterator, out_addr])?;
        self.emit_exc_check()?;
        // `0` means the iterator is exhausted.
        let exhausted = self.builder.ins().icmp_imm(IntCC::Equal, produced, 0);
        self.builder
            .ins()
            .brif(exhausted, after, &[], body_block, &[]);

        self.loops.push(LoopTargets {
            break_block: after,
            continue_block: next_block,
        });
        self.switch_to(body_block);
        let value = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), out_addr, 0);
        self.builder.def_var(local, value);
        self.lower_stmts(body)?;
        if !self.terminated {
            self.jump(next_block);
        }
        self.loops.pop();

        self.switch_to(after);
        Ok(())
    }

    /// Lower a comprehension (including eager generator expressions).
    ///
    /// The result container is created in the current block and kept in a
    /// temporary variable so the generated appends always see a live tagged
    /// root. The clauses are lowered recursively; each level runs its own
    /// iterator loop and jumps back to its head after the nested clauses.
    fn lower_comprehension(
        &mut self,
        clauses: &'a [CompClause],
        emit: CompEmit<'a>,
    ) -> Result<Value, CodegenError> {
        if clauses.is_empty() {
            return Err(CodegenError::new(
                "comprehension has no `for` clause to iterate",
            ));
        }

        // An empty argument array still needs a valid, non-null pointer.
        let empty_slot = self.stack_array_slot(0, "comprehension result")?;
        let empty_ptr = self.builder.ins().stack_addr(self.ptr_ty, empty_slot, 0);
        let zero = self.builder.ins().iconst(self.ptr_ty, 0);
        let result = match emit {
            CompEmit::List(_) => self.call_rt(self.runtime.list_new, &[zero, empty_ptr])?,
            CompEmit::Set(_) => self.call_rt(self.runtime.set_new, &[zero, empty_ptr])?,
            CompEmit::Dict(..) => {
                self.call_rt(self.runtime.dict_new, &[zero, empty_ptr, empty_ptr])?
            }
        };
        self.emit_exc_check()?;
        let result_var = self.temp_var();
        self.builder.def_var(result_var, result);

        self.lower_comp_level(clauses, 0, result_var, emit)?;
        Ok(self.builder.use_var(result_var))
    }

    /// Lower one `for target in iterable if ...` level of a comprehension.
    fn lower_comp_level(
        &mut self,
        clauses: &'a [CompClause],
        index: usize,
        result_var: Variable,
        emit: CompEmit<'a>,
    ) -> Result<(), CodegenError> {
        let clause = clauses.get(index).ok_or_else(|| {
            CodegenError::new(format!(
                "internal error: comprehension clause {index} is out of range"
            ))
        })?;

        // The iterable is evaluated once per execution of this level, in the
        // block inherited from the enclosing clause.
        let iterable = self.lower_expr(&clause.iterable)?;
        let iterator = self.call_rt(self.runtime.iter_new, &[iterable])?;
        self.emit_exc_check()?;
        let iter_var = self.temp_var();
        self.builder.def_var(iter_var, iterator);

        // Slot for the element yielded by `pyx_iter_next`, created once.
        let out_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            3,
        ));
        let out_addr = self.builder.ins().stack_addr(self.ptr_ty, out_slot, 0);

        let loop_block = self.builder.create_block();
        let body_block = self.builder.create_block();
        let after = self.builder.create_block();

        self.jump(loop_block);
        self.switch_to(loop_block);
        // Comprehension iterator loop head; also the backedge and the target
        // of a failed `if` filter.
        self.emit_loop_safepoint();
        let iterator = self.builder.use_var(iter_var);
        let produced = self.call_rt(self.runtime.iter_next, &[iterator, out_addr])?;
        self.emit_exc_check()?;
        // `0` means the iterator is exhausted; anything else yielded a value.
        let exhausted = self.builder.ins().icmp_imm(IntCC::Equal, produced, 0);
        self.builder
            .ins()
            .brif(exhausted, after, &[], body_block, &[]);

        self.switch_to(body_block);
        let value = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), out_addr, 0);
        self.bind_comp_target(&clause.target, value)?;

        // Conditions run in source order; a falsy one skips to the next item.
        for condition in &clause.conditions {
            let value = self.lower_expr(condition)?;
            let truth = self.truthy_cond(value)?;
            let next_block = self.builder.create_block();
            self.builder
                .ins()
                .brif(truth, next_block, &[], loop_block, &[]);
            self.switch_to(next_block);
        }

        if index + 1 < clauses.len() {
            self.lower_comp_level(clauses, index + 1, result_var, emit)?;
        } else {
            self.emit_comp_element(result_var, emit)?;
        }

        if !self.terminated {
            self.jump(loop_block);
        }
        self.switch_to(after);
        Ok(())
    }

    /// Bind the value yielded by the iterator to a comprehension target.
    fn bind_comp_target(
        &mut self,
        target: &'a CompTarget,
        value: Value,
    ) -> Result<(), CodegenError> {
        match target {
            CompTarget::Name(id) => {
                let var = self.local_var(*id)?;
                self.builder.def_var(var, value);
            }
            CompTarget::Unpack(targets) => {
                // Keep the sequence in a temporary root while its elements are
                // read out one by one.
                let holder = self.temp_var();
                self.builder.def_var(holder, value);
                let expected = self.builder.ins().iconst(self.ptr_ty, targets.len() as i64);
                let _ = self.call_rt(self.runtime.unpack, &[value, expected])?;
                self.emit_exc_check()?;
                for (index, id) in targets.iter().enumerate() {
                    let items = self.builder.use_var(holder);
                    let index = self.tagged_int((index as i64).wrapping_shl(3));
                    let item = self.call_rt(self.runtime.subscript, &[items, index])?;
                    self.emit_exc_check()?;
                    let var = self.local_var(*id)?;
                    self.builder.def_var(var, item);
                }
            }
        }
        Ok(())
    }

    /// Append the element (or key/value pair) of a matched comprehension.
    fn emit_comp_element(
        &mut self,
        result_var: Variable,
        emit: CompEmit<'a>,
    ) -> Result<(), CodegenError> {
        match emit {
            CompEmit::List(element) => {
                let value = self.lower_expr(element)?;
                let result = self.builder.use_var(result_var);
                let _ = self.call_rt(self.runtime.list_append, &[result, value])?;
                self.emit_exc_check()?;
            }
            CompEmit::Set(element) => {
                let value = self.lower_expr(element)?;
                let result = self.builder.use_var(result_var);
                let _ = self.call_rt(self.runtime.set_add, &[result, value])?;
                self.emit_exc_check()?;
            }
            CompEmit::Dict(key, value) => {
                // Python evaluates the key before the value.
                let key = self.lower_expr(key)?;
                let value = self.lower_expr(value)?;
                let result = self.builder.use_var(result_var);
                let _ = self.call_rt(self.runtime.dict_set, &[result, key, value])?;
                self.emit_exc_check()?;
            }
        }
        Ok(())
    }

    fn lower_try(
        &mut self,
        body: &'a [Stmt],
        handlers: &'a [Handler],
        else_body: &'a [Stmt],
        finally_body: &'a [Stmt],
    ) -> Result<(), CodegenError> {
        let outer_exc = self.exc_target;
        let after = self.builder.create_block();
        let has_finally = !finally_body.is_empty();
        let has_handlers = !handlers.is_empty();

        // Register the `finally` before lowering anything else so that every
        // return/break/continue/exception inside the region finds it.
        let cleanup = if has_finally {
            let finally_entry = self.builder.create_block();
            let action_var = self.temp_var();
            let zero = self.tagged_int(0);
            self.builder.def_var(action_var, zero);
            let pending_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            // Tagged `None`: only an exception entry parks a real value.
            let none = self.none();
            self.builder.ins().stack_store(none, pending_slot, 0);
            let id = self.cleanup_infos.len();
            let break_loop = self.loops.len().checked_sub(1);
            self.cleanup_infos.push(CleanupInfo {
                finally_entry,
                action_var,
                break_loop,
                outer_exc,
                pending_slot,
            });
            self.cleanups.push(id);
            Some(id)
        } else {
            None
        };

        let dispatch = if has_handlers {
            Some(self.builder.create_block())
        } else {
            None
        };
        let inner_exc = match (dispatch, cleanup) {
            (Some(handler), _) => ExcTarget::Handler(handler),
            (None, Some(id)) => ExcTarget::Cleanup(id),
            (None, None) => outer_exc,
        };
        let handler_exc = match cleanup {
            Some(id) => ExcTarget::Cleanup(id),
            None => outer_exc,
        };

        // `try` body.
        self.exc_target = inner_exc;
        self.lower_stmts(body)?;
        if !self.terminated {
            if else_body.is_empty() {
                self.emit_finally_normal(cleanup, after)?;
            } else {
                let else_block = self.builder.create_block();
                self.jump(else_block);
                self.switch_to(else_block);
                // Exceptions in `else` bypass this statement's handlers.
                self.exc_target = handler_exc;
                self.lower_stmts(else_body)?;
                if !self.terminated {
                    self.emit_finally_normal(cleanup, after)?;
                }
            }
        }

        // `except` dispatch.
        if let Some(dispatch) = dispatch {
            self.switch_to(dispatch);
            self.exc_target = handler_exc;

            let pending_id = self.runtime.exc_pending;
            let pending = self.call_rt(pending_id, &[])?;
            let has_exception = self.builder.ins().icmp_imm(IntCC::NotEqual, pending, 0);
            let matched = self.builder.create_block();
            self.builder
                .ins()
                .brif(has_exception, matched, &[], after, &[]);
            self.switch_to(matched);

            let unmatched = self.builder.create_block();
            for (position, handler) in handlers.iter().enumerate() {
                let handler_block = self.builder.create_block();
                let next_handler = if position + 1 == handlers.len() {
                    unmatched
                } else {
                    self.builder.create_block()
                };
                match handler.exc {
                    None => {
                        // Bare `except:` catches everything; the frontend
                        // guarantees it is the last arm.
                        self.jump(handler_block);
                    }
                    Some(kind) => {
                        let tag_id = self.runtime.exc_tag;
                        let tag = self.call_rt(tag_id, &[])?;
                        let matches =
                            self.builder
                                .ins()
                                .icmp_imm(IntCC::Equal, tag, kind.tag() as i64);
                        self.builder
                            .ins()
                            .brif(matches, handler_block, &[], next_handler, &[]);
                    }
                }

                self.switch_to(handler_block);
                // Always capture the exception object before clearing the
                // pending flag: bare `raise` must re-raise this object.
                let exc_var = self.temp_var();
                let value_id = self.runtime.exc_value;
                let value = self.call_rt(value_id, &[])?;
                self.builder.def_var(exc_var, value);
                if let Some(binding) = handler.binding {
                    let binding_var = self.local_var(binding)?;
                    self.builder.def_var(binding_var, value);
                }
                let clear_id = self.runtime.exc_clear;
                self.call_rt(clear_id, &[])?;
                self.exc_target = handler_exc;
                let previous_exc = self.current_exc.replace(exc_var);
                self.lower_stmts(handler.body.as_slice())?;
                self.current_exc = previous_exc;
                if !self.terminated {
                    self.emit_finally_normal(cleanup, after)?;
                }

                self.switch_to(next_handler);
            }

            // No handler matched: propagate to the enclosing target.
            let target = self.exc_target;
            self.emit_jump_exc_target(target)?;
        }

        if let Some(id) = cleanup {
            let popped = self.cleanups.pop();
            if popped != Some(id) {
                return Err(CodegenError::new(
                    "internal error: try cleanup stack is unbalanced",
                ));
            }
            let info = self.cleanup_info(id)?;
            let saved_current = self.current_exc;

            // Exceptions raised (or re-raised) by the `finally` body replace
            // the in-flight exception and propagate to the outer target.
            self.exc_target = info.outer_exc;
            self.suppress_pending += 1;
            self.switch_to(info.finally_entry);
            // A bare `raise` in this body re-raises the exception parked in
            // this cleanup's slot (or falls back to RuntimeError when the
            // body was entered normally and the slot is `None`).
            self.finally_slots.push(info.pending_slot);
            self.lower_stmts(finally_body)?;
            if self.finally_slots.pop() != Some(info.pending_slot) {
                return Err(CodegenError::new(
                    "internal error: finally slot stack is unbalanced",
                ));
            }
            self.suppress_pending -= 1;
            self.current_exc = saved_current;

            if !self.terminated {
                let dispatch_block = self.builder.create_block();
                self.jump(dispatch_block);
                self.switch_to(dispatch_block);
                self.emit_action_dispatch(id, after)?;
            }

            self.switch_to(after);
            self.exc_target = outer_exc;
            return Ok(());
        }

        self.switch_to(after);
        self.exc_target = outer_exc;
        Ok(())
    }

    fn lower_raise(
        &mut self,
        exc: Option<ExcKind>,
        msg: Option<&'a Expr>,
    ) -> Result<(), CodegenError> {
        match exc {
            Some(kind) => {
                let message = match msg {
                    Some(expr) => self.lower_expr(expr)?,
                    None => self.none(),
                };
                let tag = self.builder.ins().iconst(types::I32, kind.tag() as i64);
                let id = self.runtime.raise;
                self.call_rt(id, &[tag, message])?;
                let target = self.exc_target;
                self.emit_jump_exc_target(target)?;
            }
            None => {
                if msg.is_some() {
                    return Err(CodegenError::new(
                        "invalid IR: raise without an exception kind but with a message",
                    ));
                }
                // Bare raise inside an `except` arm re-raises the handled
                // exception object, preserving type, message and location.
                if let Some(exc_var) = self.current_exc {
                    let value = self.builder.use_var(exc_var);
                    let id = self.runtime.raise_value;
                    self.call_rt(id, &[value])?;
                    let target = self.exc_target;
                    self.emit_jump_exc_target(target)?;
                } else if let Some(slot) = self.finally_slots.last().copied() {
                    // A bare `raise` in a `finally` body re-raises the
                    // exception that forced the `finally` to run. The slot
                    // holds it only for an exception entry; every other entry
                    // (normal completion, return, break, continue) reset it
                    // to tagged `None`, in which case CPython reports that no
                    // exception is active.
                    let value = self.builder.ins().stack_load(types::I64, slot, 0);
                    let none = self.none();
                    let has_value = self.builder.ins().icmp(IntCC::NotEqual, value, none);
                    let parked = self.builder.create_block();
                    let fallback = self.builder.create_block();
                    self.builder
                        .ins()
                        .brif(has_value, parked, &[], fallback, &[]);

                    self.switch_to(parked);
                    let id = self.runtime.raise_value;
                    self.call_rt(id, &[value])?;
                    let target = self.exc_target;
                    self.emit_jump_exc_target(target)?;

                    self.switch_to(fallback);
                    self.lower_bare_raise_fallback()?;
                } else {
                    self.lower_bare_raise_fallback()?;
                }
            }
        }
        Ok(())
    }

    /// Bare `raise` without a handled or parked exception: propagate a
    /// pending exception if there is one, otherwise raise `RuntimeError`.
    fn lower_bare_raise_fallback(&mut self) -> Result<(), CodegenError> {
        let pending_id = self.runtime.exc_pending;
        let pending = self.call_rt(pending_id, &[])?;
        let has_exception = self.builder.ins().icmp_imm(IntCC::NotEqual, pending, 0);
        let propagate = self.builder.create_block();
        let missing = self.builder.create_block();
        self.builder
            .ins()
            .brif(has_exception, propagate, &[], missing, &[]);

        self.switch_to(propagate);
        let target = self.exc_target;
        self.emit_jump_exc_target(target)?;

        self.switch_to(missing);
        let message = self.str_const("No active exception to reraise")?;
        let tag = self
            .builder
            .ins()
            .iconst(types::I32, ExcKind::RuntimeError.tag() as i64);
        let id = self.runtime.raise;
        self.call_rt(id, &[tag, message])?;
        let target = self.exc_target;
        self.emit_jump_exc_target(target)?;
        Ok(())
    }

    fn lower_expr(&mut self, expr: &'a Expr) -> Result<Value, CodegenError> {
        match &expr.kind {
            ExprKind::Const(value) => self.lower_const(value),
            ExprKind::Local(id) => {
                let var = self.local_var(*id)?;
                Ok(self.builder.use_var(var))
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let lhs = self.lower_expr(lhs)?;
                let rhs = self.lower_expr(rhs)?;
                self.lower_binop(*op, lhs, rhs)
            }
            ExprKind::Un { op, operand } => {
                let operand = self.lower_expr(operand)?;
                let code = self.builder.ins().iconst(types::I32, op.code() as i64);
                let id = self.runtime.unop;
                let result = self.call_rt(id, &[code, operand])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            ExprKind::BoolOp { op, lhs, rhs } => self.lower_boolop(*op, lhs, rhs),
            ExprKind::List(items) => {
                let id = self.runtime.list_new;
                self.lower_items_literal(items, "list literal", id)
            }
            ExprKind::Tuple(items) => {
                let id = self.runtime.tuple_new;
                self.lower_items_literal(items, "tuple literal", id)
            }
            ExprKind::Set(items) => {
                let id = self.runtime.set_new;
                self.lower_items_literal(items, "set literal", id)
            }
            ExprKind::Dict(pairs) => {
                let key_slot = self.stack_array_slot(pairs.len(), "dict literal keys")?;
                let value_slot = self.stack_array_slot(pairs.len(), "dict literal values")?;
                // Python evaluates each key before its value, pairs in source
                // order; duplicate keys are resolved by the runtime.
                for (index, (key, value)) in pairs.iter().enumerate() {
                    let key = self.lower_expr(key)?;
                    self.builder
                        .ins()
                        .stack_store(key, key_slot, (index * 8) as i32);
                    let value = self.lower_expr(value)?;
                    self.builder
                        .ins()
                        .stack_store(value, value_slot, (index * 8) as i32);
                }
                let keys_ptr = self.builder.ins().stack_addr(self.ptr_ty, key_slot, 0);
                let values_ptr = self.builder.ins().stack_addr(self.ptr_ty, value_slot, 0);
                let argc = self.builder.ins().iconst(self.ptr_ty, pairs.len() as i64);
                let id = self.runtime.dict_new;
                let result = self.call_rt(id, &[argc, keys_ptr, values_ptr])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            ExprKind::ListComp { element, clauses } => {
                self.lower_comprehension(clauses.as_slice(), CompEmit::List(element))
            }
            ExprKind::SetComp { element, clauses } => {
                self.lower_comprehension(clauses.as_slice(), CompEmit::Set(element))
            }
            ExprKind::DictComp {
                key,
                value,
                clauses,
            } => self.lower_comprehension(clauses.as_slice(), CompEmit::Dict(key, value)),
            ExprKind::GeneratorExp { element, clauses } => {
                // Materialised eagerly until real generator objects exist.
                self.lower_comprehension(clauses.as_slice(), CompEmit::List(element))
            }
            ExprKind::Subscript { object, index } => {
                let object = self.lower_expr(object)?;
                let index = self.lower_expr(index)?;
                let id = self.runtime.subscript;
                let result = self.call_rt(id, &[object, index])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            ExprKind::Slice {
                object,
                start,
                stop,
                step,
            } => {
                // Python evaluates the sliced object before the bounds, then
                // the bounds in source order (start, stop, step).
                let object = self.lower_expr(object)?;
                let start = match start {
                    Some(start) => self.lower_expr(start)?,
                    None => self.none(),
                };
                let stop = match stop {
                    Some(stop) => self.lower_expr(stop)?,
                    None => self.none(),
                };
                let step = match step {
                    Some(step) => self.lower_expr(step)?,
                    None => self.none(),
                };
                let id = self.runtime.slice;
                let result = self.call_rt(id, &[object, start, stop, step])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            ExprKind::ModuleCall {
                module,
                function,
                args,
            } => self.lower_module_call(module, function, args),
            ExprKind::ModuleAttr { module, name } => self.lower_module_attr(module, name),
            ExprKind::GetAttr { object, name } => self.lower_getattr(object, name),
            ExprKind::MethodCall {
                object,
                method,
                args,
                keywords,
            } => self.lower_method_call(object, method, args, keywords),
            ExprKind::FunctionRef(id) => self.lower_function_ref(*id),
            ExprKind::BuiltinRef(builtin) => self.lower_builtin_ref(*builtin),
            ExprKind::IndirectCall { callee, args } => self.lower_indirect_call(callee, args),
            ExprKind::Call { callee, args } => self.lower_call(*callee, args),
        }
    }

    fn lower_binop(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Value, CodegenError> {
        let code = self.builder.ins().iconst(types::I32, op.code() as i64);
        let id = self.runtime.binop;
        let result = self.call_rt(id, &[code, lhs, rhs])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    fn lower_boolop(
        &mut self,
        op: BoolOp,
        lhs: &'a Expr,
        rhs: &'a Expr,
    ) -> Result<Value, CodegenError> {
        let lhs_value = self.lower_expr(lhs)?;
        let condition = self.truthy_cond(lhs_value)?;
        let rhs_block = self.builder.create_block();
        let after = self.builder.create_block();
        self.builder.append_block_param(after, types::I64);

        let lhs_arg = [ir::BlockArg::from(lhs_value)];
        match op {
            BoolOp::And => {
                // `lhs and rhs`: falsy lhs is returned as-is.
                self.builder
                    .ins()
                    .brif(condition, rhs_block, &[], after, &lhs_arg);
            }
            BoolOp::Or => {
                // `lhs or rhs`: truthy lhs is returned as-is.
                self.builder
                    .ins()
                    .brif(condition, after, &lhs_arg, rhs_block, &[]);
            }
        }

        self.switch_to(rhs_block);
        let rhs_value = self.lower_expr(rhs)?;
        let rhs_arg = [ir::BlockArg::from(rhs_value)];
        self.builder.ins().jump(after, &rhs_arg);
        self.terminated = true;

        self.switch_to(after);
        self.builder
            .block_params(after)
            .first()
            .copied()
            .ok_or_else(|| {
                CodegenError::new("internal error: BoolOp result block has no parameter")
            })
    }

    /// Lower `object.method(args..., name=value...)` to a dynamic method call.
    ///
    /// Python evaluates the receiver, then the positional arguments, then the
    /// keyword values, each left to right. The receiver and each already
    /// evaluated argument are stored into stack slots immediately so that a
    /// collection triggered by a later allocation can still observe them.
    /// Calls without keywords keep using `pyx_method`; calls with keywords use
    /// `pyx_method_kw` with `[name, value]` pairs.
    fn lower_method_call(
        &mut self,
        object: &'a Expr,
        method: &str,
        args: &'a [Expr],
        keywords: &'a [(String, Expr)],
    ) -> Result<Value, CodegenError> {
        let receiver_slot = self.stack_array_slot(1, "method receiver")?;
        let receiver = self.lower_expr(object)?;
        self.builder.ins().stack_store(receiver, receiver_slot, 0);

        let argv_slot = self.stack_array_slot(args.len(), "method arguments")?;
        for (index, arg) in args.iter().enumerate() {
            let value = self.lower_expr(arg)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (index * 8) as i32);
        }

        let receiver = self.builder.ins().stack_load(types::I64, receiver_slot, 0);
        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, method.as_bytes())?;
        let name = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let argc = self.builder.ins().iconst(self.ptr_ty, args.len() as i64);
        // `argc == 0` still passes a valid pointer (the slot has one element).
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);

        if keywords.is_empty() {
            let id = self.runtime.method;
            let result = self.call_rt(id, &[receiver, name, name_len, argc, argv])?;
            self.emit_exc_check()?;
            return Ok(result);
        }

        let pairs = keywords
            .len()
            .checked_mul(2)
            .ok_or_else(|| CodegenError::new("method call: too many keyword arguments"))?;
        let kw_slot = self.stack_array_slot(pairs, "method keyword arguments")?;
        for (index, (keyword, value)) in keywords.iter().enumerate() {
            // Names are tagged strings, like every other ABI value.
            let keyword = self.str_const(keyword)?;
            self.builder
                .ins()
                .stack_store(keyword, kw_slot, (index * 16) as i32);
            let value = self.lower_expr(value)?;
            self.builder
                .ins()
                .stack_store(value, kw_slot, (index * 16 + 8) as i32);
        }
        let kwargc = self
            .builder
            .ins()
            .iconst(self.ptr_ty, keywords.len() as i64);
        let kw_argv = self.builder.ins().stack_addr(self.ptr_ty, kw_slot, 0);
        let id = self.runtime.method_kw;
        let result = self.call_rt(id, &[receiver, name, name_len, argc, argv, kwargc, kw_argv])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower `module.function(args...)` to `pyx_module_call`.
    ///
    /// Python evaluates the arguments left to right before the call; each
    /// value is stored into an 8-byte-aligned stack array immediately so that
    /// a collection triggered by a later allocation cannot observe a stale
    /// value. Module and function names are materialized in byte slots.
    fn lower_module_call(
        &mut self,
        module: &str,
        function: &str,
        args: &'a [Expr],
    ) -> Result<Value, CodegenError> {
        let argv_slot = self.stack_array_slot(args.len(), "module call arguments")?;
        for (index, arg) in args.iter().enumerate() {
            let value = self.lower_expr(arg)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (index * 8) as i32);
        }

        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (module_slot, module_len) = make_bytes_slot(&mut self.builder, module.as_bytes())?;
        let module_ptr = self.builder.ins().stack_addr(self.ptr_ty, module_slot, 0);
        let module_len = self.builder.ins().iconst(self.ptr_ty, module_len as i64);
        let (function_slot, function_len) =
            make_bytes_slot(&mut self.builder, function.as_bytes())?;
        let function_ptr = self.builder.ins().stack_addr(self.ptr_ty, function_slot, 0);
        let function_len = self.builder.ins().iconst(self.ptr_ty, function_len as i64);
        let argc = self.builder.ins().iconst(self.ptr_ty, args.len() as i64);
        // `argc == 0` still passes a valid pointer (the slot has one element).
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        let id = self.runtime.module_call;
        let result = self.call_rt(
            id,
            &[
                module_ptr,
                module_len,
                function_ptr,
                function_len,
                argc,
                argv,
            ],
        )?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower `module.name` to `pyx_module_get` (constant or native function
    /// value).
    fn lower_module_attr(&mut self, module: &str, name: &str) -> Result<Value, CodegenError> {
        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (module_slot, module_len) = make_bytes_slot(&mut self.builder, module.as_bytes())?;
        let module_ptr = self.builder.ins().stack_addr(self.ptr_ty, module_slot, 0);
        let module_len = self.builder.ins().iconst(self.ptr_ty, module_len as i64);
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
        let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let id = self.runtime.module_get;
        let result = self.call_rt(id, &[module_ptr, module_len, name_ptr, name_len])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower `object.name` to `pyx_getattr`: the receiver is evaluated first,
    /// then the runtime resolves the data attribute by receiver kind.
    fn lower_getattr(&mut self, object: &'a Expr, name: &str) -> Result<Value, CodegenError> {
        let object = self.lower_expr(object)?;
        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
        let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let id = self.runtime.getattr;
        let result = self.call_rt(id, &[object, name_ptr, name_len])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower a list/tuple/set literal: the elements are evaluated left-to-right
    /// into an 8-byte-aligned stack array, then handed to the runtime
    /// constructor `id` as `(argc, argv)`.
    fn lower_items_literal(
        &mut self,
        items: &'a [Expr],
        what: &str,
        id: FuncId,
    ) -> Result<Value, CodegenError> {
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            values.push(self.lower_expr(item)?);
        }
        let slot = self.stack_array_slot(values.len(), what)?;
        for (index, value) in values.iter().enumerate() {
            self.builder
                .ins()
                .stack_store(*value, slot, (index * 8) as i32);
        }
        let items_ptr = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let argc = self.builder.ins().iconst(self.ptr_ty, values.len() as i64);
        let result = self.call_rt(id, &[argc, items_ptr])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Create an 8-byte-aligned stack slot that can hold `count` tagged values
    /// (at least one slot, so the address is valid even for empty literals).
    fn stack_array_slot(&mut self, count: usize, what: &str) -> Result<StackSlot, CodegenError> {
        let bytes = count
            .checked_mul(8)
            .ok_or_else(|| CodegenError::new(format!("{what}: too many elements")))?;
        let bytes = bytes.max(8);
        if bytes > i32::MAX as usize {
            return Err(CodegenError::new(format!("{what}: too large")));
        }
        let size = bytes as u32;
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        Ok(slot)
    }

    fn lower_const(&mut self, value: &Const) -> Result<Value, CodegenError> {
        match value {
            Const::Int(value) => Ok(self.tagged_int(value.wrapping_shl(3))),
            Const::Bool(true) => Ok(self.tagged_int(TRUE_TAG)),
            Const::Bool(false) => Ok(self.tagged_int(FALSE_TAG)),
            Const::None => Ok(self.none()),
            Const::Float(value) => {
                let float = self.builder.ins().f64const(*value);
                let id = self.runtime.float_new;
                self.call_rt(id, &[float])
            }
            Const::Str(text) => self.str_const(text),
            Const::Bytes(bytes) => self.bytes_const(bytes),
        }
    }

    /// Lower a reference to a program function to a first-class function
    /// value via `pyx_function_new(wrapper, arity, name, name_len)`.
    fn lower_function_ref(&mut self, id: FunctionId) -> Result<Value, CodegenError> {
        let program = self.program;
        let function = program
            .functions
            .get(id as usize)
            .ok_or_else(|| CodegenError::new(format!("reference to unknown function id {id}")))?;
        if function.capture_count != 0 {
            return Err(CodegenError::new(format!(
                "internal error: function '{}' has captures and needs a closure value",
                function.name
            )));
        }
        let wrapper = *self.wrappers.get(&id).ok_or_else(|| {
            CodegenError::new(format!(
                "reference to undeclared wrapper for function id {id}"
            ))
        })?;
        let reference = self.func_ref(wrapper);
        let wrapper_address = self.builder.ins().func_addr(self.ptr_ty, reference);
        let wrapper_address = self.pointer_to_i64(wrapper_address);
        let arity = self
            .builder
            .ins()
            .iconst(self.ptr_ty, function.params.len() as i64);
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, function.name.as_bytes())?;
        let name = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let id = self.runtime.function_new;
        let result = self.call_rt(id, &[wrapper_address, arity, name, name_len])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower a reference to a builtin function to a first-class function
    /// value via `pyx_builtin_function(name, name_len)`.
    fn lower_builtin_ref(&mut self, builtin: Builtin) -> Result<Value, CodegenError> {
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, builtin.name().as_bytes())?;
        let name = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let id = self.runtime.builtin_function;
        let result = self.call_rt(id, &[name, name_len])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower a call through a function value via `pyx_call(fn, argc, argv)`.
    ///
    /// Python evaluates the callee first, then the arguments left to right.
    /// Both the callee and every already-evaluated argument are parked in
    /// stack slots immediately so that later evaluation cannot invalidate
    /// them.
    fn lower_indirect_call(
        &mut self,
        callee: &'a Expr,
        args: &'a [Expr],
    ) -> Result<Value, CodegenError> {
        let callee_slot = self.stack_array_slot(1, "call callee")?;
        let callee = self.lower_expr(callee)?;
        self.builder.ins().stack_store(callee, callee_slot, 0);

        let argv_slot = self.stack_array_slot(args.len(), "call arguments")?;
        for (position, arg) in args.iter().enumerate() {
            let value = self.lower_expr(arg)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (position * 8) as i32);
        }

        let callee = self.builder.ins().stack_load(types::I64, callee_slot, 0);
        let argc = self.builder.ins().iconst(self.ptr_ty, args.len() as i64);
        // `argc == 0` still passes a valid pointer: the slot holds one
        // element, which is 8-byte aligned.
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        let id = self.runtime.call;
        let result = self.call_rt(id, &[callee, argc, argv])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Reinterpret a pointer-width value as the ABI's `i64`.
    fn pointer_to_i64(&mut self, value: Value) -> Value {
        if self.ptr_ty == types::I64 {
            value
        } else {
            self.builder.ins().uextend(types::I64, value)
        }
    }

    fn lower_call(&mut self, callee: Callee, args: &'a [Expr]) -> Result<Value, CodegenError> {
        // F-string fields carry their static spec and conversion flag as
        // trailing constant arguments; they become a direct runtime call
        // instead of a first-class builtin invocation.
        if callee == Callee::Builtin(Builtin::Format) {
            return self.lower_format_call(args);
        }
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.lower_expr(arg)?);
        }

        match callee {
            Callee::Function(id) => {
                let function = self.program.functions.get(id as usize).ok_or_else(|| {
                    CodegenError::new(format!("call to unknown function id {id}"))
                })?;
                if function.capture_count != 0 {
                    return Err(CodegenError::new(format!(
                        "internal error: function '{}' has captures and needs an indirect call",
                        function.name
                    )));
                }
                if function.params.len() != values.len() {
                    let name = &function.name;
                    let expected = function.params.len();
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "call to function {name} expects {expected} arguments, got {actual}"
                    )));
                }
                let target = *self.symbols.get(&id).ok_or_else(|| {
                    CodegenError::new(format!("call to undeclared function id {id}"))
                })?;
                let reference = self.func_ref(target);
                let call = self.builder.ins().call(reference, &values);
                let result = self
                    .builder
                    .inst_results(call)
                    .first()
                    .copied()
                    .ok_or_else(|| {
                        CodegenError::new("internal error: user call returned no value")
                    })?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Callee::Builtin(builtin) => self.lower_builtin(builtin, &values),
        }
    }

    /// Lowers one f-string interpolation field to `pyx_format_value`.
    fn lower_format_call(&mut self, args: &'a [Expr]) -> Result<Value, CodegenError> {
        if args.len() != 3 {
            return Err(CodegenError::new(format!(
                "internal error: f-string format field expects 3 arguments, got {}",
                args.len()
            )));
        }
        let spec = match &args[1].kind {
            ExprKind::Const(Const::Str(spec)) => spec.as_str(),
            _ => {
                return Err(CodegenError::new(
                    "internal error: f-string format spec is not a constant string",
                ));
            }
        };
        let conversion = match &args[2].kind {
            ExprKind::Const(Const::Int(conversion)) if (0..=3).contains(conversion) => *conversion,
            _ => {
                return Err(CodegenError::new(
                    "internal error: f-string conversion flag is not 0..=3",
                ));
            }
        };
        let value = self.lower_expr(&args[0])?;
        let (spec_slot, spec_len) = make_bytes_slot(&mut self.builder, spec.as_bytes())?;
        let spec_ptr = self.builder.ins().stack_addr(self.ptr_ty, spec_slot, 0);
        let spec_len = self.builder.ins().iconst(self.ptr_ty, spec_len as i64);
        let conversion = self.builder.ins().iconst(types::I32, conversion);
        let id = self.runtime.format;
        let result = self.call_rt(id, &[value, spec_ptr, spec_len, conversion])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    fn lower_builtin(&mut self, builtin: Builtin, values: &[Value]) -> Result<Value, CodegenError> {
        match builtin {
            Builtin::Format => Err(CodegenError::new(
                "internal error: f-string format field reached first-class builtin lowering",
            )),
            Builtin::Print => {
                let bytes = values
                    .len()
                    .checked_mul(8)
                    .ok_or_else(|| CodegenError::new("print: too many arguments"))?;
                let size: u32 = bytes
                    .max(8)
                    .try_into()
                    .map_err(|_| CodegenError::new("print: argument list too large"))?;
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    size,
                    3,
                ));
                for (index, value) in values.iter().enumerate() {
                    self.builder
                        .ins()
                        .stack_store(*value, slot, (index * 8) as i32);
                }
                let argv = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
                let argc = self.builder.ins().iconst(types::I64, values.len() as i64);
                let id = self.runtime.print;
                let result = self.call_rt(id, &[argc, argv])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Bytes => {
                if values.len() > 2 {
                    return Err(CodegenError::new(format!(
                        "builtin bytes expects 0 to 2 arguments, got {}",
                        values.len()
                    )));
                }
                let slot = self.stack_array_slot(values.len(), "bytes()")?;
                for (index, value) in values.iter().enumerate() {
                    self.builder
                        .ins()
                        .stack_store(*value, slot, (index * 8) as i32);
                }
                let argv = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
                let argc = self.builder.ins().iconst(self.ptr_ty, values.len() as i64);
                let id = self.runtime.bytes_build;
                let result = self.call_rt(id, &[argc, argv])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Set => {
                match values.len() {
                    0 => {
                        // `set()` — a valid dummy pointer is required by the ABI.
                        let slot = self.stack_array_slot(0, "set()")?;
                        let dummy = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
                        let argc = self.builder.ins().iconst(self.ptr_ty, 0);
                        let id = self.runtime.set_new;
                        let result = self.call_rt(id, &[argc, dummy])?;
                        self.emit_exc_check()?;
                        Ok(result)
                    }
                    1 => {
                        let id = self.runtime.set_from;
                        let result = self.call_rt(id, &values[..1])?;
                        self.emit_exc_check()?;
                        Ok(result)
                    }
                    actual => Err(CodegenError::new(format!(
                        "builtin set expects at most 1 argument, got {actual}"
                    ))),
                }
            }
            Builtin::Range => {
                // Tagged integer 1 (`n << 3`), the default step.
                let one = self.tagged_int(1 << 3);
                let args = match values {
                    [stop] => [self.tagged_int(0), *stop, one],
                    [start, stop] => [*start, *stop, one],
                    [start, stop, step] => [*start, *stop, *step],
                    actual => {
                        let actual = actual.len();
                        return Err(CodegenError::new(format!(
                            "builtin range expects 1 to 3 arguments, got {actual}"
                        )));
                    }
                };
                let id = self.runtime.range_new;
                let result = self.call_rt(id, &args)?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::List => match values.len() {
                0 => {
                    // `list()` — a valid dummy pointer is required by the ABI.
                    let slot = self.stack_array_slot(0, "list()")?;
                    let dummy = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
                    let argc = self.builder.ins().iconst(self.ptr_ty, 0);
                    let id = self.runtime.list_new;
                    let result = self.call_rt(id, &[argc, dummy])?;
                    self.emit_exc_check()?;
                    Ok(result)
                }
                1 => {
                    let id = self.runtime.list_from;
                    let result = self.call_rt(id, &values[..1])?;
                    self.emit_exc_check()?;
                    Ok(result)
                }
                actual => Err(CodegenError::new(format!(
                    "builtin list expects at most 1 argument, got {actual}"
                ))),
            },
            Builtin::Dict => match values.len() {
                0 => {
                    // `dict()` — an empty table, no key/value arrays.
                    let argc = self.builder.ins().iconst(self.ptr_ty, 0);
                    let null = self.builder.ins().iconst(self.ptr_ty, 0);
                    let id = self.runtime.dict_new;
                    let result = self.call_rt(id, &[argc, null, null])?;
                    self.emit_exc_check()?;
                    Ok(result)
                }
                1 => {
                    let id = self.runtime.dict_from;
                    let result = self.call_rt(id, &values[..1])?;
                    self.emit_exc_check()?;
                    Ok(result)
                }
                actual => Err(CodegenError::new(format!(
                    "builtin dict expects at most 1 argument, got {actual}"
                ))),
            },
            Builtin::Sum | Builtin::Enumerate => {
                let (name, id) = match builtin {
                    Builtin::Sum => ("sum", self.runtime.sum),
                    _ => ("enumerate", self.runtime.enumerate),
                };
                let (iterable, start) = match values {
                    [iterable] => (*iterable, self.tagged_int(0)),
                    [iterable, start] => (*iterable, *start),
                    actual => {
                        let actual = actual.len();
                        return Err(CodegenError::new(format!(
                            "builtin {name} expects 1 or 2 arguments, got {actual}"
                        )));
                    }
                };
                let result = self.call_rt(id, &[iterable, start])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Zip => {
                let slot = self.stack_array_slot(values.len(), "zip()")?;
                for (index, value) in values.iter().enumerate() {
                    self.builder
                        .ins()
                        .stack_store(*value, slot, (index * 8) as i32);
                }
                let argv = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
                let argc = self.builder.ins().iconst(self.ptr_ty, values.len() as i64);
                let id = self.runtime.zip;
                let result = self.call_rt(id, &[argc, argv])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Map | Builtin::Filter => {
                if values.len() != 2 {
                    let name = builtin.name();
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin {name} expects 2 arguments, got {actual}"
                    )));
                }
                let id = match builtin {
                    Builtin::Map => self.runtime.map,
                    Builtin::Filter => self.runtime.filter,
                    _ => {
                        let name = builtin.name();
                        return Err(CodegenError::new(format!(
                            "internal error: {name} reached first-class builtin lowering"
                        )));
                    }
                };
                let result = self.call_rt(id, &values[..2])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Len | Builtin::Str | Builtin::Int | Builtin::Float | Builtin::Abs => {
                if values.len() != 1 {
                    let name = builtin.name();
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin {name} expects 1 argument, got {actual}"
                    )));
                }
                let id = match builtin {
                    Builtin::Len => self.runtime.len,
                    Builtin::Str => self.runtime.to_str,
                    Builtin::Int => self.runtime.to_int,
                    Builtin::Float => self.runtime.to_float,
                    Builtin::Abs => self.runtime.abs,
                    _ => {
                        let name = builtin.name();
                        return Err(CodegenError::new(format!(
                            "internal error: {name} reached scalar builtin lowering"
                        )));
                    }
                };
                let result = self.call_rt(id, &values[..1])?;
                self.emit_exc_check()?;
                Ok(result)
            }
        }
    }
}

/// Materialize `bytes` in a stack slot and return the slot plus its length.
///
/// Zero-length strings still get a one-byte slot so that a valid pointer can
/// be passed to `pyx_str_new`.
fn make_bytes_slot(
    builder: &mut FunctionBuilder,
    bytes: &[u8],
) -> Result<(StackSlot, usize), CodegenError> {
    let len = bytes.len();
    if len > i32::MAX as usize {
        return Err(CodegenError::new("string literal is too large"));
    }
    let size = len.max(1) as u32;
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 0));
    for (index, byte) in bytes.iter().enumerate() {
        let value = builder.ins().iconst(types::I8, *byte as i64);
        builder.ins().stack_store(value, slot, index as i32);
    }
    Ok((slot, len))
}

#[allow(clippy::explicit_auto_deref, clippy::needless_borrow)]
fn declare_ref<M: Module>(module: &mut M, builder: &mut FunctionBuilder, id: FuncId) -> FuncRef {
    module.declare_func_in_func(id, &mut *builder.func)
}

fn call_void_extern<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    id: FuncId,
    args: &[Value],
) {
    let reference = declare_ref(module, builder, id);
    builder.ins().call(reference, args);
}

fn call_val_extern<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    id: FuncId,
    args: &[Value],
) -> Value {
    let reference = declare_ref(module, builder, id);
    let inst = builder.ins().call(reference, args);
    builder.inst_results(inst)[0]
}

fn build_pyx_main<M: Module>(
    module: &mut M,
    program: &Program,
    id: FuncId,
    symbols: &HashMap<FunctionId, FuncId>,
    wrappers: &HashMap<FunctionId, FuncId>,
    runtime: &Runtime,
) -> Result<(), CodegenError> {
    let ptr_ty = module.target_config().pointer_type();
    let mut ctx = module.make_context();
    let signature = make_signature(module, &[], Some(types::I32));
    ctx.func.signature = signature;

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);

        call_void_extern(module, &mut builder, runtime.runtime_init, &[]);

        let (slot, len) = make_bytes_slot(&mut builder, program.source_name.as_bytes())?;
        let ptr = builder.ins().stack_addr(ptr_ty, slot, 0);
        let len_value = builder.ins().iconst(ptr_ty, len as i64);
        call_void_extern(module, &mut builder, runtime.set_file, &[ptr, len_value]);

        // Register every function (except the module body) before the spawn
        // check: a spawned child resolves its target through this registry
        // and never runs the module body.
        for (index, function) in program.functions.iter().enumerate() {
            // Closures have captures and cannot be resolved by name in a new
            // process; their wrapper expects capture parameters the registry
            // cannot supply.
            if index == program.main as usize
                || function.name == "<module>"
                || function.capture_count != 0
            {
                continue;
            }
            let wrapper = *wrappers.get(&(index as FunctionId)).ok_or_else(|| {
                CodegenError::new(format!("internal error: wrapper {index} was not declared"))
            })?;
            let reference = declare_ref(module, &mut builder, wrapper);
            let wrapper_address = builder.ins().func_addr(ptr_ty, reference);
            let (name_slot, name_len) = make_bytes_slot(&mut builder, function.name.as_bytes())?;
            let name = builder.ins().stack_addr(ptr_ty, name_slot, 0);
            let name_len = builder.ins().iconst(ptr_ty, name_len as i64);
            let arity = builder.ins().iconst(ptr_ty, function.params.len() as i64);
            call_void_extern(
                module,
                &mut builder,
                runtime.register_function,
                &[name, name_len, wrapper_address, arity],
            );
        }

        // `-1` means this is a normal start; anything else is the exit code
        // of a spawned child that already ran its single target function.
        let child = call_val_extern(module, &mut builder, runtime.spawn_child, &[]);
        let not_child = builder.ins().icmp_imm(IntCC::NotEqual, child, -1);
        let spawn_block = builder.create_block();
        let body_block = builder.create_block();
        builder
            .ins()
            .brif(not_child, spawn_block, &[], body_block, &[]);

        builder.switch_to_block(spawn_block);
        builder.ins().return_(&[child]);

        builder.switch_to_block(body_block);
        let module_main = program.main;
        let body = *symbols.get(&module_main).ok_or_else(|| {
            CodegenError::new(format!(
                "module body function {module_main} was not declared"
            ))
        })?;
        let body_ref = declare_ref(module, &mut builder, body);
        builder.ins().call(body_ref, &[]);

        let pending = call_val_extern(module, &mut builder, runtime.exc_pending, &[]);
        let has_exception = builder.ins().icmp_imm(IntCC::NotEqual, pending, 0);
        let uncaught_block = builder.create_block();
        let ok_block = builder.create_block();
        builder
            .ins()
            .brif(has_exception, uncaught_block, &[], ok_block, &[]);

        builder.switch_to_block(uncaught_block);
        let code = call_val_extern(module, &mut builder, runtime.uncaught, &[]);
        let code = builder.ins().ireduce(types::I32, code);
        builder.ins().return_(&[code]);

        builder.switch_to_block(ok_block);
        let zero = builder.ins().iconst(types::I32, 0);
        builder.ins().return_(&[zero]);

        builder.seal_all_blocks();
        builder.finalize();
    }

    module
        .define_function(id, &mut ctx)
        .map_err(|e| module_error("cannot define pyx_main", e))
}

fn build_c_main<M: Module>(
    module: &mut M,
    id: FuncId,
    pyx_main: FuncId,
) -> Result<(), CodegenError> {
    let ptr_ty = module.target_config().pointer_type();
    let mut ctx = module.make_context();
    let signature = make_signature(module, &[types::I32, ptr_ty], Some(types::I32));
    ctx.func.signature = signature;

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);

        let result = call_val_extern(module, &mut builder, pyx_main, &[]);
        builder.ins().return_(&[result]);

        builder.seal_all_blocks();
        builder.finalize();
    }

    module
        .define_function(id, &mut ctx)
        .map_err(|e| module_error("cannot define main", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyx_ir::{ClassMethod, LocalInfo, Param, StmtKind, Type};

    /// A module body with one `bytes`-typed local `x` assigned `value`.
    fn program_assigning(value: Expr) -> Program {
        Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![LocalInfo {
                    name: "x".to_string(),
                    ty: Type::Bytes,
                }],
                body: vec![Stmt::new(StmtKind::Assign { target: 0, value }, 1)],
            }],
            main: 0,
        }
    }

    #[test]
    fn bytes_literal_emits_an_object_file() {
        let program = program_assigning(Expr::new(
            ExprKind::Const(Const::Bytes(vec![b'a', 0, b'\n', b'\\'])),
            Type::Bytes,
            1,
        ));
        let object = crate::compile_object(&program).expect("bytes literal must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn bytes_builtin_calls_lower_to_the_runtime_builder() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let programs: [Vec<Expr>; 4] = [
            Vec::new(),
            vec![int(3)],
            vec![Expr::new(
                ExprKind::List(vec![int(65), int(66)]),
                Type::List,
                1,
            )],
            vec![text("x"), text("utf-8")],
        ];
        for args in programs {
            let program = program_assigning(Expr::new(
                ExprKind::Call {
                    callee: Callee::Builtin(Builtin::Bytes),
                    args,
                },
                Type::Bytes,
                1,
            ));
            let object =
                crate::compile_object(&program).expect("bytes() must lower to pyx_bytes_build");
            assert!(!object.is_empty());
        }
    }

    #[test]
    fn fstring_format_field_lowers_to_pyx_format_value() {
        let value = Expr::new(ExprKind::Const(Const::Int(42)), Type::Int, 1);
        let spec = Expr::new(ExprKind::Const(Const::Str("04d".to_string())), Type::Str, 1);
        let conversion = Expr::new(ExprKind::Const(Const::Int(0)), Type::Int, 1);
        let program = program_assigning(Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Format),
                args: vec![value, spec, conversion],
            },
            Type::Str,
            1,
        ));
        let object = crate::compile_object(&program).expect("format field must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn fstring_format_field_requires_a_constant_spec() {
        let value = Expr::new(ExprKind::Const(Const::Int(42)), Type::Int, 1);
        let spec = Expr::new(ExprKind::Local(0), Type::Str, 1);
        let conversion = Expr::new(ExprKind::Const(Const::Int(0)), Type::Int, 1);
        let program = program_assigning(Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Format),
                args: vec![value, spec, conversion],
            },
            Type::Str,
            1,
        ));
        let error = crate::compile_object(&program).expect_err("non-constant spec must fail");
        assert!(
            error.message.contains("constant string"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn bytes_builtin_rejects_too_many_arguments() {
        let arg = || Expr::new(ExprKind::Const(Const::Int(1)), Type::Int, 1);
        let program = program_assigning(Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Bytes),
                args: vec![arg(), arg(), arg()],
            },
            Type::Bytes,
            1,
        ));
        let error = crate::compile_object(&program).expect_err("three arguments must fail");
        assert!(
            error
                .message
                .contains("builtin bytes expects 0 to 2 arguments"),
            "{error}"
        );
    }

    #[test]
    fn builtin_ref_lowers_to_a_function_value() {
        let program = program_assigning(Expr::new(
            ExprKind::BuiltinRef(Builtin::Int),
            Type::Function,
            1,
        ));
        let object = crate::compile_object(&program)
            .expect("builtin reference must lower to pyx_builtin_function");
        assert!(!object.is_empty());
    }

    #[test]
    fn method_call_with_keywords_lowers_to_pyx_method_kw() {
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let count = Expr::new(ExprKind::Const(Const::Int(1)), Type::Int, 1);
        let program = program_assigning(Expr::new(
            ExprKind::MethodCall {
                object: Box::new(text("a-a")),
                method: "replace".to_string(),
                args: vec![text("a"), text("b")],
                keywords: vec![("count".to_string(), count)],
            },
            Type::Str,
            1,
        ));
        let object = crate::compile_object(&program).expect("keyword method call must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn method_call_without_keywords_still_lowers() {
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let program = program_assigning(Expr::new(
            ExprKind::MethodCall {
                object: Box::new(text("ab")),
                method: "upper".to_string(),
                args: Vec::new(),
                keywords: Vec::new(),
            },
            Type::Str,
            1,
        ));
        let object = crate::compile_object(&program).expect("plain method call must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn class_definition_and_setattr_emit_an_object_file() {
        let method = Function {
            name: "C.get".to_string(),
            params: vec![Param {
                local: 0,
                name: "self".to_string(),
                ty: Type::Any,
            }],
            capture_count: 0,
            ret_ty: Type::Any,
            locals: vec![LocalInfo {
                name: "self".to_string(),
                ty: Type::Any,
            }],
            body: vec![Stmt::new(
                StmtKind::Return(Some(Expr::new(ExprKind::Local(0), Type::Any, 1))),
                1,
            )],
        };
        let module_body = Function {
            name: "<module>".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![
                LocalInfo {
                    name: "C".to_string(),
                    ty: Type::Any,
                },
                LocalInfo {
                    name: "c".to_string(),
                    ty: Type::Any,
                },
            ],
            body: vec![
                Stmt::new(
                    StmtKind::ClassDef {
                        target: 0,
                        name: "C".to_string(),
                        base: None,
                        methods: vec![ClassMethod {
                            name: "get".to_string(),
                            function: 1,
                        }],
                    },
                    1,
                ),
                Stmt::new(
                    StmtKind::Assign {
                        target: 1,
                        value: Expr::new(
                            ExprKind::IndirectCall {
                                callee: Box::new(Expr::new(ExprKind::Local(0), Type::Any, 2)),
                                args: Vec::new(),
                            },
                            Type::Any,
                            2,
                        ),
                    },
                    2,
                ),
                Stmt::new(
                    StmtKind::SetAttr {
                        object: Expr::new(ExprKind::Local(1), Type::Any, 3),
                        name: "x".to_string(),
                        value: Expr::new(ExprKind::Const(Const::Int(7)), Type::Int, 3),
                    },
                    3,
                ),
            ],
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, method],
            main: 0,
        };
        let object = crate::compile_object(&program)
            .expect("class definition and attribute store must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn closure_definition_and_indirect_call_emit_an_object_file() {
        let add = |lhs: u32, rhs: u32| {
            Expr::new(
                ExprKind::Bin {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::new(ExprKind::Local(lhs), Type::Any, 4)),
                    rhs: Box::new(Expr::new(ExprKind::Local(rhs), Type::Any, 4)),
                },
                Type::Any,
                4,
            )
        };
        let inner = Function {
            name: "make.add".to_string(),
            params: vec![
                Param {
                    local: 0,
                    name: "x".to_string(),
                    ty: Type::Any,
                },
                Param {
                    local: 1,
                    name: "y".to_string(),
                    ty: Type::Any,
                },
            ],
            capture_count: 1,
            ret_ty: Type::Any,
            locals: vec![
                LocalInfo {
                    name: "x".to_string(),
                    ty: Type::Any,
                },
                LocalInfo {
                    name: "y".to_string(),
                    ty: Type::Any,
                },
            ],
            body: vec![Stmt::new(StmtKind::Return(Some(add(0, 1))), 4)],
        };
        let module_body = Function {
            name: "<module>".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![
                LocalInfo {
                    name: "x".to_string(),
                    ty: Type::Int,
                },
                LocalInfo {
                    name: "f".to_string(),
                    ty: Type::Any,
                },
            ],
            body: vec![
                Stmt::new(
                    StmtKind::Assign {
                        target: 0,
                        value: Expr::new(ExprKind::Const(Const::Int(2)), Type::Int, 1),
                    },
                    1,
                ),
                Stmt::new(
                    StmtKind::Closure {
                        target: 1,
                        function: 1,
                        captures: vec![0],
                    },
                    2,
                ),
                Stmt::new(
                    StmtKind::Expr(Expr::new(
                        ExprKind::IndirectCall {
                            callee: Box::new(Expr::new(ExprKind::Local(1), Type::Any, 3)),
                            args: vec![Expr::new(ExprKind::Const(Const::Int(3)), Type::Int, 3)],
                        },
                        Type::Any,
                        3,
                    )),
                    3,
                ),
            ],
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, inner],
            main: 0,
        };
        let object = crate::compile_object(&program).expect("closure definition must lower");
        assert!(!object.is_empty());
    }
}
