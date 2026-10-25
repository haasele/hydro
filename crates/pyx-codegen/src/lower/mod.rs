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
    Function, FunctionId, Handler, LocalId, LocalInfo, Param, ParamKind, Program, Stmt, StmtKind,
    Type, UnpackTarget,
};

use crate::CodegenError;

/// The `None` immediate, see `ARCHITECTURE.md` §3.1.
const NONE_TAG: i64 = 1;
const FALSE_TAG: i64 = 9;
const TRUE_TAG: i64 = 17;
/// Reserved special immediate for `NotImplemented` (special payload 3); see
/// `pyx_runtime::value::NOT_IMPLEMENTED`.
const NOT_IMPLEMENTED_TAG: i64 = 25;

/// Tagged-immediate integers are `payload << 3`; the payload is recovered with
/// an arithmetic shift right.
const INT_TAG_SHIFT: i64 = 3;

/// Payload bounds of a tagged immediate, mirroring `value::MIN_INT` and
/// `value::MAX_INT` in `pyx-runtime` (`i64::MIN >> 3` and `i64::MAX >> 3`, i.e.
/// `-2^60 ..= 2^60 - 1`). A result outside this range cannot be retagged and
/// must go through `pyx_binop`, which raises `OverflowError`.
const TAGGED_INT_MIN: i64 = i64::MIN >> INT_TAG_SHIFT;
const TAGGED_INT_MAX: i64 = i64::MAX >> INT_TAG_SHIFT;

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
    to_repr: FuncId,
    hash: FuncId,
    format: FuncId,
    to_int: FuncId,
    to_float: FuncId,
    abs: FuncId,
    all: FuncId,
    ord: FuncId,
    chr: FuncId,
    list_new: FuncId,
    list_append: FuncId,
    tuple_new: FuncId,
    set_new: FuncId,
    set_add: FuncId,
    set_from: FuncId,
    range_new: FuncId,
    list_from: FuncId,
    tuple_from: FuncId,
    dict_from: FuncId,
    function_new: FuncId,
    closure_new: FuncId,
    function_params: FuncId,
    coroutine_new: FuncId,
    generator_new: FuncId,
    await_value: FuncId,
    yield_value: FuncId,
    yield_from: FuncId,
    builtin_function: FuncId,
    call: FuncId,
    call_star: FuncId,
    deferred_call: FuncId,
    arity_error: FuncId,
    map: FuncId,
    filter: FuncId,
    sum: FuncId,
    enumerate: FuncId,
    zip: FuncId,
    unpack_seq: FuncId,
    unpack_star: FuncId,
    dict_new: FuncId,
    dict_set: FuncId,
    dict_merge: FuncId,
    subscript: FuncId,
    subscript_set: FuncId,
    slice: FuncId,
    slice_set: FuncId,
    module_call: FuncId,
    module_call_kw: FuncId,
    module_get: FuncId,
    module_value: FuncId,
    deferred_import: FuncId,
    global_get: FuncId,
    global_set: FuncId,
    extension_call: FuncId,
    extension_get: FuncId,
    getattr: FuncId,
    hasattr: FuncId,
    setattr: FuncId,
    class_new: FuncId,
    /// Hidden ABI: the cached native class object of a built-in exception.
    exception_class: FuncId,
    method: FuncId,
    method_kw: FuncId,
    method_star: FuncId,
    iter_new: FuncId,
    iter_next: FuncId,
    iter_of: FuncId,
    next_value: FuncId,
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
    raise_from: FuncId,
    raise_value_from: FuncId,
    exc_matches: FuncId,
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
            to_repr: declare_runtime(module, "pyx_to_repr", &[i64t], Some(i64t))?,
            hash: declare_runtime(module, "pyx_hash", &[i64t], Some(i64t))?,
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
            all: declare_runtime(module, "pyx_all", &[i64t], Some(i64t))?,
            ord: declare_runtime(module, "pyx_ord", &[i64t], Some(i64t))?,
            chr: declare_runtime(module, "pyx_chr", &[i64t], Some(i64t))?,
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
            tuple_from: declare_runtime(module, "pyx_tuple_from", &[i64t], Some(i64t))?,
            dict_from: declare_runtime(module, "pyx_dict_from", &[i64t], Some(i64t))?,
            // `wrapper` is an `i64` function pointer in the ABI; `arity` and
            // `name_len` are `uint64_t`/`size_t`: pointer-width. `flags` is a
            // `uint32_t` (bit0 async, bit1 generator).
            function_new: declare_runtime(
                module,
                "pyx_function_new",
                &[i64t, ptr, ptr, ptr, i32t],
                Some(i64t),
            )?,
            // `wrapper` is an `i64`; `arity`, `name_len` and `capture_count`
            // are `size_t`, `captures` is a pointer to tagged values and
            // `flags` is a `uint32_t` (bit0 async, bit1 generator).
            closure_new: declare_runtime(
                module,
                "pyx_closure_new",
                &[i64t, ptr, ptr, ptr, ptr, ptr, i32t],
                Some(i64t),
            )?,
            // `function`, the parameter count and a pointer to `2 * count`
            // words (name pointer, name length) of user-visible parameter
            // names. Returns the function value unchanged.
            function_params: declare_runtime(
                module,
                "pyx_function_params",
                &[i64t, ptr, ptr],
                Some(i64t),
            )?,
            // `entry` is the `__pyx_wrap_N` trampoline address passed as an
            // `i64` (like `wrapper` above); `argc` is `size_t` and `argv`
            // points to the tagged arguments.
            coroutine_new: declare_runtime(
                module,
                "pyx_coroutine_new",
                &[i64t, ptr, ptr],
                Some(i64t),
            )?,
            generator_new: declare_runtime(
                module,
                "pyx_generator_new",
                &[i64t, ptr, ptr],
                Some(i64t),
            )?,
            // `pyx_await` takes the awaitable and returns its result;
            // `pyx_yield` takes the yielded value and returns the value sent
            // on resume; `pyx_yield_from` delegates iteration to an iterable
            // and returns its result.
            await_value: declare_runtime(module, "pyx_await", &[i64t], Some(i64t))?,
            yield_value: declare_runtime(module, "pyx_yield", &[i64t], Some(i64t))?,
            yield_from: declare_runtime(module, "pyx_yield_from", &[i64t], Some(i64t))?,
            // `name_len` is a `size_t` in the ABI: pointer-width.
            builtin_function: declare_runtime(
                module,
                "pyx_builtin_function",
                &[ptr, ptr],
                Some(i64t),
            )?,
            call: declare_runtime(module, "pyx_call", &[i64t, ptr, ptr], Some(i64t))?,
            // Generic starred call: `fn(args, *star)`. `argc`/`argv` are the
            // leading positional operands; `star` is the tagged list/tuple to
            // flatten after them.
            call_star: declare_runtime(
                module,
                "pyx_call_star",
                &[i64t, ptr, ptr, i64t],
                Some(i64t),
            )?,
            // `argc` is `size_t`/pointer-width; the helper only raises.
            deferred_call: declare_runtime(module, "pyx_deferred_call", &[ptr, ptr], Some(i64t))?,
            // `(expected, actual)` as `i64`; the wrapper passes its bound
            // positional maximum and the supplied count.
            arity_error: declare_runtime(module, "pyx_arity_error", &[i64t, i64t], Some(i64t))?,
            map: declare_runtime(module, "pyx_map", &[i64t, i64t], Some(i64t))?,
            filter: declare_runtime(module, "pyx_filter", &[i64t, i64t], Some(i64t))?,
            sum: declare_runtime(module, "pyx_sum", &[i64t, i64t], Some(i64t))?,
            enumerate: declare_runtime(module, "pyx_enumerate", &[i64t, i64t], Some(i64t))?,
            // `argc` is `uint64_t` in the ABI: pointer-width.
            zip: declare_runtime(module, "pyx_zip", &[ptr, ptr], Some(i64t))?,
            // `expected` is a `size_t` in the ABI: pointer-width. Returns the
            // sequence being unpacked (the input itself for strings, bytes,
            // lists and tuples; a fresh list for other iterables).
            unpack_seq: declare_runtime(module, "pyx_unpack_seq", &[i64t, ptr], Some(i64t))?,
            // `value`, the number of targets before and after the starred one.
            // Returns a fresh list whose starred slot holds the middle list.
            unpack_star: declare_runtime(module, "pyx_unpack_star", &[i64t, ptr, ptr], Some(i64t))?,
            dict_new: declare_runtime(module, "pyx_dict_new", &[ptr, ptr, ptr], Some(i64t))?,
            dict_set: declare_runtime(module, "pyx_dict_set", &[i64t, i64t, i64t], Some(i64t))?,
            dict_merge: declare_runtime(module, "pyx_dict_merge", &[i64t, i64t], Some(i64t))?,
            subscript: declare_runtime(module, "pyx_subscript", &[i64t, i64t], Some(i64t))?,
            subscript_set: declare_runtime(
                module,
                "pyx_subscript_set",
                &[i64t, i64t, i64t],
                Some(i64t),
            )?,
            slice: declare_runtime(module, "pyx_slice", &[i64t, i64t, i64t, i64t], Some(i64t))?,
            // `target[start:stop:step] = value`; bounds and value are tagged
            // values, omitted bounds are `None`. Returns the runtime `None`;
            // errors are reported through the pending exception.
            slice_set: declare_runtime(
                module,
                "pyx_slice_set",
                &[i64t, i64t, i64t, i64t, i64t],
                Some(i64t),
            )?,
            // `module_len`, `function_len` and `argc` are `size_t`/`uint64_t`
            // in the ABI: pointer-width.
            module_call: declare_runtime(
                module,
                "pyx_module_call",
                &[ptr, ptr, ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // Same prefix as `pyx_module_call`, plus `kwargc` and `kw_argv`
            // (`2 * kwargc` tagged values: name, value, ...).
            module_call_kw: declare_runtime(
                module,
                "pyx_module_call_kw",
                &[ptr, ptr, ptr, ptr, ptr, ptr, ptr, ptr],
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
            module_value: declare_runtime(module, "pyx_module_value", &[ptr, ptr], Some(i64t))?,
            // Module name and optional member, each a pointer/length pair. The
            // helper always raises, so it never binds a real value.
            deferred_import: declare_runtime(
                module,
                "pyx_deferred_import",
                &[ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // Module name plus global name, each a pointer/length pair.
            global_get: declare_runtime(
                module,
                "pyx_global_get",
                &[ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            global_set: declare_runtime(
                module,
                "pyx_global_set",
                &[ptr, ptr, ptr, ptr, i64t],
                Some(i64t),
            )?,
            // Compiled CPython extension member: `path_len`, `member_len` and
            // `argc` are `size_t`/`usize` in the ABI: pointer-width. `argv`
            // points to `argc` tagged values and `path` is the shared-object
            // file name (not `init_name`).
            extension_call: declare_runtime(
                module,
                "pyx_extension_call",
                &[ptr, ptr, ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // Same `path`/`member` (ptr+len) pair as `extension_call`.
            extension_get: declare_runtime(
                module,
                "pyx_extension_get",
                &[ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // `name_len` is a `size_t` in the ABI: pointer-width.
            getattr: declare_runtime(module, "pyx_getattr", &[i64t, ptr, ptr], Some(i64t))?,
            hasattr: declare_runtime(module, "pyx_hasattr", &[i64t, ptr, ptr], Some(i64t))?,
            // `name_len` is a `size_t` in the ABI: pointer-width.
            setattr: declare_runtime(module, "pyx_setattr", &[i64t, ptr, ptr, i64t], Some(i64t))?,
            // `name_len`, `method_count` and `attr_count` are `usize` in the
            // ABI: pointer-width. `names_ptr`/`funcs_ptr`/`kinds_ptr` hold the
            // method names, function values and raw binding-kind tags;
            // `attr_names_ptr`/`attr_values_ptr` hold class attribute
            // name/value pairs.
            class_new: declare_runtime(
                module,
                "pyx_class_new",
                &[ptr, ptr, i64t, ptr, ptr, ptr, ptr, ptr, ptr, ptr],
                Some(i64t),
            )?,
            // `tag` is a `uint32_t`; the runtime returns the cached native
            // class object for that built-in exception.
            exception_class: declare_runtime(module, "pyx_exception_class", &[i32t], Some(i64t))?,
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
            // `name_len` and `kwargc` are pointer-width; `star` is the tagged
            // operand to flatten, `kw_argv` holds `[name, value]` pairs.
            method_star: declare_runtime(
                module,
                "pyx_method_star",
                &[i64t, ptr, ptr, i64t, ptr, ptr],
                Some(i64t),
            )?,
            iter_new: declare_runtime(module, "pyx_iter_new", &[i64t], Some(i64t))?,
            iter_next: declare_runtime(module, "pyx_iter_next", &[i64t, ptr], Some(i64t))?,
            iter_of: declare_runtime(module, "pyx_iter", &[i64t], Some(i64t))?,
            // `has_default` is a plain `i64` flag (1/0).
            next_value: declare_runtime(module, "pyx_next", &[i64t, i64t, i64t], Some(i64t))?,
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
            // Explicit chaining. Both arguments are tagged values; a `None`
            // cause stands for `from None` (no chained traceback).
            raise_from: declare_runtime(module, "pyx_raise_from", &[i32t, i64t, i64t], Some(i64t))?,
            raise_value_from: declare_runtime(
                module,
                "pyx_raise_value_from",
                &[i64t, i64t],
                Some(i64t),
            )?,
            // Exception-hierarchy test used by `except Exception` /
            // `except BaseException`; returns 1 when the pending exception
            // matches the encoded hierarchy kind.
            exc_matches: declare_runtime(module, "pyx_exc_matches", &[i32t], Some(i32t))?,
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
            function,
            &runtime,
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
/// concrete `__pyx_fn_N` symbol. Functions with a dynamic signature (`*args`,
/// `**kwargs` or keyword-only parameters) receive their total argument count
/// as a tagged value at `argv[0]`; the wrapper then binds literal defaults,
/// packs the remaining positional arguments into the `*args` list and creates
/// the `**kwargs` dict before the call.
fn define_wrapper<M: Module>(
    module: &mut M,
    index: FunctionId,
    function: &Function,
    runtime: &Runtime,
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
        let mut args = Vec::with_capacity(function.params.len());
        if function.has_dynamic_signature() {
            bind_dynamic_arguments(
                module,
                &mut builder,
                ptr_ty,
                function,
                runtime,
                index,
                argv,
                &mut args,
            )?;
        } else {
            for position in 0..function.params.len() {
                let offset = wrapper_arg_offset(index, position)?;
                let value = builder
                    .ins()
                    .load(types::I64, MemFlags::new(), argv, offset);
                args.push(value);
            }
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

/// Byte offset of argument slot `position` in a wrapper's `argv` buffer.
fn wrapper_arg_offset(index: FunctionId, position: usize) -> Result<i32, CodegenError> {
    position
        .checked_mul(8)
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or_else(|| {
            CodegenError::new(format!(
                "internal error: wrapper {index} argument offset out of range"
            ))
        })
}

/// Reinterpret an `i64` as a pointer-width integer (the ABI uses `usize` for
/// lengths).
fn as_pointer_width(builder: &mut FunctionBuilder, ptr_ty: ir::Type, value: Value) -> Value {
    if ptr_ty == types::I64 {
        value
    } else {
        builder.ins().ireduce(ptr_ty, value)
    }
}

/// Materialise an IR [`Const`] in a wrapper body.
fn wrapper_const<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    ptr_ty: ir::Type,
    runtime: &Runtime,
    value: &Const,
) -> Result<Value, CodegenError> {
    Ok(match value {
        Const::Int(value) => builder.ins().iconst(types::I64, value.wrapping_shl(3)),
        Const::Bool(true) => builder.ins().iconst(types::I64, TRUE_TAG),
        Const::Bool(false) => builder.ins().iconst(types::I64, FALSE_TAG),
        Const::None => builder.ins().iconst(types::I64, NONE_TAG),
        Const::NotImplemented => builder.ins().iconst(types::I64, NOT_IMPLEMENTED_TAG),
        Const::Float(value) => {
            let float = builder.ins().f64const(*value);
            call_val_extern(module, builder, runtime.float_new, &[float])
        }
        Const::Str(text) => {
            let (slot, len) = make_bytes_slot(builder, text.as_bytes())?;
            let ptr = builder.ins().stack_addr(ptr_ty, slot, 0);
            let len_value = builder.ins().iconst(types::I64, len as i64);
            let len = as_pointer_width(builder, ptr_ty, len_value);
            call_val_extern(module, builder, runtime.str_new, &[ptr, len])
        }
        Const::Bytes(bytes) => {
            let (slot, len) = make_bytes_slot(builder, bytes)?;
            let ptr = builder.ins().stack_addr(ptr_ty, slot, 0);
            let len_value = builder.ins().iconst(types::I64, len as i64);
            let len = as_pointer_width(builder, ptr_ty, len_value);
            call_val_extern(module, builder, runtime.bytes_new, &[ptr, len])
        }
    })
}

/// Bind the arguments of a dynamic signature in a generated wrapper.
///
/// The buffer layout is `[tagged total count, captures..., user arguments...]`.
/// The wrapper fills the concrete function's parameter slots in order:
/// captures, positional parameters (literal defaults replace missing values),
/// the `*args` list, keyword-only parameters (their defaults) and the
/// `**kwargs` dict.
#[allow(clippy::too_many_arguments)]
fn bind_dynamic_arguments<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    ptr_ty: ir::Type,
    function: &Function,
    runtime: &Runtime,
    index: FunctionId,
    argv: Value,
    args: &mut Vec<Value>,
) -> Result<(), CodegenError> {
    let total = builder.ins().load(types::I64, MemFlags::new(), argv, 0);
    let total = builder.ins().sshr_imm(total, INT_TAG_SHIFT);
    let capture_count = function.capture_count;
    let user_count = builder.ins().iadd_imm(total, -(capture_count as i64));
    let positional_end = function.positional_end();
    let fixed = positional_end.saturating_sub(capture_count);

    // Without a `*args` slot, extra positional arguments cannot be bound.
    // The runtime checked the minimum arity before entering the wrapper, so
    // only the maximum is validated here.
    if function.vararg().is_none() {
        let raise_block = builder.create_block();
        let continue_block = builder.create_block();
        let too_many = builder
            .ins()
            .icmp_imm(IntCC::SignedGreaterThan, user_count, fixed as i64);
        builder
            .ins()
            .brif(too_many, raise_block, &[], continue_block, &[]);
        builder.switch_to_block(raise_block);
        let expected = builder.ins().iconst(types::I64, fixed as i64);
        let result = call_val_extern(
            module,
            builder,
            runtime.arity_error,
            &[expected, user_count],
        );
        builder.ins().return_(&[result]);
        builder.switch_to_block(continue_block);
    }

    for position in 0..capture_count {
        let offset = wrapper_arg_offset(index, position + 1)?;
        args.push(
            builder
                .ins()
                .load(types::I64, MemFlags::new(), argv, offset),
        );
    }

    for position in 0..fixed {
        let offset = wrapper_arg_offset(index, position + 1 + capture_count)?;
        let loaded = builder
            .ins()
            .load(types::I64, MemFlags::new(), argv, offset);
        let value = match &function.params[capture_count + position].default {
            Some(default) => {
                let fallback = wrapper_const(module, builder, ptr_ty, runtime, default)?;
                let present =
                    builder
                        .ins()
                        .icmp_imm(IntCC::SignedGreaterThan, user_count, position as i64);
                builder.ins().select(present, loaded, fallback)
            }
            None => loaded,
        };
        args.push(value);
    }

    if function.vararg().is_some() {
        let extra = builder.ins().iadd_imm(user_count, -(fixed as i64));
        let count = if fixed == 0 {
            user_count
        } else {
            let has_extra =
                builder
                    .ins()
                    .icmp_imm(IntCC::SignedGreaterThan, user_count, fixed as i64 - 1);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().select(has_extra, extra, zero)
        };
        let count = as_pointer_width(builder, ptr_ty, count);
        let first_extra = wrapper_arg_offset(index, 1 + capture_count + fixed)?;
        let extras = builder.ins().iadd_imm(argv, i64::from(first_extra));
        let list = call_val_extern(module, builder, runtime.list_new, &[count, extras]);
        args.push(list);
    }

    for param in function.kwonly_params() {
        let value = match &param.default {
            Some(default) => wrapper_const(module, builder, ptr_ty, runtime, default)?,
            None => builder.ins().iconst(types::I64, NONE_TAG),
        };
        args.push(value);
    }

    if function.kwarg().is_some() {
        let null = builder.ins().iconst(ptr_ty, 0);
        let dict = call_val_extern(module, builder, runtime.dict_new, &[null, null, null]);
        args.push(dict);
    }

    if args.len() != function.params.len() {
        return Err(CodegenError::new(format!(
            "internal error: wrapper {index} bound {} of {} parameters",
            args.len(),
            function.params.len()
        )));
    }
    Ok(())
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

    /// Encode the function kind as the runtime's `flags` word: bit0 marks an
    /// `async def`, bit1 marks a generator. Both the coroutine and the
    /// generator object are created from this flag word by the function
    /// wrappers.
    fn function_flags(&mut self, function: &Function) -> Value {
        let flags = u32::from(function.is_async)
            | (u32::from(function.is_generator) << 1)
            | (u32::from(function.has_dynamic_signature()) << 2);
        self.builder.ins().iconst(types::I32, i64::from(flags))
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
            StmtKind::GlobalSet {
                module,
                name,
                value,
            } => {
                // The result is `None` and is never observed.
                self.lower_global_set(module, name, value)?;
            }
            StmtKind::AugAssign { target, op, value } => {
                let var = self.local_var(*target)?;
                let value_ty = value.ty;
                // Python evaluates the right-hand side before the read of the
                // target; for a local the read has no side effects, but the
                // value expression is still lowered exactly once here.
                let rhs = self.lower_expr(value)?;
                let lhs = self.builder.use_var(var);
                let target_ty = self.locals.get(*target as usize).map(|info| info.ty);
                // `i += 1` is `i = i + 1`: with both sides statically `int`
                // and an arithmetic operator the checked tagged-int path is
                // exact; anything else keeps the dynamic operator.
                let result = if target_ty == Some(Type::Int)
                    && value_ty == Type::Int
                    && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                {
                    self.lower_int_arith(*op, lhs, rhs)?
                } else {
                    self.lower_binop(*op, lhs, rhs)?
                };
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
            StmtKind::SetSlice {
                target,
                start,
                stop,
                step,
                value,
            } => {
                // Python evaluates the right-hand side first, then the target
                // expression, then the bounds in source order. Omitted bounds
                // arrive as `Const::None`, which lowers to the runtime `None`
                // sentinel.
                let value = self.lower_expr(value)?;
                let object = self.lower_expr(target)?;
                let start = self.lower_expr(start)?;
                let stop = self.lower_expr(stop)?;
                let step = self.lower_expr(step)?;
                let id = self.runtime.slice_set;
                let _ = self.call_rt(id, &[object, start, stop, step, value])?;
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
                let user_arity = func.call_arity();
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
                let flags = self.function_flags(func);
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
                        flags,
                    ],
                )?;
                self.emit_exc_check()?;
                let closure = self.attach_function_params(closure, func)?;
                let var = self.local_var(*target)?;
                self.builder.def_var(var, closure);
            }
            StmtKind::ClassDef {
                target,
                name,
                base,
                methods,
                attributes,
            } => {
                // Python evaluates the base expression first, then executes the
                // class body (creating methods and evaluating attributes).
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
                let kinds_slot = self.stack_array_slot(methods.len(), "class method kinds")?;
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

                    let kind = self
                        .builder
                        .ins()
                        .iconst(self.ptr_ty, method.kind.abi_tag());
                    self.builder
                        .ins()
                        .stack_store(kind, kinds_slot, (index * 8) as i32);
                }

                let attr_names_slot =
                    self.stack_array_slot(attributes.len(), "class attribute names")?;
                let attr_values_slot =
                    self.stack_array_slot(attributes.len(), "class attribute values")?;
                for (index, attribute) in attributes.iter().enumerate() {
                    let (name_slot, name_len) =
                        make_bytes_slot(&mut self.builder, attribute.name.as_bytes())?;
                    let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
                    let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
                    let id = self.runtime.str_new;
                    let attr_name = self.call_rt(id, &[name_ptr, name_len])?;
                    self.emit_exc_check()?;
                    self.builder
                        .ins()
                        .stack_store(attr_name, attr_names_slot, (index * 8) as i32);

                    let value = self.lower_expr(&attribute.value)?;
                    self.builder
                        .ins()
                        .stack_store(value, attr_values_slot, (index * 8) as i32);
                }

                let (class_slot, class_len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
                let class_ptr = self.builder.ins().stack_addr(self.ptr_ty, class_slot, 0);
                let class_len = self.builder.ins().iconst(self.ptr_ty, class_len as i64);
                let count = self.builder.ins().iconst(self.ptr_ty, methods.len() as i64);
                let names_ptr = self.builder.ins().stack_addr(self.ptr_ty, names_slot, 0);
                let funcs_ptr = self.builder.ins().stack_addr(self.ptr_ty, funcs_slot, 0);
                let kinds_ptr = self.builder.ins().stack_addr(self.ptr_ty, kinds_slot, 0);
                let attr_count = self
                    .builder
                    .ins()
                    .iconst(self.ptr_ty, attributes.len() as i64);
                let attr_names_ptr = self
                    .builder
                    .ins()
                    .stack_addr(self.ptr_ty, attr_names_slot, 0);
                let attr_values_ptr =
                    self.builder
                        .ins()
                        .stack_addr(self.ptr_ty, attr_values_slot, 0);
                let base = self.builder.use_var(base_holder);
                let id = self.runtime.class_new;
                let class = self.call_rt(
                    id,
                    &[
                        class_ptr,
                        class_len,
                        base,
                        count,
                        names_ptr,
                        funcs_ptr,
                        kinds_ptr,
                        attr_count,
                        attr_names_ptr,
                        attr_values_ptr,
                    ],
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
                let value = self.lower_expr(value)?;
                self.bind_unpack(targets.as_slice(), value)?;
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
            StmtKind::Yield { value } => {
                let value = match value {
                    Some(expr) => self.lower_expr(expr)?,
                    None => self.none(),
                };
                // Statement form: the value sent on resume is discarded.
                // `yield` in an expression position uses `ExprKind::Yield`
                // instead, which keeps the sent value.
                let id = self.runtime.yield_value;
                self.call_rt(id, &[value])?;
                self.emit_exc_check()?;
            }
            StmtKind::Raise { exc, msg, cause } => {
                self.lower_raise(*exc, msg.as_ref(), cause.as_ref())?;
            }
            StmtKind::Assert { test, msg } => {
                let condition = self.lower_expr(test)?;
                let passed = self.truthy_cond(condition)?;
                let failure = self.builder.create_block();
                let after = self.builder.create_block();
                self.builder.ins().brif(passed, after, &[], failure, &[]);
                self.switch_to(failure);
                // The message is evaluated only on failure and becomes the
                // `AssertionError` message (`assert x` has none).
                self.lower_raise(Some(ExcKind::AssertionError), msg.as_ref(), None)?;
                self.switch_to(after);
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
        // When the loop variable and every `range()` bound are statically
        // `int`, the counter stays a tagged int and the backedge increment can
        // use the checked fast path. Any other typing keeps the dynamic
        // operator.
        let int_counter = self.locals.get(var as usize).map(|info| info.ty) == Some(Type::Int)
            && start.ty == Type::Int
            && stop.ty == Type::Int
            && step.is_none_or(|expr| expr.ty == Type::Int);

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

        // Choose the comparison direction based on the sign of the step. A
        // fully int loop compares tagged integers directly and branches on the
        // sentinel, with no runtime operator or truthiness call.
        let step_current = self.builder.use_var(step_var);
        let zero = self.tagged_int(0);
        let positive_step = if int_counter {
            self.lower_int_comparison(BinOp::Gt, step_current, zero)?
        } else {
            self.lower_binop(BinOp::Gt, step_current, zero)?
        };
        let direction = if int_counter {
            self.bool_cond(positive_step)
        } else {
            self.truthy_cond(positive_step)?
        };
        self.builder
            .ins()
            .brif(direction, positive, &[], negative, &[]);

        self.switch_to(positive);
        let current = self.builder.use_var(local);
        let limit = self.builder.use_var(stop_var);
        let comparison = if int_counter {
            self.lower_int_comparison(BinOp::Lt, current, limit)?
        } else {
            self.lower_binop(BinOp::Lt, current, limit)?
        };
        let branch = if int_counter {
            self.bool_cond(comparison)
        } else {
            self.truthy_cond(comparison)?
        };
        self.builder.ins().brif(branch, body_block, &[], after, &[]);

        self.switch_to(negative);
        let current = self.builder.use_var(local);
        let limit = self.builder.use_var(stop_var);
        let comparison = if int_counter {
            self.lower_int_comparison(BinOp::Gt, current, limit)?
        } else {
            self.lower_binop(BinOp::Gt, current, limit)?
        };
        let branch = if int_counter {
            self.bool_cond(comparison)
        } else {
            self.truthy_cond(comparison)?
        };
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
        let next = if int_counter {
            self.lower_int_arith(BinOp::Add, current, step_current)?
        } else {
            self.lower_binop(BinOp::Add, current, step_current)?
        };
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
                self.bind_unpack(targets.as_slice(), value)?;
            }
        }
        Ok(())
    }

    /// Materialise and validate `value` against `targets`, then bind the
    /// extracted elements left to right.
    ///
    /// The source is evaluated by the caller (before this runs) and parked in
    /// a variable so it stays a GC root while `pyx_unpack_seq` may allocate.
    /// For lists, tuples, strings and bytes the runtime returns the source
    /// itself; other iterables (including existing iterators and generators)
    /// become a fresh list, which the holder roots for the element reads.
    fn bind_unpack(
        &mut self,
        targets: &'a [UnpackTarget],
        value: Value,
    ) -> Result<(), CodegenError> {
        let holder = self.temp_var();
        self.builder.def_var(holder, value);
        // One `*target` collects the elements no plain target consumed; the
        // runtime returns a fresh list whose starred slot is that middle list,
        // so the element reads stay identical to the exact-length case.
        let sequence = match targets
            .iter()
            .position(|target| matches!(target, UnpackTarget::Star(_)))
        {
            Some(star) => {
                let before = self.builder.ins().iconst(self.ptr_ty, star as i64);
                let after = self
                    .builder
                    .ins()
                    .iconst(self.ptr_ty, (targets.len() - star - 1) as i64);
                self.call_rt(self.runtime.unpack_star, &[value, before, after])?
            }
            None => {
                let expected = self.builder.ins().iconst(self.ptr_ty, targets.len() as i64);
                self.call_rt(self.runtime.unpack_seq, &[value, expected])?
            }
        };
        self.emit_exc_check()?;
        self.builder.def_var(holder, sequence);
        for (index, target) in targets.iter().enumerate() {
            let sequence = self.builder.use_var(holder);
            let index = self.tagged_int((index as i64).wrapping_shl(3));
            let item = self.call_rt(self.runtime.subscript, &[sequence, index])?;
            self.emit_exc_check()?;
            self.bind_unpack_target(target, item)?;
        }
        Ok(())
    }

    /// Bind one already-extracted element to a target. Nested targets recurse;
    /// attribute and subscript receivers are evaluated here, after the source
    /// and after every earlier target, matching Python's store order.
    fn bind_unpack_target(
        &mut self,
        target: &'a UnpackTarget,
        value: Value,
    ) -> Result<(), CodegenError> {
        match target {
            UnpackTarget::Name(id) => {
                let var = self.local_var(*id)?;
                self.builder.def_var(var, value);
            }
            UnpackTarget::Attr { object, name } => {
                // Park the element while the receiver expression runs: the
                // calls it may make can trigger a collection or raise.
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
                let _ = self.call_rt(self.runtime.setattr, &[object, name_ptr, name_len, value])?;
                self.emit_exc_check()?;
            }
            UnpackTarget::Subscript { object, index } => {
                // Python evaluates the receiver, then the index, both after the
                // element was extracted.
                let value_holder = self.temp_var();
                self.builder.def_var(value_holder, value);
                let object = self.lower_expr(object)?;
                let object_holder = self.temp_var();
                self.builder.def_var(object_holder, object);
                let index = self.lower_expr(index)?;
                let index_holder = self.temp_var();
                self.builder.def_var(index_holder, index);

                let object = self.builder.use_var(object_holder);
                let index = self.builder.use_var(index_holder);
                let value = self.builder.use_var(value_holder);
                let _ = self.call_rt(self.runtime.subscript_set, &[object, index, value])?;
                self.emit_exc_check()?;
            }
            UnpackTarget::Nested(targets) => {
                self.bind_unpack(targets.as_slice(), value)?;
            }
            // The element is the fresh middle list; bind it like any other
            // value (the frontend only allows a name, attribute or subscript).
            UnpackTarget::Star(target) => {
                self.bind_unpack_target(target, value)?;
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
                        let matches = match kind {
                            // `Exception` matches every concrete kind except
                            // `KeyboardInterrupt`/`GeneratorExit`, and
                            // `BaseException` matches everything. `OSError`
                            // and its `FileNotFoundError` subclass match each
                            // other. The runtime owns that hierarchy so
                            // codegen only asks it.
                            ExcKind::Exception
                            | ExcKind::BaseException
                            | ExcKind::OSError
                            | ExcKind::FileNotFoundError => {
                                let tag = self.builder.ins().iconst(types::I32, kind.tag() as i64);
                                let id = self.runtime.exc_matches;
                                let caught = self.call_rt(id, &[tag])?;
                                self.builder.ins().icmp_imm(IntCC::NotEqual, caught, 0)
                            }
                            _ => {
                                let tag_id = self.runtime.exc_tag;
                                let tag = self.call_rt(tag_id, &[])?;
                                self.builder
                                    .ins()
                                    .icmp_imm(IntCC::Equal, tag, kind.tag() as i64)
                            }
                        };
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
        cause: Option<&'a Expr>,
    ) -> Result<(), CodegenError> {
        match exc {
            Some(kind) => {
                // Python evaluates the exception expression before the
                // `from` cause; an exception raised while evaluating either
                // one propagates through the normal exception path.
                let message = match msg {
                    Some(expr) => self.lower_expr(expr)?,
                    None => self.none(),
                };
                let tag = self.builder.ins().iconst(types::I32, kind.tag() as i64);
                match cause {
                    Some(expr) => {
                        let cause = self.lower_expr(expr)?;
                        let id = self.runtime.raise_from;
                        self.call_rt(id, &[tag, message, cause])?;
                    }
                    None => {
                        let id = self.runtime.raise;
                        self.call_rt(id, &[tag, message])?;
                    }
                }
                let target = self.exc_target;
                self.emit_jump_exc_target(target)?;
            }
            None => {
                if let Some(expr) = msg {
                    // `raise <value>`: the expression evaluates to an
                    // exception object. As with a kind/message raise, the
                    // explicit cause is evaluated after the exception value.
                    let value = self.lower_expr(expr)?;
                    match cause {
                        Some(expr) => {
                            let cause = self.lower_expr(expr)?;
                            let id = self.runtime.raise_value_from;
                            self.call_rt(id, &[value, cause])?;
                        }
                        None => {
                            let id = self.runtime.raise_value;
                            self.call_rt(id, &[value])?;
                        }
                    }
                    let target = self.exc_target;
                    self.emit_jump_exc_target(target)?;
                    return Ok(());
                }
                if cause.is_some() {
                    return Err(CodegenError::new(
                        "invalid IR: bare raise cannot have an explicit cause",
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
            ExprKind::NamedExpr { target, value } => {
                // The value is evaluated first, then stored in the local; the
                // local is a GC root via `declare_var`, so the value stays
                // reachable while it is also the expression result.
                let var = self.local_var(*target)?;
                let value = self.lower_expr(value)?;
                self.builder.def_var(var, value);
                Ok(value)
            }
            ExprKind::Bin { op, lhs, rhs } => {
                // Both operands are evaluated left to right before the
                // operation itself, as in Python.
                let lhs_ty = lhs.ty;
                let rhs_ty = rhs.ty;
                let lhs = self.lower_expr(lhs)?;
                let rhs = self.lower_expr(rhs)?;
                if lhs_ty == Type::Int && rhs_ty == Type::Int {
                    match op {
                        // The tagged-int fast path is only exact when the
                        // result stays an `int`; other results (e.g. `/`)
                        // go through the runtime operator.
                        BinOp::Add | BinOp::Sub | BinOp::Mul if expr.ty == Type::Int => {
                            return self.lower_int_arith(*op, lhs, rhs);
                        }
                        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                            return self.lower_int_comparison(*op, lhs, rhs)
                        }
                        _ => {}
                    }
                }
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
            ExprKind::IfExp { test, then, orelse } => self.lower_ifexp(test, then, orelse),
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
                keywords,
            } => self.lower_module_call(module, function, args, keywords),
            ExprKind::ModuleRef { module } => self.lower_module_ref(module),
            ExprKind::DeferredImport { module, member } => {
                self.lower_deferred_import(module, member.as_deref())
            }
            ExprKind::GlobalGet { module, name } => self.lower_global_get(module, name),
            ExprKind::ModuleAttr { module, name } => self.lower_module_attr(module, name),
            ExprKind::ExtensionAttr { path, member } => self.lower_extension_attr(path, member),
            ExprKind::GetAttr { object, name } => self.lower_getattr(object, name),
            ExprKind::MethodCall {
                object,
                method,
                args,
                star,
                keywords,
            } => self.lower_method_call(object, method, args, star.as_deref(), keywords),
            ExprKind::FunctionRef(id) => self.lower_function_ref(*id),
            ExprKind::BuiltinRef(builtin) => self.lower_builtin_ref(*builtin),
            ExprKind::ExceptionClass(kind) => {
                let tag = self.builder.ins().iconst(types::I32, kind.tag() as i64);
                let id = self.runtime.exception_class;
                let class = self.call_rt(id, &[tag])?;
                self.emit_exc_check()?;
                Ok(class)
            }
            ExprKind::IndirectCall { callee, args, star } => {
                self.lower_indirect_call(callee, args, star.as_deref())
            }
            ExprKind::Call { callee, args } => self.lower_call(callee, args),
            ExprKind::DeferredCall { callee, args } => {
                self.lower_deferred_call(callee.as_deref(), args)
            }
            ExprKind::Await { value } => {
                let value = self.lower_expr(value)?;
                let id = self.runtime.await_value;
                let result = self.call_rt(id, &[value])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            ExprKind::Yield { value } => {
                let value = match value {
                    Some(expr) => self.lower_expr(expr)?,
                    None => self.none(),
                };
                // Returns the value passed to the resume that continues the
                // generator (`send(value)`, or `None` for `next`).
                let id = self.runtime.yield_value;
                let sent = self.call_rt(id, &[value])?;
                self.emit_exc_check()?;
                Ok(sent)
            }
            ExprKind::YieldFrom { value } => {
                let value = self.lower_expr(value)?;
                // Delegates iteration; the result is the delegate's return
                // value (or `None` for a plain iterator).
                let id = self.runtime.yield_from;
                let result = self.call_rt(id, &[value])?;
                self.emit_exc_check()?;
                Ok(result)
            }
        }
    }

    fn lower_binop(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Value, CodegenError> {
        let code = self.builder.ins().iconst(types::I32, op.code() as i64);
        let id = self.runtime.binop;
        let result = self.call_rt(id, &[code, lhs, rhs])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Recover the payload of a tagged integer (`v >> 3`, arithmetic shift).
    fn untag_int(&mut self, value: Value) -> Value {
        self.builder.ins().sshr_imm(value, INT_TAG_SHIFT)
    }

    /// `int op int` for `+`, `-` and `*`.
    ///
    /// Both operands are known to be `int` and the result is `int`, so the
    /// operation cannot dispatch on types or raise anything but
    /// `OverflowError`. The fast path untags, computes with the checked
    /// machine operation, verifies the payload still fits the 61-bit tagged
    /// immediate and retags. Any overflow or out-of-range result branches to
    /// the runtime operator, which raises the exact `OverflowError`.
    fn lower_int_arith(
        &mut self,
        op: BinOp,
        lhs: Value,
        rhs: Value,
    ) -> Result<Value, CodegenError> {
        let a = self.untag_int(lhs);
        let b = self.untag_int(rhs);
        let (result, overflowed) = match op {
            BinOp::Add => self.builder.ins().sadd_overflow(a, b),
            BinOp::Sub => self.builder.ins().ssub_overflow(a, b),
            BinOp::Mul => self.builder.ins().smul_overflow(a, b),
            _ => {
                return Err(CodegenError::new(
                    "internal error: non-arithmetic operator on the int fast path",
                ));
            }
        };

        let no_i64_overflow = self.builder.ins().icmp_imm(IntCC::Equal, overflowed, 0);
        let above_min =
            self.builder
                .ins()
                .icmp_imm(IntCC::SignedGreaterThanOrEqual, result, TAGGED_INT_MIN);
        let below_max =
            self.builder
                .ins()
                .icmp_imm(IntCC::SignedLessThanOrEqual, result, TAGGED_INT_MAX);
        let tagged_range = self.builder.ins().band(above_min, below_max);
        let fast = self.builder.ins().band(no_i64_overflow, tagged_range);

        let fast_block = self.builder.create_block();
        let slow_block = self.builder.create_block();
        let join = self.builder.create_block();
        self.builder.append_block_param(join, types::I64);
        self.builder
            .ins()
            .brif(fast, fast_block, &[], slow_block, &[]);

        self.switch_to(slow_block);
        let code = self.builder.ins().iconst(types::I32, op.code() as i64);
        let id = self.runtime.binop;
        // The slow path sees the original tagged operands, so it raises the
        // same `OverflowError` the generic operator always did.
        let slow_value = self.call_rt(id, &[code, lhs, rhs])?;
        self.emit_exc_check()?;
        let slow_arg = [ir::BlockArg::from(slow_value)];
        self.builder.ins().jump(join, &slow_arg);
        self.terminated = true;

        self.switch_to(fast_block);
        let tagged = self.builder.ins().ishl_imm(result, INT_TAG_SHIFT);
        let fast_arg = [ir::BlockArg::from(tagged)];
        self.builder.ins().jump(join, &fast_arg);
        self.terminated = true;

        self.switch_to(join);
        self.builder
            .block_params(join)
            .first()
            .copied()
            .ok_or_else(|| CodegenError::new("internal error: int arith join has no parameter"))
    }

    /// `int cmp int` for the six comparison operators.
    ///
    /// Ordering comparisons on two tagged immediates would already be exact
    /// (the payload encoding is monotonic), but untagging keeps the rule
    /// simple: compare payloads, then materialize the `True`/`False`
    /// sentinels. Integer comparison never raises, so there is no exception
    /// check.
    fn lower_int_comparison(
        &mut self,
        op: BinOp,
        lhs: Value,
        rhs: Value,
    ) -> Result<Value, CodegenError> {
        let a = self.untag_int(lhs);
        let b = self.untag_int(rhs);
        let condition = match op {
            BinOp::Eq => IntCC::Equal,
            BinOp::Ne => IntCC::NotEqual,
            BinOp::Lt => IntCC::SignedLessThan,
            BinOp::Le => IntCC::SignedLessThanOrEqual,
            BinOp::Gt => IntCC::SignedGreaterThan,
            BinOp::Ge => IntCC::SignedGreaterThanOrEqual,
            _ => {
                return Err(CodegenError::new(
                    "internal error: non-comparison operator on the int fast path",
                ));
            }
        };
        let condition = self.builder.ins().icmp(condition, a, b);
        // `True` is 17 and `False` is 9 in the runtime's special immediates
        // (`value::TRUE` / `value::FALSE`); both are plain constants.
        let truthy = self.builder.ins().iconst(types::I64, TRUE_TAG);
        let falsy = self.builder.ins().iconst(types::I64, FALSE_TAG);
        Ok(self.builder.ins().select(condition, truthy, falsy))
    }

    /// Branch condition for a tagged `True`/`False` sentinel produced by the
    /// typed int comparison path.
    ///
    /// Both `True` (17) and `False` (9) are non-zero, so a plain zero test
    /// would always branch; compare against the `False` sentinel instead. This
    /// avoids the `pyx_truthy` runtime call on the typed comparison path.
    fn bool_cond(&mut self, value: Value) -> Value {
        self.builder
            .ins()
            .icmp_imm(IntCC::NotEqual, value, FALSE_TAG)
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

    /// Lower `then if test else orelse`.
    ///
    /// Only the selected branch is evaluated: the test drives a `brif` into
    /// two blocks whose values meet in a single result block parameter, the
    /// same shape as [`Self::lower_boolop`].
    fn lower_ifexp(
        &mut self,
        test: &'a Expr,
        then: &'a Expr,
        orelse: &'a Expr,
    ) -> Result<Value, CodegenError> {
        let condition = self.lower_expr(test)?;
        let condition = self.truthy_cond(condition)?;
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let after = self.builder.create_block();
        self.builder.append_block_param(after, types::I64);
        self.builder
            .ins()
            .brif(condition, then_block, &[], else_block, &[]);

        self.switch_to(then_block);
        let then_value = self.lower_expr(then)?;
        let then_arg = [ir::BlockArg::from(then_value)];
        self.builder.ins().jump(after, &then_arg);
        self.terminated = true;

        self.switch_to(else_block);
        let else_value = self.lower_expr(orelse)?;
        let else_arg = [ir::BlockArg::from(else_value)];
        self.builder.ins().jump(after, &else_arg);
        self.terminated = true;

        self.switch_to(after);
        self.builder
            .block_params(after)
            .first()
            .copied()
            .ok_or_else(|| {
                CodegenError::new("internal error: conditional expression result has no parameter")
            })
    }

    /// Lower `object.method(args..., name=value...)` or
    /// `object.method(*star, name=value...)` to a dynamic method call.
    ///
    /// Python evaluates the receiver, then the positional arguments (or the
    /// starred operand), then the keyword values, each left to right. The
    /// receiver and each already evaluated argument are stored into stack
    /// slots immediately so that a collection triggered by a later allocation
    /// can still observe them. Calls without keywords keep using `pyx_method`;
    /// calls with keywords use `pyx_method_kw`; a starred operand uses
    /// `pyx_method_star`, which flattens it into positional arguments.
    fn lower_method_call(
        &mut self,
        object: &'a Expr,
        method: &str,
        args: &'a [Expr],
        star: Option<&'a Expr>,
        keywords: &'a [(String, Expr)],
    ) -> Result<Value, CodegenError> {
        if star.is_some() && !args.is_empty() {
            return Err(CodegenError::new(
                "method call cannot mix positional and starred arguments",
            ));
        }
        let receiver_slot = self.stack_array_slot(1, "method receiver")?;
        let receiver = self.lower_expr(object)?;
        self.builder.ins().stack_store(receiver, receiver_slot, 0);
        let star_slot = if star.is_some() {
            Some(self.stack_array_slot(1, "method starred argument")?)
        } else {
            None
        };

        let argv_slot = self.stack_array_slot(args.len(), "method arguments")?;
        for (index, arg) in args.iter().enumerate() {
            let value = self.lower_expr(arg)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (index * 8) as i32);
        }
        // The starred operand is evaluated after the receiver and before the
        // keywords, matching its position in the call. It is parked in a
        // stack slot like the receiver so that a collection triggered by a
        // keyword value can still observe it.
        let star = match (star, star_slot) {
            (Some(star), Some(slot)) => {
                let value = self.lower_expr(star)?;
                self.builder.ins().stack_store(value, slot, 0);
                Some(value)
            }
            _ => None,
        };

        let receiver = self.builder.ins().stack_load(types::I64, receiver_slot, 0);
        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, method.as_bytes())?;
        let name = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let argc = self.builder.ins().iconst(self.ptr_ty, args.len() as i64);
        // `argc == 0` still passes a valid pointer (the slot has one element).
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);

        if star.is_none() && keywords.is_empty() {
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
        if star.is_some() {
            let Some(star_slot) = star_slot else {
                return Err(CodegenError::new(
                    "internal error: starred method call without a stack slot",
                ));
            };
            let star = self.builder.ins().stack_load(types::I64, star_slot, 0);
            let id = self.runtime.method_star;
            let result = self.call_rt(id, &[receiver, name, name_len, star, kwargc, kw_argv])?;
            self.emit_exc_check()?;
            return Ok(result);
        }
        let id = self.runtime.method_kw;
        let result = self.call_rt(id, &[receiver, name, name_len, argc, argv, kwargc, kw_argv])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower `module.function(args...)` to `pyx_module_call`, or to
    /// `pyx_module_call_kw` when the spec keeps keyword names
    /// (`functools.partial`'s `**kwargs`).
    ///
    /// Python evaluates the arguments left to right before the call; each
    /// value is stored into an 8-byte-aligned stack array immediately so that
    /// a collection triggered by a later allocation cannot observe a stale
    /// value. Module and function names are materialized in byte slots.
    /// Keyword names are constant strings in the IR and become string values,
    /// so the runtime consumes one uniform `[name, value, ...]` buffer.
    fn lower_module_call(
        &mut self,
        module: &str,
        function: &str,
        args: &'a [Expr],
        keywords: &'a [(String, Expr)],
    ) -> Result<Value, CodegenError> {
        let argv_slot = self.stack_array_slot(args.len(), "module call arguments")?;
        for (index, arg) in args.iter().enumerate() {
            let value = self.lower_expr(arg)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (index * 8) as i32);
        }
        let kw_slot = self.stack_array_slot(
            keywords
                .len()
                .checked_mul(2)
                .ok_or_else(|| CodegenError::new("module call: too many keyword arguments"))?,
            "module call keyword arguments",
        )?;
        for (index, (name, value)) in keywords.iter().enumerate() {
            let name = self.str_const(name)?;
            self.builder
                .ins()
                .stack_store(name, kw_slot, (index * 16) as i32);
            let value = self.lower_expr(value)?;
            self.builder
                .ins()
                .stack_store(value, kw_slot, (index * 16 + 8) as i32);
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
        if keywords.is_empty() {
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
            return Ok(result);
        }
        let kwargc = self
            .builder
            .ins()
            .iconst(self.ptr_ty, keywords.len() as i64);
        let kw_argv = self.builder.ins().stack_addr(self.ptr_ty, kw_slot, 0);
        let id = self.runtime.module_call_kw;
        let result = self.call_rt(
            id,
            &[
                module_ptr,
                module_len,
                function_ptr,
                function_len,
                argc,
                argv,
                kwargc,
                kw_argv,
            ],
        )?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower `module` used as a value to `pyx_module_value`.
    ///
    /// The runtime returns (and caches) the module object for the builtin
    /// module name; names are materialized in a byte slot like every other
    /// module ABI call.
    fn lower_module_ref(&mut self, module: &str) -> Result<Value, CodegenError> {
        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, module.as_bytes())?;
        let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let id = self.runtime.module_value;
        let result = self.call_rt(id, &[name_ptr, name_len])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower a name bound by a nested import to `pyx_deferred_import`.
    ///
    /// The helper never returns normally: it raises `RuntimeError` naming the
    /// module and member, so no dynamic import takes place. The exception
    /// check makes the generated code branch to the unit's handler like any
    /// other failing runtime call.
    fn lower_deferred_import(
        &mut self,
        module: &str,
        member: Option<&str>,
    ) -> Result<Value, CodegenError> {
        let (module_ptr, module_len) = self.module_name_args(module)?;
        // An empty member means the binding is a module alias; the ABI treats
        // a zero length as "no member".
        let (member_ptr, member_len) = self.module_name_args(member.unwrap_or(""))?;
        let id = self.runtime.deferred_import;
        let result = self.call_rt(id, &[module_ptr, module_len, member_ptr, member_len])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower a module-global read to `pyx_global_get`.
    fn lower_global_get(&mut self, module: &str, name: &str) -> Result<Value, CodegenError> {
        let (module_ptr, module_len) = self.module_name_args(module)?;
        let (name_ptr, name_len) = self.module_name_args(name)?;
        let id = self.runtime.global_get;
        let result = self.call_rt(id, &[module_ptr, module_len, name_ptr, name_len])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower a module-global store to `pyx_global_set`, which returns `None`.
    fn lower_global_set(
        &mut self,
        module: &str,
        name: &str,
        value: &'a Expr,
    ) -> Result<Value, CodegenError> {
        let module_args = self.module_name_args(module)?;
        let name_args = self.module_name_args(name)?;
        let value = self.lower_expr(value)?;
        let id = self.runtime.global_set;
        let result = self.call_rt(
            id,
            &[
                module_args.0,
                module_args.1,
                name_args.0,
                name_args.1,
                value,
            ],
        )?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Materializes one module/global name in a byte slot and returns the
    /// `(pointer, length)` pair expected by the module/global ABI calls.
    fn module_name_args(&mut self, name: &str) -> Result<(Value, Value), CodegenError> {
        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (slot, len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
        let ptr = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let len = self.builder.ins().iconst(self.ptr_ty, len as i64);
        Ok((ptr, len))
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

    /// Lower `ext.member(args...)` for a compiled CPython extension module to
    /// `pyx_extension_call`.
    ///
    /// Python evaluates the arguments left to right before the call; each
    /// value is stored into an 8-byte-aligned stack array immediately so that
    /// a collection triggered by a later allocation cannot observe a stale
    /// value. The shared-object path and the member name are materialized in
    /// byte slots, like `pyx_module_call`. Keyword arguments never reach the
    /// IR: the frontend rejects them for extension members.
    fn lower_extension_call(
        &mut self,
        path: &str,
        member: &str,
        args: &'a [Expr],
    ) -> Result<Value, CodegenError> {
        let argv_slot = self.stack_array_slot(args.len(), "extension call arguments")?;
        for (index, arg) in args.iter().enumerate() {
            let value = self.lower_expr(arg)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (index * 8) as i32);
        }

        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (path_slot, path_len) = make_bytes_slot(&mut self.builder, path.as_bytes())?;
        let path_ptr = self.builder.ins().stack_addr(self.ptr_ty, path_slot, 0);
        let path_len = self.builder.ins().iconst(self.ptr_ty, path_len as i64);
        let (member_slot, member_len) = make_bytes_slot(&mut self.builder, member.as_bytes())?;
        let member_ptr = self.builder.ins().stack_addr(self.ptr_ty, member_slot, 0);
        let member_len = self.builder.ins().iconst(self.ptr_ty, member_len as i64);
        let argc = self.builder.ins().iconst(self.ptr_ty, args.len() as i64);
        // `argc == 0` still passes a valid pointer (the slot has one element).
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        let id = self.runtime.extension_call;
        let result = self.call_rt(
            id,
            &[path_ptr, path_len, member_ptr, member_len, argc, argv],
        )?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower `ext.name` for a compiled CPython extension module to
    /// `pyx_extension_get` (constant or callable value).
    fn lower_extension_attr(&mut self, path: &str, member: &str) -> Result<Value, CodegenError> {
        // Empty names still get a valid, non-null pointer to a one-byte slot.
        let (path_slot, path_len) = make_bytes_slot(&mut self.builder, path.as_bytes())?;
        let path_ptr = self.builder.ins().stack_addr(self.ptr_ty, path_slot, 0);
        let path_len = self.builder.ins().iconst(self.ptr_ty, path_len as i64);
        let (member_slot, member_len) = make_bytes_slot(&mut self.builder, member.as_bytes())?;
        let member_ptr = self.builder.ins().stack_addr(self.ptr_ty, member_slot, 0);
        let member_len = self.builder.ins().iconst(self.ptr_ty, member_len as i64);
        let id = self.runtime.extension_get;
        let result = self.call_rt(id, &[path_ptr, path_len, member_ptr, member_len])?;
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

    /// Lower `getattr(object, name)`.
    ///
    /// A constant name uses the `pyx_getattr(object, bytes, len)` entry point,
    /// which resolves the attribute by receiver kind (native value, class or
    /// builtin module). A dynamic name first materializes the `getattr` native
    /// function with `pyx_builtin_function` and calls it through `pyx_call`,
    /// exactly like `g = getattr; g(obj, name)`.
    fn lower_getattr_builtin(&mut self, args: &'a [Expr]) -> Result<Value, CodegenError> {
        if args.len() != 2 && args.len() != 3 {
            return Err(CodegenError::new(format!(
                "builtin getattr expects 2 or 3 arguments, got {}",
                args.len()
            )));
        }
        if args.len() == 2 {
            let [object, name] = args else { unreachable!() };
            if let ExprKind::Const(Const::Str(name)) = &name.kind {
                return self.lower_getattr(object, name);
            }
        }

        // Park the callee and both arguments in stack slots as soon as they
        // are evaluated, so a later allocation cannot invalidate them.
        let callee_slot = self.stack_array_slot(1, "getattr callee")?;
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, b"getattr")?;
        let builtin_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let builtin_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let id = self.runtime.builtin_function;
        let function = self.call_rt(id, &[builtin_ptr, builtin_len])?;
        self.emit_exc_check()?;
        self.builder.ins().stack_store(function, callee_slot, 0);

        let argv_slot = self.stack_array_slot(args.len(), "getattr arguments")?;
        for (index, argument) in args.iter().enumerate() {
            let value = self.lower_expr(argument)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (index * 8) as i32);
        }
        // Reload after all argument expressions: the slot is the GC-safe
        // owner of the callee while they may allocate.
        let callee = self.builder.ins().stack_load(types::I64, callee_slot, 0);
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        let argc = self.builder.ins().iconst(self.ptr_ty, args.len() as i64);
        let id = self.runtime.call;
        let result = self.call_rt(id, &[callee, argc, argv])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower `hasattr(object, name)`.
    ///
    /// A constant name uses the `pyx_hasattr(object, bytes, len)` entry point,
    /// like `getattr`; a dynamic name (including a non-string, which the
    /// runtime rejects with `TypeError`) goes through the builtin function
    /// value so the attribute name stays a plain tagged string.
    fn lower_hasattr_builtin(&mut self, args: &'a [Expr]) -> Result<Value, CodegenError> {
        if args.len() != 2 {
            return Err(CodegenError::new(format!(
                "builtin hasattr expects 2 arguments, got {}",
                args.len()
            )));
        }
        let [object, name] = args else { unreachable!() };
        let object = self.lower_expr(object)?;
        if let ExprKind::Const(Const::Str(name)) = &name.kind {
            let (name_slot, name_len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
            let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
            let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
            let id = self.runtime.hasattr;
            let result = self.call_rt(id, &[object, name_ptr, name_len])?;
            self.emit_exc_check()?;
            return Ok(result);
        }
        let name = self.lower_expr(name)?;
        self.lower_builtin_values(Builtin::Hasattr, &[object, name])
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
            Const::NotImplemented => Ok(self.tagged_int(NOT_IMPLEMENTED_TAG)),
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
    /// value via `pyx_function_new(wrapper, arity, name, name_len, flags)`.
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
            .iconst(self.ptr_ty, function.call_arity() as i64);
        let (name_slot, name_len) = make_bytes_slot(&mut self.builder, function.name.as_bytes())?;
        let name = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
        let name_len = self.builder.ins().iconst(self.ptr_ty, name_len as i64);
        let flags = self.function_flags(function);
        let id = self.runtime.function_new;
        let result = self.call_rt(id, &[wrapper_address, arity, name, name_len, flags])?;
        self.emit_exc_check()?;
        let result = self.attach_function_params(result, function)?;
        Ok(result)
    }

    /// Attach the user-visible positional parameter names of a generated
    /// wrapper to its function value through `pyx_function_params`.
    ///
    /// The runtime needs them to bind `functools.partial` keyword arguments by
    /// name. Captured values, `*args`, keyword-only parameters and `**kwargs`
    /// have no positional slot and are therefore not listed; a dynamic
    /// signature is skipped entirely because its calls take the count-prefixed
    /// buffer the runtime cannot bind by name.
    fn attach_function_params(
        &mut self,
        value: Value,
        function: &Function,
    ) -> Result<Value, CodegenError> {
        if function.has_dynamic_signature() {
            return Ok(value);
        }
        let names: Vec<&str> = function.params[function.capture_count..]
            .iter()
            .filter(|param| param.kind == ParamKind::Positional)
            .map(|param| param.name.as_str())
            .collect();
        if names.is_empty() {
            return Ok(value);
        }
        let slots = names
            .len()
            .checked_mul(2)
            .ok_or_else(|| CodegenError::new("function: too many parameters"))?;
        let names_slot = self.stack_array_slot(slots, "function parameter names")?;
        for (index, name) in names.iter().enumerate() {
            let (name_slot, name_len) = make_bytes_slot(&mut self.builder, name.as_bytes())?;
            // Both words are `i64`: the ABI buffer layout is fixed-width even
            // on a 32-bit target.
            let name_ptr = self.builder.ins().stack_addr(self.ptr_ty, name_slot, 0);
            let name_ptr = self.pointer_to_i64(name_ptr);
            self.builder
                .ins()
                .stack_store(name_ptr, names_slot, (index * 16) as i32);
            let name_len = self.builder.ins().iconst(types::I64, name_len as i64);
            self.builder
                .ins()
                .stack_store(name_len, names_slot, (index * 16 + 8) as i32);
        }
        let count = self.builder.ins().iconst(self.ptr_ty, names.len() as i64);
        let names_ptr = self.builder.ins().stack_addr(self.ptr_ty, names_slot, 0);
        let id = self.runtime.function_params;
        self.call_rt(id, &[value, count, names_ptr])
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

    /// Lower a call through a function value via `pyx_call(fn, argc, argv)`,
    /// or `pyx_call_star(fn, argc, argv, star)` when the call has a starred
    /// operand.
    ///
    /// Python evaluates the callee first, then the arguments left to right.
    /// Both the callee and every already-evaluated argument are parked in
    /// stack slots immediately so that later evaluation cannot invalidate
    /// them. The starred operand is evaluated last; the runtime flattens it
    /// after the leading positional operands.
    fn lower_indirect_call(
        &mut self,
        callee: &'a Expr,
        args: &'a [Expr],
        star: Option<&'a Expr>,
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
        let star_slot = match star {
            Some(star) => {
                let slot = self.stack_array_slot(1, "call starred argument")?;
                let value = self.lower_expr(star)?;
                self.builder.ins().stack_store(value, slot, 0);
                Some(slot)
            }
            None => None,
        };

        let callee = self.builder.ins().stack_load(types::I64, callee_slot, 0);
        let argc = self.builder.ins().iconst(self.ptr_ty, args.len() as i64);
        // `argc == 0` still passes a valid pointer: the slot holds one
        // element, which is 8-byte aligned.
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        if let Some(star_slot) = star_slot {
            let star = self.builder.ins().stack_load(types::I64, star_slot, 0);
            let id = self.runtime.call_star;
            let result = self.call_rt(id, &[callee, argc, argv, star])?;
            self.emit_exc_check()?;
            return Ok(result);
        }
        let id = self.runtime.call;
        let result = self.call_rt(id, &[callee, argc, argv])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    /// Lower a call the frontend could not bind statically.
    ///
    /// The callee and every operand are evaluated in source order into stack
    /// slots (so side effects and evaluation order match Python), then
    /// `pyx_deferred_call` raises `RuntimeError`.
    fn lower_deferred_call(
        &mut self,
        callee: Option<&'a Expr>,
        args: &'a [Expr],
    ) -> Result<Value, CodegenError> {
        let count = args.len() + usize::from(callee.is_some());
        let argv_slot = self.stack_array_slot(count, "deferred call arguments")?;
        let mut position = 0usize;
        if let Some(callee) = callee {
            let value = self.lower_expr(callee)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (position * 8) as i32);
            position += 1;
        }
        for arg in args {
            let value = self.lower_expr(arg)?;
            self.builder
                .ins()
                .stack_store(value, argv_slot, (position * 8) as i32);
            position += 1;
        }
        let argc = self.builder.ins().iconst(self.ptr_ty, count as i64);
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        let id = self.runtime.deferred_call;
        let result = self.call_rt(id, &[argc, argv])?;
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

    fn lower_call(&mut self, callee: &Callee, args: &'a [Expr]) -> Result<Value, CodegenError> {
        match callee {
            Callee::Function(id) => self.lower_direct_call(*id, args),
            // F-string fields carry their static spec and conversion flag as
            // trailing constant arguments; they become a direct runtime call
            // instead of a first-class builtin invocation. A source-level
            // `format(value, spec)` has no such constants and takes the
            // ordinary builtin path.
            Callee::Builtin(Builtin::Format) if is_format_field(args) => {
                self.lower_format_call(args)
            }
            Callee::Builtin(Builtin::Getattr) => self.lower_getattr_builtin(args),
            Callee::Builtin(Builtin::Hasattr) => self.lower_hasattr_builtin(args),
            Callee::Builtin(builtin) => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.lower_expr(arg)?);
                }
                self.lower_builtin(*builtin, &values)
            }
            // Keyword arguments are impossible here: the frontend rejects
            // them for extension members.
            Callee::Extension { path, member } => self.lower_extension_call(path, member, args),
        }
    }

    /// Lower a call to a program function.
    ///
    /// `async def` and generator functions are not executed inline: the
    /// arguments are evaluated left to right into a stack array and handed to
    /// `pyx_coroutine_new`/`pyx_generator_new`, which create the suspended
    /// object without running the body. Plain functions keep the direct
    /// `__pyx_fn_N` call.
    fn lower_direct_call(
        &mut self,
        id: FunctionId,
        args: &'a [Expr],
    ) -> Result<Value, CodegenError> {
        let function = self
            .program
            .functions
            .get(id as usize)
            .ok_or_else(|| CodegenError::new(format!("call to unknown function id {id}")))?;
        if function.capture_count != 0 {
            return Err(CodegenError::new(format!(
                "internal error: function '{}' has captures and needs an indirect call",
                function.name
            )));
        }
        if function.params.len() != args.len() {
            let name = &function.name;
            let expected = function.params.len();
            let actual = args.len();
            return Err(CodegenError::new(format!(
                "call to function {name} expects {expected} arguments, got {actual}"
            )));
        }
        let deferred = function.is_async || function.is_generator;
        let is_async = function.is_async;

        if deferred {
            if function.has_dynamic_signature() {
                // The runtime creates the fiber from the caller's positional
                // arguments; the frontend therefore emits an indirect call for
                // a dynamic coroutine/generator signature instead of binding
                // the argument slots here.
                return Err(CodegenError::new(format!(
                    "internal error: dynamic generator/coroutine '{}' must be called through a function value",
                    function.name
                )));
            }
            // Each argument is stored as soon as it is evaluated so that an
            // allocation in a later argument cannot observe a stale value.
            let argv_slot = self.stack_array_slot(args.len(), "call arguments")?;
            for (index, arg) in args.iter().enumerate() {
                let value = self.lower_expr(arg)?;
                self.builder
                    .ins()
                    .stack_store(value, argv_slot, (index * 8) as i32);
            }
            let wrapper = *self.wrappers.get(&id).ok_or_else(|| {
                CodegenError::new(format!("call to undeclared wrapper for function id {id}"))
            })?;
            let reference = self.func_ref(wrapper);
            let entry = self.builder.ins().func_addr(self.ptr_ty, reference);
            let entry = self.pointer_to_i64(entry);
            let argc = self.builder.ins().iconst(self.ptr_ty, args.len() as i64);
            // `argc == 0` still passes a valid pointer (the slot has one
            // element).
            let argv = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
            let runtime_id = if is_async {
                self.runtime.coroutine_new
            } else {
                self.runtime.generator_new
            };
            let result = self.call_rt(runtime_id, &[entry, argc, argv])?;
            self.emit_exc_check()?;
            return Ok(result);
        }

        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.lower_expr(arg)?);
        }
        let target = *self
            .symbols
            .get(&id)
            .ok_or_else(|| CodegenError::new(format!("call to undeclared function id {id}")))?;
        let reference = self.func_ref(target);
        let call = self.builder.ins().call(reference, &values);
        let result = self
            .builder
            .inst_results(call)
            .first()
            .copied()
            .ok_or_else(|| CodegenError::new("internal error: user call returned no value"))?;
        self.emit_exc_check()?;
        Ok(result)
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

    /// Invoke a builtin with an already-evaluated argument list.
    ///
    /// Builtins without a dedicated runtime entry point are called through
    /// their first-class function value: `pyx_builtin_function` materialises
    /// it and `pyx_call` dispatches to the runtime builtin table.
    fn lower_builtin_values(
        &mut self,
        builtin: Builtin,
        values: &[Value],
    ) -> Result<Value, CodegenError> {
        let slot = self.stack_array_slot(values.len(), builtin.name())?;
        for (index, value) in values.iter().enumerate() {
            self.builder
                .ins()
                .stack_store(*value, slot, (index * 8) as i32);
        }
        self.call_builtin_value(builtin, values.len(), slot)
    }

    /// Materialise the builtin function value and call it with `argc`
    /// arguments parked in `argv`.
    fn call_builtin_value(
        &mut self,
        builtin: Builtin,
        argc: usize,
        argv: StackSlot,
    ) -> Result<Value, CodegenError> {
        let function = self.lower_builtin_ref(builtin)?;
        let argc = self.builder.ins().iconst(self.ptr_ty, argc as i64);
        let argv = self.builder.ins().stack_addr(self.ptr_ty, argv, 0);
        let id = self.runtime.call;
        let result = self.call_rt(id, &[function, argc, argv])?;
        self.emit_exc_check()?;
        Ok(result)
    }

    fn lower_builtin(&mut self, builtin: Builtin, values: &[Value]) -> Result<Value, CodegenError> {
        match builtin {
            Builtin::Bool
            | Builtin::Type
            | Builtin::Reversed
            | Builtin::Divmod
            | Builtin::Callable
            | Builtin::Import => self.lower_builtin_values(builtin, values),
            Builtin::Max | Builtin::Min => {
                // The frontend always appends the key slot as the final
                // argument (`None` without `key=`, the callable otherwise) and
                // the runtime treats it as such.
                if values.len() < 2 {
                    let name = builtin.name();
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin {name} expects the candidates and the trailing key slot, got {actual} argument(s)"
                    )));
                }
                self.lower_builtin_values(builtin, values)
            }
            Builtin::Sorted => {
                // Direct lowering always supplies `(iterable, key, reverse)`;
                // first-class calls pass one to three arguments. The generic
                // builtin value call keeps the ABI unchanged.
                if values.is_empty() || values.len() > 3 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin sorted expects 1 to 3 arguments, got {actual}"
                    )));
                }
                self.lower_builtin_values(builtin, values)
            }
            Builtin::All => {
                if values.len() != 1 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin all expects exactly 1 argument, got {actual}"
                    )));
                }
                let id = self.runtime.all;
                let result = self.call_rt(id, values)?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Ord | Builtin::Chr => {
                if values.len() != 1 {
                    let name = builtin.name();
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin {name} expects exactly 1 argument, got {actual}"
                    )));
                }
                let id = match builtin {
                    Builtin::Ord => self.runtime.ord,
                    Builtin::Chr => self.runtime.chr,
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
            Builtin::DictMerge => {
                if values.len() != 2 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "internal dict merge expects exactly 2 arguments, got {actual}"
                    )));
                }
                let id = self.runtime.dict_merge;
                let result = self.call_rt(id, values)?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Round => {
                if values.is_empty() || values.len() > 2 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin round expects 1 or 2 arguments, got {actual}"
                    )));
                }
                self.lower_builtin_values(builtin, values)
            }
            Builtin::Format => {
                // Three arguments are the internal f-string shape and are
                // handled by `lower_format_call` before they reach this point;
                // source-level `format(value, spec)` takes exactly two.
                if values.len() != 2 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin format expects 2 arguments, got {actual}"
                    )));
                }
                self.lower_builtin_values(builtin, values)
            }
            Builtin::Hash => {
                if values.len() != 1 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin hash expects exactly 1 argument, got {actual}"
                    )));
                }
                let id = self.runtime.hash;
                let result = self.call_rt(id, &values[..1])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Isinstance => {
                if values.len() != 2 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin isinstance expects 2 arguments, got {actual}"
                    )));
                }
                self.lower_builtin_values(builtin, values)
            }
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
            Builtin::Iter => {
                if values.len() != 1 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin iter expects 1 argument, got {actual}"
                    )));
                }
                let id = self.runtime.iter_of;
                let result = self.call_rt(id, &values[..1])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Next => {
                let (obj, default, has_default) = match values {
                    [obj] => (*obj, self.none(), 0i64),
                    [obj, default] => (*obj, *default, 1i64),
                    actual => {
                        let actual = actual.len();
                        return Err(CodegenError::new(format!(
                            "builtin next expects 1 or 2 arguments, got {actual}"
                        )));
                    }
                };
                let has_default = self.builder.ins().iconst(types::I64, has_default);
                let id = self.runtime.next_value;
                let result = self.call_rt(id, &[obj, default, has_default])?;
                self.emit_exc_check()?;
                Ok(result)
            }
            Builtin::Set | Builtin::Frozenset => {
                // PyX has a single immutable set representation, so
                // `frozenset(...)` shares the `set(...)` lowering.
                let name = builtin.name();
                match values.len() {
                    0 => {
                        // A valid dummy pointer is required by the ABI.
                        let slot = self.stack_array_slot(0, name)?;
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
                        "builtin {name} expects at most 1 argument, got {actual}"
                    ))),
                }
            }
            Builtin::Getattr => Err(CodegenError::new(
                "internal error: getattr reached first-class builtin lowering",
            )),
            Builtin::Hasattr => {
                if values.len() != 2 {
                    let actual = values.len();
                    return Err(CodegenError::new(format!(
                        "builtin hasattr expects 2 arguments, got {actual}"
                    )));
                }
                self.lower_builtin_values(builtin, values)
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
            Builtin::Tuple => match values.len() {
                0 => {
                    // `tuple()` — a valid dummy pointer is required by the ABI.
                    let slot = self.stack_array_slot(0, "tuple()")?;
                    let dummy = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
                    let argc = self.builder.ins().iconst(self.ptr_ty, 0);
                    let id = self.runtime.tuple_new;
                    let result = self.call_rt(id, &[argc, dummy])?;
                    self.emit_exc_check()?;
                    Ok(result)
                }
                1 => {
                    let id = self.runtime.tuple_from;
                    let result = self.call_rt(id, &values[..1])?;
                    self.emit_exc_check()?;
                    Ok(result)
                }
                actual => Err(CodegenError::new(format!(
                    "builtin tuple expects at most 1 argument, got {actual}"
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
            Builtin::Len
            | Builtin::Str
            | Builtin::Repr
            | Builtin::Int
            | Builtin::Float
            | Builtin::Abs => {
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
                    Builtin::Repr => self.runtime.to_repr,
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

/// True when `Builtin::Format` arguments have the internal f-string shape.
///
/// An f-string field always passes three arguments `(value, spec,
/// conversion)`; source-level `format(value, spec)` passes two and is lowered
/// through the ordinary builtin path.
fn is_format_field(args: &[Expr]) -> bool {
    args.len() == 3
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
        // Imported module bodies run once, in dependency order, before the
        // entry module. A spawned child skipped this block entirely, so it
        // never re-runs them.
        for &initializer in &program.initializers {
            let symbol = *symbols.get(&initializer).ok_or_else(|| {
                CodegenError::new(format!("module initializer {initializer} was not declared"))
            })?;
            let reference = declare_ref(module, &mut builder, symbol);
            builder.ins().call(reference, &[]);
        }
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
    use pyx_ir::{ClassMethod, LocalInfo, MethodKind, Param, StmtKind, Type};

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
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
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
    fn divmod_and_callable_builtins_lower_through_the_value_path() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let cases = [
            (Builtin::Divmod, vec![int(7), int(3)], Type::Tuple),
            (Builtin::Callable, vec![int(1)], Type::Bool),
        ];
        for (builtin, args, ret) in cases {
            let mut program = program_assigning(Expr::new(
                ExprKind::Call {
                    callee: Callee::Builtin(builtin),
                    args,
                },
                ret,
                1,
            ));
            program.functions[0].locals[0].ty = ret;
            let object = crate::compile_object(&program)
                .unwrap_or_else(|_| panic!("{}() must lower", builtin.name()));
            assert!(!object.is_empty());
        }
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
                star: None,
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
                star: None,
                keywords: Vec::new(),
            },
            Type::Str,
            1,
        ));
        let object = crate::compile_object(&program).expect("plain method call must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn starred_method_call_lowers_to_pyx_method_star() {
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let local = || Expr::new(ExprKind::Local(0), Type::Any, 1);
        let program = program_assigning(Expr::new(
            ExprKind::MethodCall {
                object: Box::new(local()),
                method: "add_row".to_string(),
                args: Vec::new(),
                star: Some(Box::new(Expr::new(
                    ExprKind::List(vec![text("a"), text("b")]),
                    Type::List,
                    1,
                ))),
                keywords: vec![("style".to_string(), text("bold"))],
            },
            Type::Any,
            1,
        ));
        let object = crate::compile_object(&program).expect("starred method call must lower");
        assert!(runtime_call_sites(&object, "pyx_method_star") > 0);
    }

    #[test]
    fn starred_indirect_call_lowers_to_pyx_call_star() {
        let int = |value: i64| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let program = program_assigning(Expr::new(
            ExprKind::IndirectCall {
                callee: Box::new(Expr::new(
                    ExprKind::BuiltinRef(Builtin::Abs),
                    Type::Function,
                    1,
                )),
                args: vec![int(1)],
                star: Some(Box::new(Expr::new(
                    ExprKind::List(vec![int(2), int(3)]),
                    Type::List,
                    1,
                ))),
            },
            Type::Any,
            1,
        ));
        let object = crate::compile_object(&program).expect("starred call must lower");
        assert!(runtime_call_sites(&object, "pyx_call_star") > 0);
    }

    #[test]
    fn repr_lowers_to_pyx_to_repr() {
        let int = |value: i64| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let program = program_assigning(Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Repr),
                args: vec![int(7)],
            },
            Type::Str,
            1,
        ));
        let object = crate::compile_object(&program).expect("repr must lower");
        assert!(runtime_call_sites(&object, "pyx_to_repr") > 0);
    }

    #[test]
    fn dict_merge_and_all_lower_to_the_runtime() {
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let key = || Expr::new(ExprKind::Const(Const::Str("k".to_string())), Type::Str, 1);
        let empty = || Expr::new(ExprKind::Dict(Vec::new()), Type::Dict, 1);
        let merge = Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::DictMerge),
                args: vec![
                    Expr::new(ExprKind::Dict(vec![(key(), text("v"))]), Type::Dict, 1),
                    empty(),
                ],
            },
            Type::Dict,
            1,
        );
        let object =
            crate::compile_object(&program_assigning(merge)).expect("dict merge must lower");
        assert!(runtime_call_sites(&object, "pyx_dict_merge") > 0);

        let every = Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::All),
                args: vec![Expr::new(ExprKind::List(vec![text("x")]), Type::List, 1)],
            },
            Type::Bool,
            1,
        );
        let object = crate::compile_object(&program_assigning(every)).expect("all() must lower");
        assert!(runtime_call_sites(&object, "pyx_all") > 0);
    }

    #[test]
    fn class_definition_and_setattr_emit_an_object_file() {
        let method = Function {
            name: "C.get".to_string(),
            params: vec![Param {
                local: 0,
                name: "self".to_string(),
                ty: Type::Any,
                default: None,
                kind: ParamKind::Positional,
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
            is_async: false,
            is_generator: false,
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
                            kind: MethodKind::Normal,
                        }],
                        attributes: Vec::new(),
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
                                star: None,
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
            is_async: false,
            is_generator: false,
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, method],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program)
            .expect("class definition and attribute store must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn exception_class_base_lowers_through_the_cached_class_abi() {
        let module_body = Function {
            name: "<module>".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![
                LocalInfo {
                    name: "E".to_string(),
                    ty: Type::Any,
                },
                LocalInfo {
                    name: "e".to_string(),
                    ty: Type::Any,
                },
            ],
            body: vec![
                Stmt::new(
                    StmtKind::ClassDef {
                        target: 0,
                        name: "E".to_string(),
                        base: Some(Expr::new(
                            ExprKind::ExceptionClass(ExcKind::Exception),
                            Type::Any,
                            1,
                        )),
                        methods: Vec::new(),
                        attributes: Vec::new(),
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
                                star: None,
                            },
                            Type::Any,
                            2,
                        ),
                    },
                    2,
                ),
            ],
            is_async: false,
            is_generator: false,
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program)
            .expect("a built-in exception base must lower as a class value");
        assert!(runtime_call_sites(&object, "pyx_exception_class") > 0);
        assert!(runtime_call_sites(&object, "pyx_class_new") > 0);
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
                    default: None,
                    kind: ParamKind::Positional,
                },
                Param {
                    local: 1,
                    name: "y".to_string(),
                    ty: Type::Any,
                    default: None,
                    kind: ParamKind::Positional,
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
            is_async: false,
            is_generator: false,
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
                            star: None,
                        },
                        Type::Any,
                        3,
                    )),
                    3,
                ),
            ],
            is_async: false,
            is_generator: false,
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, inner],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program).expect("closure definition must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn direct_call_to_async_function_creates_a_coroutine() {
        let module_body = Function {
            name: "<module>".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![LocalInfo {
                name: "c".to_string(),
                ty: Type::Any,
            }],
            body: vec![Stmt::new(
                StmtKind::Assign {
                    target: 0,
                    value: Expr::new(
                        ExprKind::Call {
                            callee: Callee::Function(1),
                            args: vec![Expr::new(ExprKind::Const(Const::Int(1)), Type::Int, 1)],
                        },
                        Type::Any,
                        1,
                    ),
                },
                1,
            )],
            is_async: false,
            is_generator: false,
        };
        // The body would be a `return` if the call executed it directly; the
        // call must only build the coroutine object.
        let coroutine = Function {
            name: "work".to_string(),
            params: vec![Param {
                local: 0,
                name: "n".to_string(),
                ty: Type::Int,
                default: None,
                kind: ParamKind::Positional,
            }],
            capture_count: 0,
            ret_ty: Type::Any,
            locals: vec![LocalInfo {
                name: "n".to_string(),
                ty: Type::Int,
            }],
            body: vec![Stmt::new(
                StmtKind::Return(Some(Expr::new(ExprKind::Local(0), Type::Int, 2))),
                2,
            )],
            is_async: true,
            is_generator: false,
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, coroutine],
            main: 0,
            initializers: Vec::new(),
        };
        let object =
            crate::compile_object(&program).expect("direct call to an async function must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn await_and_yield_lower_to_the_runtime_abi() {
        let generator = Function {
            name: "count".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::Any,
            locals: Vec::new(),
            body: vec![
                Stmt::new(
                    StmtKind::Yield {
                        value: Some(Expr::new(ExprKind::Const(Const::Int(1)), Type::Int, 1)),
                    },
                    1,
                ),
                Stmt::new(StmtKind::Yield { value: None }, 2),
            ],
            is_async: false,
            is_generator: true,
        };
        let module_body = Function {
            name: "<module>".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![
                LocalInfo {
                    name: "c".to_string(),
                    ty: Type::Any,
                },
                LocalInfo {
                    name: "g".to_string(),
                    ty: Type::Any,
                },
            ],
            body: vec![
                Stmt::new(
                    StmtKind::Assign {
                        target: 0,
                        value: Expr::new(
                            ExprKind::Call {
                                callee: Callee::Function(1),
                                args: vec![Expr::new(ExprKind::Const(Const::Int(1)), Type::Int, 1)],
                            },
                            Type::Any,
                            1,
                        ),
                    },
                    1,
                ),
                Stmt::new(
                    StmtKind::Assign {
                        target: 1,
                        value: Expr::new(
                            ExprKind::Call {
                                callee: Callee::Function(2),
                                args: Vec::new(),
                            },
                            Type::Any,
                            2,
                        ),
                    },
                    2,
                ),
            ],
            is_async: false,
            is_generator: false,
        };
        let async_body = Function {
            name: "run".to_string(),
            params: vec![Param {
                local: 0,
                name: "c".to_string(),
                ty: Type::Any,
                default: None,
                kind: ParamKind::Positional,
            }],
            capture_count: 0,
            ret_ty: Type::Any,
            locals: vec![LocalInfo {
                name: "c".to_string(),
                ty: Type::Any,
            }],
            body: vec![Stmt::new(
                StmtKind::Return(Some(Expr::new(
                    ExprKind::Await {
                        value: Box::new(Expr::new(ExprKind::Local(0), Type::Any, 3)),
                    },
                    Type::Any,
                    3,
                ))),
                3,
            )],
            is_async: true,
            is_generator: false,
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, async_body, generator],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program).expect("await/yield must lower");
        assert!(!object.is_empty());
    }

    /// A module body containing `body` and no locals.
    fn program_with_body(body: Vec<Stmt>) -> Program {
        Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: Vec::new(),
                body,
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        }
    }

    #[test]
    fn dynamic_function_signatures_emit_wrapper_binding() {
        let int = |value: i64| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let module_body = Function {
            name: "<module>".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![
                LocalInfo {
                    name: "f".to_string(),
                    ty: Type::Function,
                },
                LocalInfo {
                    name: "x".to_string(),
                    ty: Type::Any,
                },
            ],
            body: vec![
                Stmt::new(
                    StmtKind::Assign {
                        target: 0,
                        value: Expr::new(ExprKind::FunctionRef(1), Type::Function, 1),
                    },
                    1,
                ),
                Stmt::new(
                    StmtKind::Assign {
                        target: 1,
                        value: Expr::new(
                            ExprKind::IndirectCall {
                                callee: Box::new(Expr::new(ExprKind::Local(0), Type::Function, 1)),
                                args: vec![int(1), int(2)],
                                star: None,
                            },
                            Type::Any,
                            1,
                        ),
                    },
                    1,
                ),
            ],
            is_async: false,
            is_generator: false,
        };
        let varargs = Function {
            name: "f".to_string(),
            params: vec![Param {
                local: 0,
                name: "args".to_string(),
                ty: Type::List,
                default: None,
                kind: ParamKind::VarArg,
            }],
            capture_count: 0,
            ret_ty: Type::List,
            locals: vec![LocalInfo {
                name: "args".to_string(),
                ty: Type::List,
            }],
            body: vec![Stmt::new(
                StmtKind::Return(Some(Expr::new(ExprKind::Local(0), Type::List, 1))),
                1,
            )],
            is_async: false,
            is_generator: false,
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, varargs],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program).expect("dynamic signature must lower");
        assert!(!object.is_empty());
        assert!(program.functions[1].has_dynamic_signature());
        assert_eq!(program.functions[1].call_arity(), 0);
    }

    #[test]
    fn dynamic_wrapper_materializes_literal_defaults() {
        // `def f(a=1.5, *, key="k")`: the wrapper binds both defaults when the
        // function value is called without arguments.
        let module_body = Function {
            name: "<module>".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![LocalInfo {
                name: "f".to_string(),
                ty: Type::Function,
            }],
            body: vec![Stmt::new(
                StmtKind::Assign {
                    target: 0,
                    value: Expr::new(ExprKind::FunctionRef(1), Type::Function, 1),
                },
                1,
            )],
            is_async: false,
            is_generator: false,
        };
        let defaults = Function {
            name: "f".to_string(),
            params: vec![
                Param {
                    local: 0,
                    name: "a".to_string(),
                    ty: Type::Any,
                    default: Some(Const::Float(1.5)),
                    kind: ParamKind::Positional,
                },
                Param {
                    local: 1,
                    name: "key".to_string(),
                    ty: Type::Any,
                    default: Some(Const::Str("k".to_string())),
                    kind: ParamKind::KwOnly,
                },
            ],
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![
                LocalInfo {
                    name: "a".to_string(),
                    ty: Type::Any,
                },
                LocalInfo {
                    name: "key".to_string(),
                    ty: Type::Any,
                },
            ],
            body: vec![Stmt::new(StmtKind::Return(None), 1)],
            is_async: false,
            is_generator: false,
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, defaults],
            main: 0,
            initializers: Vec::new(),
        };
        let object =
            crate::compile_object(&program).expect("literal defaults must lower in the wrapper");
        assert!(!object.is_empty());
        assert_eq!(program.functions[1].call_arity(), 0);
    }

    #[test]
    fn deferred_calls_lower_to_the_runtime_helper() {
        let body = vec![Stmt::new(
            StmtKind::Expr(Expr::new(
                ExprKind::DeferredCall {
                    callee: None,
                    args: vec![Expr::new(ExprKind::Const(Const::Int(1)), Type::Int, 1)],
                },
                Type::Any,
                1,
            )),
            1,
        )];
        let program = program_with_body(body);
        let object = crate::compile_object(&program).expect("deferred calls must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn deferred_call_without_operands_lowers() {
        let body = vec![Stmt::new(
            StmtKind::Expr(Expr::new(
                ExprKind::DeferredCall {
                    callee: None,
                    args: Vec::new(),
                },
                Type::Any,
                1,
            )),
            1,
        )];
        let program = program_with_body(body);
        let object = crate::compile_object(&program).expect("deferred calls must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn int_binop_fast_paths_emit_an_object_file() {
        let int = || Expr::new(ExprKind::Const(Const::Int(2)), Type::Int, 1);
        let binary = |op: BinOp, ty: Type| {
            Expr::new(
                ExprKind::Bin {
                    op,
                    lhs: Box::new(int()),
                    rhs: Box::new(int()),
                },
                ty,
                1,
            )
        };
        // Every specialised shape must lower: the three checked-arithmetic
        // paths (with their overflow slow block) and the six comparisons.
        let operations = [
            binary(BinOp::Add, Type::Int),
            binary(BinOp::Sub, Type::Int),
            binary(BinOp::Mul, Type::Int),
            binary(BinOp::Eq, Type::Bool),
            binary(BinOp::Ne, Type::Bool),
            binary(BinOp::Lt, Type::Bool),
            binary(BinOp::Le, Type::Bool),
            binary(BinOp::Gt, Type::Bool),
            binary(BinOp::Ge, Type::Bool),
        ];
        let body = operations
            .into_iter()
            .map(|expr| Stmt::new(StmtKind::Expr(expr), 1))
            .collect();
        let program = program_with_body(body);
        let object = crate::compile_object(&program).expect("int binops must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn any_operand_add_still_lowers_through_the_runtime_binop() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![LocalInfo {
                    name: "x".to_string(),
                    ty: Type::Any,
                }],
                body: vec![
                    Stmt::new(
                        StmtKind::Assign {
                            target: 0,
                            value: int(1),
                        },
                        1,
                    ),
                    Stmt::new(
                        StmtKind::Expr(Expr::new(
                            ExprKind::Bin {
                                op: BinOp::Add,
                                lhs: Box::new(Expr::new(ExprKind::Local(0), Type::Any, 2)),
                                rhs: Box::new(int(2)),
                            },
                            Type::Any,
                            2,
                        )),
                        2,
                    ),
                ],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program).expect("Any operand must lower");
        assert!(!object.is_empty());
        // The dynamic operator stays in the object's symbol/relocation table.
        assert!(
            object
                .windows(b"pyx_binop".len())
                .any(|window| window == &b"pyx_binop"[..]),
            "the Any operand must call pyx_binop"
        );
    }

    #[test]
    fn int_aug_assign_and_range_loop_emit_an_object_file() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let local = |id| Expr::new(ExprKind::Local(id), Type::Int, 1);
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![
                    LocalInfo {
                        name: "i".to_string(),
                        ty: Type::Int,
                    },
                    LocalInfo {
                        name: "total".to_string(),
                        ty: Type::Int,
                    },
                ],
                body: vec![
                    Stmt::new(
                        StmtKind::Assign {
                            target: 0,
                            value: int(0),
                        },
                        1,
                    ),
                    // `while i < 3: i += 1` exercises the int aug-assign path.
                    Stmt::new(
                        StmtKind::While {
                            cond: Expr::new(
                                ExprKind::Bin {
                                    op: BinOp::Lt,
                                    lhs: Box::new(local(0)),
                                    rhs: Box::new(int(3)),
                                },
                                Type::Bool,
                                2,
                            ),
                            body: vec![Stmt::new(
                                StmtKind::AugAssign {
                                    target: 0,
                                    op: BinOp::Add,
                                    value: int(1),
                                },
                                3,
                            )],
                        },
                        2,
                    ),
                    Stmt::new(
                        StmtKind::Assign {
                            target: 1,
                            value: int(0),
                        },
                        4,
                    ),
                    // `for i in range(0, 3, 1): total += i` exercises the
                    // typed range counter and the aug-assign path together.
                    Stmt::new(
                        StmtKind::ForRange {
                            var: 0,
                            start: int(0),
                            stop: int(3),
                            step: Some(int(1)),
                            body: vec![Stmt::new(
                                StmtKind::AugAssign {
                                    target: 1,
                                    op: BinOp::Add,
                                    value: local(0),
                                },
                                5,
                            )],
                        },
                        4,
                    ),
                ],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object =
            crate::compile_object(&program).expect("int aug-assign and range loop must lower");
        assert!(!object.is_empty());
    }

    /// Number of relocation entries in an object file that target the import
    /// named `symbol`, i.e. the number of emitted calls to it.
    fn runtime_call_sites(object: &[u8], symbol: &str) -> usize {
        use cranelift_object::object::read::{Object, ObjectSection, ObjectSymbol};
        use cranelift_object::object::RelocationTarget;

        let file = cranelift_object::object::read::File::parse(object)
            .expect("the AOT backend must emit a readable object file");
        let Some(target) = file
            .symbols()
            .find(|candidate| candidate.name().ok() == Some(symbol))
            .map(|candidate| candidate.index())
        else {
            return 0;
        };
        let mut sites = 0;
        for section in file.sections() {
            for (_, relocation) in section.relocations() {
                if relocation.target() == RelocationTarget::Symbol(target) {
                    sites += 1;
                }
            }
        }
        sites
    }

    /// Number of relocation entries in an object file that target the
    /// `pyx_binop` import, i.e. the number of emitted calls to the dynamic
    /// operator.
    fn runtime_binop_call_sites(object: &[u8]) -> usize {
        runtime_call_sites(object, "pyx_binop")
    }

    #[test]
    fn extension_call_and_attr_lower_to_the_runtime_imports() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let path = "pyx_capi_probe.abi3.so".to_string();
        let call = Expr::new(
            ExprKind::Call {
                callee: Callee::Extension {
                    path: path.clone(),
                    member: "add".to_string(),
                },
                args: vec![int(2), int(3)],
            },
            Type::Int,
            1,
        );
        let attr = Expr::new(
            ExprKind::ExtensionAttr {
                path: path.clone(),
                member: "tag".to_string(),
            },
            Type::Any,
            2,
        );
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
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
                        name: "tag".to_string(),
                        ty: Type::Any,
                    },
                ],
                body: vec![
                    Stmt::new(
                        StmtKind::Assign {
                            target: 0,
                            value: call,
                        },
                        1,
                    ),
                    Stmt::new(
                        StmtKind::Assign {
                            target: 1,
                            value: attr,
                        },
                        2,
                    ),
                ],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program)
            .expect("extension call and attribute must lower to object code");
        assert!(!object.is_empty());
        assert!(
            runtime_call_sites(&object, "pyx_extension_call") > 0,
            "the extension call must reference pyx_extension_call"
        );
        assert!(
            runtime_call_sites(&object, "pyx_extension_get") > 0,
            "the extension attribute must reference pyx_extension_get"
        );
    }

    #[test]
    fn module_ref_lowers_to_pyx_module_value() {
        let program = program_assigning(Expr::new(
            ExprKind::ModuleRef {
                module: "errno".to_string(),
            },
            Type::Any,
            1,
        ));
        let object =
            crate::compile_object(&program).expect("a module value must lower to object code");
        assert!(
            runtime_call_sites(&object, "pyx_module_value") > 0,
            "the module value must reference pyx_module_value"
        );
    }

    #[test]
    fn getattr_builtin_lowers_to_pyx_getattr() {
        let module = Expr::new(
            ExprKind::ModuleRef {
                module: "errno".to_string(),
            },
            Type::Any,
            1,
        );
        let name = Expr::new(
            ExprKind::Const(Const::Str("EDOM".to_string())),
            Type::Str,
            1,
        );
        let program = program_assigning(Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Getattr),
                args: vec![module, name],
            },
            Type::Any,
            1,
        ));
        let object = crate::compile_object(&program).expect("getattr must lower to pyx_getattr");
        assert!(
            runtime_call_sites(&object, "pyx_getattr") > 0,
            "getattr() must reference pyx_getattr"
        );
    }

    #[test]
    fn dynamic_getattr_name_calls_the_native_builtin() {
        let module = Expr::new(
            ExprKind::ModuleRef {
                module: "errno".to_string(),
            },
            Type::Any,
            1,
        );
        let name = Expr::new(ExprKind::Local(0), Type::Str, 1);
        let program = program_assigning(Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Getattr),
                args: vec![module, name],
            },
            Type::Any,
            1,
        ));
        let object = crate::compile_object(&program)
            .expect("getattr with a dynamic name must lower through pyx_call");
        assert!(
            runtime_call_sites(&object, "pyx_call") > 0,
            "a dynamic getattr must call the native builtin through pyx_call"
        );
    }

    #[test]
    fn global_get_and_set_lower_to_the_runtime_imports() {
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![LocalInfo {
                    name: "x".to_string(),
                    ty: Type::Any,
                }],
                body: vec![
                    Stmt::new(
                        StmtKind::GlobalSet {
                            module: "__main__".to_string(),
                            name: "x".to_string(),
                            value: Expr::new(ExprKind::Const(Const::Int(1)), Type::Int, 1),
                        },
                        1,
                    ),
                    Stmt::new(
                        StmtKind::Assign {
                            target: 0,
                            value: Expr::new(
                                ExprKind::GlobalGet {
                                    module: "__main__".to_string(),
                                    name: "x".to_string(),
                                },
                                Type::Any,
                                2,
                            ),
                        },
                        2,
                    ),
                ],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program)
            .expect("module-global access must lower to object code");
        assert!(
            runtime_call_sites(&object, "pyx_global_set") > 0,
            "GlobalSet must reference pyx_global_set"
        );
        assert!(
            runtime_call_sites(&object, "pyx_global_get") > 0,
            "GlobalGet must reference pyx_global_get"
        );
    }

    #[test]
    fn int_range_loop_condition_avoids_the_runtime_binop() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        // `for i in range(0, 3, 1): pass` with the loop variable typed either
        // `int` (specialised condition and increment) or `Any` (fully dynamic).
        let range_program = |var_ty: Type| Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![LocalInfo {
                    name: "i".to_string(),
                    ty: var_ty,
                }],
                body: vec![Stmt::new(
                    StmtKind::ForRange {
                        var: 0,
                        start: int(0),
                        stop: int(3),
                        step: Some(int(1)),
                        body: Vec::new(),
                    },
                    1,
                )],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };

        let int_object =
            crate::compile_object(&range_program(Type::Int)).expect("int range loop must lower");
        let any_object =
            crate::compile_object(&range_program(Type::Any)).expect("Any range loop must lower");

        // The typed loop keeps `pyx_binop` only in the counter-increment
        // overflow slow block; the dynamic loop additionally calls it for the
        // step-direction test and both loop conditions.
        let int_sites = runtime_binop_call_sites(&int_object);
        let any_sites = runtime_binop_call_sites(&any_object);
        assert!(any_sites > 0, "the Any range loop must call pyx_binop");
        assert!(
            int_sites < any_sites,
            "the int loop condition must avoid pyx_binop: int={int_sites}, any={any_sites}"
        );
    }

    #[test]
    fn exception_hierarchy_and_chaining_lower_to_the_runtime_imports() {
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let pass = || Stmt::new(StmtKind::Pass, 2);
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![LocalInfo {
                    name: "exc".to_string(),
                    ty: Type::Any,
                }],
                body: vec![
                    Stmt::new(
                        StmtKind::Assign {
                            target: 0,
                            value: Expr::new(ExprKind::Const(Const::None), Type::None, 1),
                        },
                        1,
                    ),
                    Stmt::new(
                        StmtKind::Try {
                            body: vec![Stmt::new(
                                StmtKind::Raise {
                                    exc: Some(ExcKind::ValueError),
                                    msg: Some(text("boom")),
                                    cause: Some(text("origin")),
                                },
                                2,
                            )],
                            handlers: vec![
                                Handler {
                                    exc: Some(ExcKind::Exception),
                                    binding: None,
                                    body: vec![pass()],
                                },
                                Handler {
                                    exc: Some(ExcKind::BaseException),
                                    binding: None,
                                    body: vec![pass()],
                                },
                            ],
                            else_body: Vec::new(),
                            finally_body: Vec::new(),
                        },
                        1,
                    ),
                    // `raise <value> from <cause>` exercises the value-based
                    // chaining ABI.
                    Stmt::new(
                        StmtKind::Raise {
                            exc: None,
                            msg: Some(Expr::new(ExprKind::Local(0), Type::Any, 3)),
                            cause: Some(text("chained")),
                        },
                        3,
                    ),
                ],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program)
            .expect("exception hierarchy handlers and chaining must lower");
        for symbol in ["pyx_exc_matches", "pyx_raise_from", "pyx_raise_value_from"] {
            assert!(
                runtime_call_sites(&object, symbol) > 0,
                "the generated code must reference {symbol}"
            );
        }
    }

    #[test]
    fn assert_lowers_to_a_conditional_assertion_error_raise() {
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: Vec::new(),
                body: vec![
                    // `assert False, "boom"`: the message belongs to the
                    // failure branch only.
                    Stmt::new(
                        StmtKind::Assert {
                            test: Expr::new(ExprKind::Const(Const::Bool(false)), Type::Bool, 1),
                            msg: Some(text("boom")),
                        },
                        1,
                    ),
                    // `assert True`: a message-less assert still lowers.
                    Stmt::new(
                        StmtKind::Assert {
                            test: Expr::new(ExprKind::Const(Const::Bool(true)), Type::Bool, 2),
                            msg: None,
                        },
                        2,
                    ),
                ],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program).expect("assert statements must lower");
        assert!(
            runtime_call_sites(&object, "pyx_raise") > 0,
            "assert must raise through the runtime"
        );
        assert!(
            runtime_call_sites(&object, "pyx_truthy") > 0,
            "assert must test the condition dynamically"
        );
        assert!(
            runtime_call_sites(&object, "pyx_str_new") > 0,
            "the message must be materialised"
        );
    }

    #[test]
    fn conditional_expression_with_identity_test_lowers_to_an_object_file() {
        let none = || Expr::new(ExprKind::Const(Const::None), Type::None, 1);
        let test = Expr::new(
            ExprKind::Bin {
                op: BinOp::Is,
                lhs: Box::new(Expr::new(ExprKind::Local(0), Type::Any, 1)),
                rhs: Box::new(none()),
            },
            Type::Bool,
            1,
        );
        let value = Expr::new(
            ExprKind::IfExp {
                test: Box::new(test),
                then: Box::new(Expr::new(ExprKind::Const(Const::Int(7)), Type::Int, 1)),
                orelse: Box::new(none()),
            },
            Type::Any,
            1,
        );
        let object = crate::compile_object(&program_assigning(value))
            .expect("ternary and identity must lower");
        assert!(!object.is_empty());
        assert!(
            runtime_call_sites(&object, "pyx_binop") > 0,
            "identity must go through the runtime operator"
        );
    }

    #[test]
    fn direct_new_builtin_calls_lower_to_an_object_file() {
        let int = |value: i64| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let int_type = || Expr::new(ExprKind::BuiltinRef(Builtin::Int), Type::Function, 1);
        let none = || Expr::new(ExprKind::Const(Const::None), Type::None, 1);
        let cases: Vec<(Builtin, Vec<Expr>)> = vec![
            // The frontend appends the trailing key slot.
            (Builtin::Max, vec![int(1), none()]),
            (Builtin::Min, vec![int(1), none()]),
            // The frontend appends the key and reverse slots.
            (Builtin::Sorted, vec![int(1), none(), none()]),
            (Builtin::All, vec![int(1)]),
            (Builtin::Round, vec![int(1)]),
            (Builtin::Format, vec![int(1), text("d")]),
            (Builtin::Isinstance, vec![int(1), int_type()]),
        ];
        for (builtin, args) in cases {
            let program = program_assigning(Expr::new(
                ExprKind::Call {
                    callee: Callee::Builtin(builtin),
                    args,
                },
                Type::Any,
                1,
            ));
            let object = crate::compile_object(&program)
                .unwrap_or_else(|error| panic!("{builtin:?} must lower: {error}"));
            assert!(!object.is_empty(), "{builtin:?}");
        }
    }

    #[test]
    fn dataclass_style_init_lowers_to_an_object_file() {
        // `class Bench:` with a synthesized `__init__(self, key)`.
        let self_local = || Expr::new(ExprKind::Local(0), Type::Any, 1);
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![
                Function {
                    name: "<module>".to_string(),
                    params: Vec::new(),
                    capture_count: 0,
                    ret_ty: Type::None,
                    locals: vec![LocalInfo {
                        name: "Bench".to_string(),
                        ty: Type::Any,
                    }],
                    body: vec![Stmt::new(
                        StmtKind::ClassDef {
                            target: 0,
                            name: "Bench".to_string(),
                            base: None,
                            methods: vec![ClassMethod {
                                name: "__init__".to_string(),
                                function: 1,
                                kind: MethodKind::Normal,
                            }],
                            attributes: Vec::new(),
                        },
                        1,
                    )],
                    is_async: false,
                    is_generator: false,
                },
                Function {
                    name: "Bench.__init__".to_string(),
                    params: vec![
                        Param {
                            local: 0,
                            name: "self".to_string(),
                            ty: Type::Any,
                            default: None,
                            kind: ParamKind::Positional,
                        },
                        Param {
                            local: 1,
                            name: "key".to_string(),
                            ty: Type::Str,
                            default: None,
                            kind: ParamKind::Positional,
                        },
                    ],
                    capture_count: 0,
                    ret_ty: Type::None,
                    locals: vec![
                        LocalInfo {
                            name: "self".to_string(),
                            ty: Type::Any,
                        },
                        LocalInfo {
                            name: "key".to_string(),
                            ty: Type::Str,
                        },
                    ],
                    body: vec![
                        Stmt::new(
                            StmtKind::SetAttr {
                                object: self_local(),
                                name: "key".to_string(),
                                value: Expr::new(ExprKind::Local(1), Type::Str, 1),
                            },
                            1,
                        ),
                        Stmt::new(StmtKind::Return(None), 1),
                    ],
                    is_async: false,
                    is_generator: false,
                },
            ],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program).expect("dataclass __init__ must lower");
        assert!(!object.is_empty());
        assert!(
            runtime_call_sites(&object, "pyx_setattr") > 0,
            "the synthesized __init__ must store its fields"
        );
    }

    #[test]
    fn nested_and_attribute_unpack_targets_lower() {
        fn local(id: u32) -> Expr {
            Expr::new(ExprKind::Local(id), Type::Any, 1)
        }
        let names = ["obj", "items", "a", "b", "t"];
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: names
                    .iter()
                    .map(|name| LocalInfo {
                        name: (*name).to_string(),
                        ty: Type::Any,
                    })
                    .collect(),
                body: vec![Stmt::new(
                    StmtKind::Unpack {
                        targets: vec![
                            UnpackTarget::Attr {
                                object: local(0),
                                name: "x".to_string(),
                            },
                            UnpackTarget::Subscript {
                                object: local(1),
                                index: Expr::new(ExprKind::Const(Const::Int(0)), Type::Int, 1),
                            },
                            UnpackTarget::Nested(vec![
                                UnpackTarget::Name(2),
                                UnpackTarget::Name(3),
                            ]),
                        ],
                        value: local(4),
                    },
                    1,
                )],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object =
            crate::compile_object(&program).expect("unpack targets must lower to object code");
        assert!(!object.is_empty());
        assert!(runtime_call_sites(&object, "pyx_unpack_seq") > 0);
        assert!(runtime_call_sites(&object, "pyx_setattr") > 0);
        assert!(runtime_call_sites(&object, "pyx_subscript_set") > 0);
    }

    #[test]
    fn slice_assignment_lowers_to_the_slice_set_runtime_helper() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let none = || Expr::new(ExprKind::Const(Const::None), Type::None, 1);
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![LocalInfo {
                    name: "xs".to_string(),
                    ty: Type::List,
                }],
                body: vec![Stmt::new(
                    StmtKind::SetSlice {
                        target: Expr::new(ExprKind::Local(0), Type::List, 1),
                        start: int(1),
                        stop: none(),
                        step: none(),
                        value: Expr::new(ExprKind::List(vec![int(9)]), Type::List, 1),
                    },
                    1,
                )],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object =
            crate::compile_object(&program).expect("slice assignment must lower to object code");
        assert!(!object.is_empty());
        assert!(
            runtime_call_sites(&object, "pyx_slice_set") > 0,
            "slice assignment must call pyx_slice_set"
        );
        assert!(
            runtime_call_sites(&object, "pyx_list_new") > 0,
            "the right-hand side list must be materialised"
        );
    }

    #[test]
    fn starred_unpack_lowers_to_the_star_runtime_helper() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![Function {
                name: "<module>".to_string(),
                params: Vec::new(),
                capture_count: 0,
                ret_ty: Type::None,
                locals: vec![
                    LocalInfo {
                        name: "key".to_string(),
                        ty: Type::Any,
                    },
                    LocalInfo {
                        name: "value".to_string(),
                        ty: Type::Any,
                    },
                    LocalInfo {
                        name: "default".to_string(),
                        ty: Type::Any,
                    },
                ],
                body: vec![Stmt::new(
                    StmtKind::Unpack {
                        targets: vec![
                            UnpackTarget::Name(0),
                            UnpackTarget::Name(1),
                            UnpackTarget::Star(Box::new(UnpackTarget::Name(2))),
                        ],
                        value: Expr::new(
                            ExprKind::List(vec![int(1), int(2), int(3)]),
                            Type::List,
                            1,
                        ),
                    },
                    1,
                )],
                is_async: false,
                is_generator: false,
            }],
            main: 0,
            initializers: Vec::new(),
        };
        let object =
            crate::compile_object(&program).expect("starred unpack must lower to object code");
        assert!(runtime_call_sites(&object, "pyx_unpack_star") > 0);
    }

    #[test]
    fn tuple_and_hasattr_builtins_lower_to_their_runtime_helpers() {
        let text =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let tuple = Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Tuple),
                args: vec![text("ab")],
            },
            Type::Tuple,
            1,
        );
        let hasattr = Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Hasattr),
                args: vec![tuple, text("count")],
            },
            Type::Bool,
            1,
        );
        let object =
            crate::compile_object(&program_assigning(hasattr)).expect("hasattr must lower");
        assert!(runtime_call_sites(&object, "pyx_tuple_from") > 0);
        assert!(runtime_call_sites(&object, "pyx_hasattr") > 0);
    }

    #[test]
    fn keyword_module_calls_lower_to_the_kw_module_abi() {
        let int = |value| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let call = Expr::new(
            ExprKind::ModuleCall {
                module: "functools".to_string(),
                function: "partial".to_string(),
                args: vec![Expr::new(ExprKind::Local(0), Type::Any, 1)],
                keywords: vec![("angular".to_string(), int(1))],
            },
            Type::Any,
            1,
        );
        let object =
            crate::compile_object(&program_assigning(call)).expect("keyword module call lowers");
        assert!(runtime_call_sites(&object, "pyx_module_call_kw") > 0);
    }

    #[test]
    fn function_values_carry_their_parameter_names() {
        let parameter = |local: LocalId, name: &str| Param {
            local,
            name: name.to_string(),
            ty: Type::Any,
            default: None,
            kind: ParamKind::Positional,
        };
        let target = Function {
            name: "pair".to_string(),
            params: vec![parameter(0, "left"), parameter(1, "right")],
            capture_count: 0,
            ret_ty: Type::Any,
            locals: vec![
                LocalInfo {
                    name: "left".to_string(),
                    ty: Type::Any,
                },
                LocalInfo {
                    name: "right".to_string(),
                    ty: Type::Any,
                },
            ],
            body: vec![Stmt::new(
                StmtKind::Return(Some(Expr::new(ExprKind::Local(0), Type::Any, 1))),
                1,
            )],
            is_async: false,
            is_generator: false,
        };
        let module_body = Function {
            name: "<module>".to_string(),
            params: Vec::new(),
            capture_count: 0,
            ret_ty: Type::None,
            locals: vec![LocalInfo {
                name: "f".to_string(),
                ty: Type::Function,
            }],
            body: vec![Stmt::new(
                StmtKind::Assign {
                    target: 0,
                    value: Expr::new(ExprKind::FunctionRef(1), Type::Function, 1),
                },
                1,
            )],
            is_async: false,
            is_generator: false,
        };
        let program = Program {
            source_name: "test.py".to_string(),
            functions: vec![module_body, target],
            main: 0,
            initializers: Vec::new(),
        };
        let object = crate::compile_object(&program).expect("function reference must lower");
        assert!(runtime_call_sites(&object, "pyx_function_params") > 0);
    }

    #[test]
    fn not_implemented_constant_lowers_to_the_reserved_tag() {
        let program = program_assigning(Expr::new(
            ExprKind::Const(Const::NotImplemented),
            Type::Any,
            1,
        ));
        let object = crate::compile_object(&program).expect("NotImplemented must lower");
        assert!(!object.is_empty());
    }

    #[test]
    fn invert_lowers_to_pyx_unop() {
        let operand = || Expr::new(ExprKind::Const(Const::Int(5)), Type::Int, 1);
        let program = program_assigning(Expr::new(
            ExprKind::Un {
                op: pyx_ir::UnOp::Invert,
                operand: Box::new(operand()),
            },
            Type::Int,
            1,
        ));
        let object = crate::compile_object(&program).expect("invert must lower");
        assert!(runtime_call_sites(&object, "pyx_unop") > 0);
    }

    #[test]
    fn hash_builtin_lowers_to_pyx_hash() {
        let int = |value: i64| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        let program = program_assigning(Expr::new(
            ExprKind::Call {
                callee: Callee::Builtin(Builtin::Hash),
                args: vec![int(7)],
            },
            Type::Int,
            1,
        ));
        let object = crate::compile_object(&program).expect("hash must lower");
        assert!(runtime_call_sites(&object, "pyx_hash") > 0);
    }

    #[test]
    fn ord_and_chr_builtins_lower_to_their_abi_entries() {
        let string =
            |value: &str| Expr::new(ExprKind::Const(Const::Str(value.to_string())), Type::Str, 1);
        let int = |value: i64| Expr::new(ExprKind::Const(Const::Int(value)), Type::Int, 1);
        for (builtin, arg, symbol) in [
            (Builtin::Ord, string("A"), "pyx_ord"),
            (Builtin::Chr, int(65), "pyx_chr"),
        ] {
            let program = program_assigning(Expr::new(
                ExprKind::Call {
                    callee: Callee::Builtin(builtin),
                    args: vec![arg],
                },
                Type::Any,
                1,
            ));
            let object = crate::compile_object(&program).expect("builtin must lower");
            assert!(
                runtime_call_sites(&object, symbol) > 0,
                "{symbol} was not called"
            );
        }
    }
}
