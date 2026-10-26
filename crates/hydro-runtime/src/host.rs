//! Bridge between the runtime and the tree-walk interpreter (`hydro-interp`).
//!
//! Interpreter functions are ordinary `FunctionBody::Host`
//! values: they carry a program-registry token, their captured values and the
//! same parameter metadata a generated wrapper would have. The runtime calls
//! back into the interpreter through a process-wide dispatcher installed by
//! the interpreter front end, so native code (`sorted(key=...)`, class
//! methods, `threading.Thread`, `functools.partial`, ...) can invoke
//! interpreted functions without knowing that no machine code exists.
//!
//! # GC discipline
//!
//! Every call into the interpreter roots the complete argument buffer
//! (`captures` followed by the bound parameter slots) with
//! [`crate::heap::root_values`] before the invocation and releases it
//! afterwards. The interpreter additionally registers its own frame slots and
//! temporary values as precise roots, so no tagged value crosses an
//! allocation without a root. Generator/coroutine creation roots the payload
//! while the fiber object (which then traces the values itself) is allocated.

use std::sync::RwLock;

use hydro_ir::ParamKind;

use crate::{exc, heap, value};

/// Signature of the interpreter callback installed by `hydro-interp`.
///
/// `token` identifies the IR function; `args` holds the captured values
/// followed by one fully bound value per declared parameter (the runtime
/// keyword binder has already applied defaults, `*args` and `**kwargs`).
pub type HostDispatch = fn(token: u64, args: &[i64]) -> i64;

static DISPATCH: RwLock<Option<HostDispatch>> = RwLock::new(None);

/// Installs (or replaces) the interpreter callback.
pub fn set_dispatch(dispatch: HostDispatch) {
    let mut slot = DISPATCH
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(dispatch);
}

fn dispatcher() -> Option<HostDispatch> {
    *DISPATCH
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One parameter of an interpreter function value, in declaration order.
///
/// `default` is the already-evaluated default value (a literal constant or the
/// result of a `default_expr` evaluated where the function value was created).
#[derive(Clone, Debug)]
pub struct HostParam {
    pub name: String,
    pub kind: ParamKind,
    pub default: Option<i64>,
}

/// Allocates a function value backed by the interpreter.
///
/// The values in `captures` and every `HostParam::default` are rooted while
/// the function object is allocated; `token` is opaque to the runtime.
pub fn function_new(
    token: u64,
    captures: Vec<i64>,
    arity: usize,
    name: String,
    is_async: bool,
    is_generator: bool,
    params: Vec<HostParam>,
) -> i64 {
    let flags = u32::from(is_async)
        | (u32::from(is_generator) << 1)
        | (u32::from(
            params
                .iter()
                .any(|param| param.kind != ParamKind::Positional),
        ) << 2);
    let mut rooted: Vec<i64> = captures.clone();
    rooted.extend(params.iter().filter_map(|param| param.default));
    let roots = heap::root_values(&rooted);
    let specs = params
        .into_iter()
        .map(|param| (param.name, param.kind, param.default))
        .collect();
    let function = value::host_function_new(token, captures, arity, name, flags, specs);
    heap::release_roots(roots);
    function
}

/// `NotImplemented` as a tagged value (`Const::NotImplemented`).
pub fn not_implemented() -> i64 {
    value::NOT_IMPLEMENTED
}

/// `None` as a tagged value, for interpreter-internal initialisation.
pub fn none() -> i64 {
    value::NONE
}

/// Encodes a Rust integer as a tagged immediate value.
///
/// IR `Const::Int` and internal indices always fit the tagged range; the
/// fallback keeps a malformed value from silently aliasing a different
/// integer (`hydro_int` treats already-tagged input as identity).
pub fn tagged_int(value: i64) -> i64 {
    value::encode_int(value).unwrap_or(value)
}

/// Calls an interpreter function from the runtime.
///
/// `captures` and `args` normally come straight from a `Host` body; this
/// helper is used by the generic call paths after keyword binding.
pub(crate) fn call_slots(
    token: u64,
    captures: &[i64],
    args: &[i64],
    flags: u32,
    name: &str,
) -> i64 {
    if flags & (value::FUNCTION_FLAG_ASYNC | value::FUNCTION_FLAG_GENERATOR) != 0 {
        return fiber_new(
            token,
            captures,
            args,
            flags & value::FUNCTION_FLAG_ASYNC != 0,
            flags & value::FUNCTION_FLAG_GENERATOR != 0,
            name,
        );
    }
    let mut buffer: Vec<i64> = Vec::with_capacity(captures.len() + args.len());
    buffer.extend_from_slice(captures);
    buffer.extend_from_slice(args);
    let roots = heap::root_values(&buffer);
    let result = dispatch(token, &buffer);
    heap::release_roots(roots);
    result
}

/// Creates a lazy generator/coroutine fiber whose body is an interpreter
/// function.
///
/// The fiber payload is `[token, value-count, captures..., slots...]`; the
/// fiber entry point decodes it and dispatches to the interpreter on first
/// resume. `slots` must already hold one fully bound value per parameter.
pub fn fiber_new(
    token: u64,
    captures: &[i64],
    slots: &[i64],
    is_async: bool,
    is_generator: bool,
    name: &str,
) -> i64 {
    let Some(encoded_token) = value::encode_int(token as i64) else {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "function token does not fit a value");
    };
    let count = captures.len() + slots.len();
    let Some(encoded_count) = value::encode_int(count as i64) else {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "too many function arguments");
    };
    let mut payload: Vec<i64> = Vec::with_capacity(count + 2);
    payload.push(encoded_token);
    payload.push(encoded_count);
    payload.extend_from_slice(captures);
    payload.extend_from_slice(slots);
    let roots = heap::root_values(&payload);
    let created = if is_async {
        if is_generator {
            value::fiber_async_generator_new(
                fiber_entry as *const () as usize,
                name.to_string(),
                payload,
            )
        } else {
            value::fiber_coroutine_new(fiber_entry as *const () as usize, name.to_string(), payload)
        }
    } else {
        value::fiber_generator_new(fiber_entry as *const () as usize, name.to_string(), payload)
    };
    heap::release_roots(roots);
    match created {
        Some(fiber) => fiber,
        None => exc::raise_with_message(
            exc::RUNTIME_ERROR,
            "cannot allocate a generator/coroutine stack",
        ),
    }
}

/// Fiber entry point: decodes the token and argument count, then re-enters the
/// interpreter on the fiber stack.
extern "C" fn fiber_entry(argv: *const i64) -> i64 {
    if argv.is_null() {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "interpreter fiber has no arguments");
    }
    // SAFETY: the fiber owns a `[token, count, values...]` buffer created by
    // `fiber_new` and passes its pointer here on every resume.
    let token = unsafe { value::as_integer(*argv) }.and_then(|token| u64::try_from(token).ok());
    let count =
        unsafe { value::as_integer(*argv.add(1)) }.and_then(|count| usize::try_from(count).ok());
    let (Some(token), Some(count)) = (token, count) else {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "interpreter fiber payload is corrupt");
    };
    // SAFETY: `count` was recorded together with the buffer; the values
    // follow the two header words.
    let args = unsafe { std::slice::from_raw_parts(argv.add(2), count) };
    dispatch(token, args)
}

fn dispatch(token: u64, args: &[i64]) -> i64 {
    match dispatcher() {
        Some(dispatch) => dispatch(token, args),
        None => exc::raise_with_message(
            exc::RUNTIME_ERROR,
            "no interpreter is installed for this program",
        ),
    }
}
