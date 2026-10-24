//! Native implementations for the builtin modules declared in
//! `pyx_ir::stdlib` (Phase 2.10).
//!
//! The central spec is the single source of truth for names, arities and
//! result types. `pyx_module_call` dispatches a direct `module.function(...)`
//! call, `pyx_module_get` materialises constants and native function values,
//! and [`call_native`] handles calls through a native function value
//! (including `map`/`filter`/`functools.reduce` callbacks).
//!
//! All fallible paths report a pending exception and return the `None` dummy
//! value; no user input can panic.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pyx_ir::stdlib;
use pyx_ir::{PYTHON_VERSION, PYTHON_VERSION_STRING};

pub(crate) mod argparse;
pub(crate) mod asyncio;
pub(crate) mod collections;
pub(crate) mod errno;
pub(crate) mod extension;
pub(crate) mod re;
pub(crate) mod socket;
pub(crate) mod unittest;

use crate::builtins;
use crate::exc::{
    self, ATTRIBUTE_ERROR, INDEX_ERROR, OS_ERROR, OVERFLOW_ERROR, RUNTIME_ERROR, TYPE_ERROR,
    VALUE_ERROR,
};
use crate::heap;
use crate::iter;
use crate::ops;
use crate::process;
use crate::serialize;
use crate::value::{self, Num, FALSE, NONE, TRUE};

/// Maximum length accepted for an ABI name buffer (`isize::MAX` keeps the
/// slice representable).
const MAX_NAME_LEN: usize = isize::MAX as usize;

/// Maximum argument count whose byte length still fits in `isize`.
const MAX_ARGS: usize = MAX_NAME_LEN / std::mem::size_of::<i64>();

/// Borrows a UTF-8 name from the ABI buffer without allocating. `NULL`, empty,
/// over-long and non-UTF-8 buffers decode to `None`; those are error paths,
/// where callers fall back to [`decode_name`] to preserve the historical lossy
/// rendering.
fn name_str<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() || len == 0 || len > MAX_NAME_LEN {
        return None;
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable; the
    // length check above keeps the slice representable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).ok()
}

/// Copies a UTF-8 name from the ABI buffer. `NULL`, empty and over-long
/// buffers decode to the empty string, which then fails the module lookup.
/// Only used on error paths; hot paths borrow through [`name_str`].
fn decode_name(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 || len > MAX_NAME_LEN {
        return String::new();
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable; the
    // length check above keeps the slice representable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// One resolved native call. Names identify the call-site buffers by
/// pointer/length pair; the byte comparison in [`NativeCache::lookup`]
/// rechecks the contents so a recycled pointer with different text cannot
/// produce a false hit.
#[derive(Clone, Copy)]
struct NativeCacheEntry {
    module_ptr: usize,
    module_len: usize,
    function_ptr: usize,
    function_len: usize,
    /// Interned from `ModuleSpec::name`; never allocated per call.
    module: &'static str,
    /// Interned from `FunctionSpec::name`; never allocated per call.
    function: &'static str,
    spec: &'static stdlib::FunctionSpec,
}

const NATIVE_CACHE_SIZE: usize = 8;

/// Bounded FIFO cache of resolved native calls. Resolving a name scans the
/// central spec table; repeated `module.function(...)` call sites instead pay
/// a pointer/length probe plus a byte comparison and reuse the interned names.
struct NativeCache {
    entries: [Option<NativeCacheEntry>; NATIVE_CACHE_SIZE],
    next: usize,
    #[cfg(test)]
    hits: usize,
}

thread_local! {
    static NATIVE_CACHE: RefCell<NativeCache> = const {
        RefCell::new(NativeCache {
            entries: [None; NATIVE_CACHE_SIZE],
            next: 0,
            #[cfg(test)]
            hits: 0,
        })
    };
}

impl NativeCache {
    fn lookup(
        &mut self,
        module_ptr: usize,
        module_len: usize,
        module: &str,
        function_ptr: usize,
        function_len: usize,
        function: &str,
    ) -> Option<NativeCacheEntry> {
        let entry = self.entries.iter().flatten().copied().find(|entry| {
            entry.module_ptr == module_ptr
                && entry.module_len == module_len
                && entry.function_ptr == function_ptr
                && entry.function_len == function_len
                && entry.module.as_bytes() == module.as_bytes()
                && entry.function.as_bytes() == function.as_bytes()
        });
        #[cfg(test)]
        if entry.is_some() {
            self.hits += 1;
        }
        entry
    }

    fn insert(&mut self, entry: NativeCacheEntry) {
        self.entries[self.next] = Some(entry);
        self.next += 1;
        if self.next == NATIVE_CACHE_SIZE {
            self.next = 0;
        }
    }
}

/// Resolves a native call without the cache. The spec table owns the names,
/// so they are interned as `&'static str` and error rendering never allocates
/// on the hit path.
fn resolve_native(
    module: &str,
    function: &str,
) -> Option<(&'static str, &'static str, &'static stdlib::FunctionSpec)> {
    let spec = stdlib::function(module, function)?;
    let module_name = stdlib::module(module)?.name;
    Some((module_name, spec.name, spec))
}

/// Resolves `module.function` through the per-thread cache, falling back to a
/// spec-table scan on a miss. Empty names (invalid ABI buffers) are never
/// cached and simply fail the lookup.
fn native_spec(
    module_ptr: usize,
    module_len: usize,
    module: &str,
    function_ptr: usize,
    function_len: usize,
    function: &str,
) -> Option<(&'static str, &'static str, &'static stdlib::FunctionSpec)> {
    if module_len == 0 || function_len == 0 {
        return None;
    }
    let cached = NATIVE_CACHE.try_with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(entry) = cache.lookup(
            module_ptr,
            module_len,
            module,
            function_ptr,
            function_len,
            function,
        ) {
            return Some((entry.module, entry.function, entry.spec));
        }
        let resolved = resolve_native(module, function)?;
        cache.insert(NativeCacheEntry {
            module_ptr,
            module_len,
            function_ptr,
            function_len,
            module: resolved.0,
            function: resolved.1,
            spec: resolved.2,
        });
        Some(resolved)
    });
    match cached {
        Ok(resolved) => resolved,
        // The thread-local is unavailable only while thread state is being
        // torn down; resolve directly rather than panic.
        Err(_) => resolve_native(module, function),
    }
}

fn missing_attribute(module: &str, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{module}' has no attribute '{name}'"),
    )
}

fn failed() -> bool {
    exc::pending()
}

fn encode_int(n: i64) -> i64 {
    value::encode_int(n)
        .unwrap_or_else(|| exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"))
}

// -------------------------------------------------------------------------
// ABI entry helpers
// -------------------------------------------------------------------------

/// `pyx_module_call`: direct `module.function(args...)` call.
pub(crate) fn module_call(
    module_ptr: *const u8,
    module_len: usize,
    function_ptr: *const u8,
    function_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    if let (Some(module), Some(function)) = (
        name_str(module_ptr, module_len),
        name_str(function_ptr, function_len),
    ) {
        return call_native_at(
            module_ptr as usize,
            module_len,
            module,
            function_ptr as usize,
            function_len,
            function,
            argc,
            argv,
        );
    }
    // Invalid buffer or non-UTF-8 name: decode lossily so the resulting
    // error message matches the historical rendering.
    let module = decode_name(module_ptr, module_len);
    let function = decode_name(function_ptr, function_len);
    call_native(&module, &function, argc, argv)
}

/// `pyx_module_get`: returns a constant value or a native function object.
pub(crate) fn module_get(
    module_ptr: *const u8,
    module_len: usize,
    name_ptr: *const u8,
    name_len: usize,
) -> i64 {
    if let (Some(module), Some(name)) = (
        name_str(module_ptr, module_len),
        name_str(name_ptr, name_len),
    ) {
        return module_get_named(module, name);
    }
    // Invalid buffer or non-UTF-8 name: decode lossily so the resulting
    // error message matches the historical rendering.
    let module = decode_name(module_ptr, module_len);
    let name = decode_name(name_ptr, name_len);
    module_get_named(&module, &name)
}

/// Resolves `module.name` for a module value receiver (`getattr(module, name)`
/// in `crate::methods`/`crate::builtins`).
pub(crate) fn module_get_by_name(module: &str, name: &str) -> i64 {
    module_get_named(module, name)
}

fn module_get_named(module: &str, name: &str) -> i64 {
    // `errno` attributes are platform-defined names resolved on demand; the
    // static spec table cannot enumerate them.
    if module == "errno" {
        return errno::lookup(name);
    }
    if module == "unittest" && name == "TestCase" {
        return unittest::test_case();
    }
    if let Some(spec) = stdlib::function(module, name) {
        let module_name = stdlib::module(module).map(|m| m.name).unwrap_or("<module>");
        return value::native_function_new(module_name, spec.name, spec.max_args);
    }
    if stdlib::constant(module, name).is_some() {
        return constant_value(module, name);
    }
    missing_attribute(module, name)
}

/// Calls a native function by name, validating it against the central spec.
/// Shared by `pyx_module_call` and calls through native function values.
pub(crate) fn call_native(module: &str, function: &str, argc: usize, argv: *const i64) -> i64 {
    // `FunctionBody::Native` stores `&'static str`, so the pointer identity
    // used as the cache key is stable; the byte recheck still guards any
    // caller that passes an ordinary borrow.
    call_native_at(
        module.as_ptr() as usize,
        module.len(),
        module,
        function.as_ptr() as usize,
        function.len(),
        function,
        argc,
        argv,
    )
}

#[allow(clippy::too_many_arguments)]
fn call_native_at(
    module_ptr: usize,
    module_len: usize,
    module: &str,
    function_ptr: usize,
    function_len: usize,
    function: &str,
    argc: usize,
    argv: *const i64,
) -> i64 {
    let Some((module_name, function_name, spec)) = native_spec(
        module_ptr,
        module_len,
        module,
        function_ptr,
        function_len,
        function,
    ) else {
        return missing_attribute(module, function);
    };
    if argc < spec.min_args || argc > spec.max_args {
        let detail = if spec.min_args == spec.max_args {
            format!("{} arguments", spec.max_args)
        } else {
            format!("{} to {} arguments", spec.min_args, spec.max_args)
        };
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{module_name}.{function_name}() takes {detail} ({argc} given)"),
        );
    }
    if argc > MAX_ARGS {
        return exc::raise_with_message(TYPE_ERROR, "argument count is out of range");
    }
    let args: &[i64] = if argc == 0 {
        &[]
    } else if argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("function call received {argc} argument(s) without an argument buffer"),
        );
    } else {
        // SAFETY: the ABI requires `argv..argv+argc` to be readable and the
        // length check above keeps the slice representable.
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    dispatch(module_name, function_name, args)
}

fn dispatch(module: &str, function: &str, args: &[i64]) -> i64 {
    match module {
        "math" => math_call(function, args),
        "functools" => reduce(args),
        "operator" => operator_call(function, args),
        "time" => time_call(function, args),
        "_thread" => thread_call(function, args),
        "threading" => threading_call(function, args),
        "queue" => queue_call(function, args),
        "multiprocessing" => multiprocessing_call(function, args),
        "pickle" => pickle_call(function, args),
        "json" => json_call(function, args),
        "csv" => csv_call(function, args),
        "io" => io_call(function, args),
        "os" => os_call(function, args),
        "os.path" => os_path_call(function, args),
        "random" => random_call(function, args),
        "pathlib" => pathlib_call(function, args),
        "platform" => platform_call(function),
        "statistics" => statistics_call(function, args),
        "argparse" => argparse::call(function, args),
        "collections" => collections::call(function, args),
        "re" => re::call(function, args),
        "socket" => socket::call(function, args),
        "asyncio" => asyncio::call(function, args),
        "unittest" => unittest::call(function, args),
        _ => missing_attribute(module, function),
    }
}

// -------------------------------------------------------------------------
// Constants
// -------------------------------------------------------------------------

fn constant_value(module: &str, name: &str) -> i64 {
    match (module, name) {
        ("math", "pi") => value::float_new(std::f64::consts::PI),
        ("math", "e") => value::float_new(std::f64::consts::E),
        ("math", "tau") => value::float_new(std::f64::consts::TAU),
        ("math", "inf") => value::float_new(f64::INFINITY),
        ("math", "nan") => value::float_new(f64::NAN),
        ("sys", "argv") => {
            let program = value::str_from_bytes(b"pyx");
            value::list_new(vec![program])
        }
        ("sys", "platform") => value::str_from_bytes(platform().as_bytes()),
        ("sys", "version") => value::str_alloc(format!(
            "{PYTHON_VERSION_STRING} (PyX {})",
            env!("CARGO_PKG_VERSION")
        )),
        ("sys", "version_info") => value::tuple_new(vec![
            encode_int(i64::from(PYTHON_VERSION.0)),
            encode_int(i64::from(PYTHON_VERSION.1)),
            encode_int(i64::from(PYTHON_VERSION.2)),
            value::str_from_bytes(b"final"),
            encode_int(0),
        ]),
        ("sys", "pyx_version") => value::str_from_bytes(env!("CARGO_PKG_VERSION").as_bytes()),
        ("sys", "pyx_version_info") => value::tuple_new(vec![
            encode_int(0),
            encode_int(1),
            encode_int(0),
            value::str_from_bytes(b"dev"),
            encode_int(0),
        ]),
        // `i64::MAX` does not fit into the 61-bit tagged integer; the
        // documented value is `2^60 - 1` (`value::MAX_INT`).
        ("sys", "maxsize") => encode_int(value::MAX_INT),
        ("os", "name") => {
            let name = if cfg!(windows) { "nt" } else { "posix" };
            value::str_from_bytes(name.as_bytes())
        }
        ("os", "sep") => {
            let sep = if cfg!(windows) { "\\" } else { "/" };
            value::str_from_bytes(sep.as_bytes())
        }
        ("socket", "AF_INET") => encode_int(2),
        ("socket", "SOCK_STREAM") => encode_int(1),
        ("socket", "SOL_SOCKET") => encode_int(1),
        ("socket", "SO_REUSEADDR") => encode_int(2),
        ("socket", "SHUT_RD") => encode_int(0),
        ("socket", "SHUT_WR") => encode_int(1),
        ("socket", "SHUT_RDWR") => encode_int(2),
        _ => missing_attribute(module, name),
    }
}

fn platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

// -------------------------------------------------------------------------
// random
// -------------------------------------------------------------------------

/// Global generator behind the module-level `random` functions. Seeded from
/// the environment on first use.
static GLOBAL_RANDOM: LazyLock<value::RandomState> = LazyLock::new(value::RandomState::new);

fn random_type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Converts a `random` seed (`int`, `float` or `None`) to 64 bits.
fn seed_u64(v: i64) -> Result<u64, i64> {
    if let Some(integer) = value::as_integer(v) {
        return Ok(integer as u64);
    }
    if let Some(float) = value::as_float(v) {
        return Ok(float.to_bits());
    }
    Err(random_type_error(
        "The only supported seed types are: int, float, None",
    ))
}

/// Integer argument of `randrange`/`randint`; non-integers report a pending
/// `TypeError`.
fn random_int_arg(v: i64) -> Result<i64, i64> {
    value::as_integer(v).ok_or_else(|| {
        random_type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        ))
    })
}

fn random_empty_range(start: i64, stop: i64, step: i64) -> i64 {
    exc::raise_with_message(
        VALUE_ERROR,
        &format!("empty range for randrange() ({start}, {stop}, {step})"),
    )
}

/// Draws a uniform offset in `0..width` for a width that may equal `2^64`.
fn random_offset(rng: &value::RandomState, width: i128) -> u64 {
    if width > u64::MAX as i128 {
        // The only representable width above `u64::MAX` is `2^64`; every
        // 64-bit draw covers it exactly once.
        rng.next_u64()
    } else {
        rng.below(width as u64)
    }
}

fn random_randint(rng: &value::RandomState, args: &[i64]) -> i64 {
    let low = match random_int_arg(args[0]) {
        Ok(v) => v,
        Err(err) => return err,
    };
    let high = match random_int_arg(args[1]) {
        Ok(v) => v,
        Err(err) => return err,
    };
    if low > high {
        return random_empty_range(low, high, 1);
    }
    let width = high as i128 - low as i128 + 1;
    let offset = random_offset(rng, width) as i128;
    encode_int((low as i128 + offset) as i64)
}

fn random_randrange(rng: &value::RandomState, args: &[i64]) -> i64 {
    let mut values = Vec::with_capacity(args.len());
    for &arg in args {
        match random_int_arg(arg) {
            Ok(v) => values.push(v),
            Err(err) => return err,
        }
    }
    let (start, stop, step) = match values[..] {
        [stop] => (0, stop, 1),
        [start, stop] => (start, stop, 1),
        [start, stop, step] => (start, stop, step),
        _ => return random_type_error("randrange() received an invalid argument count"),
    };
    if step == 0 {
        return exc::raise_with_message(VALUE_ERROR, "randrange() arg 3 must not be zero");
    }
    let start = start as i128;
    let stop = stop as i128;
    let step = step as i128;
    let width = if step > 0 {
        (stop - start + step - 1) / step
    } else {
        (stop - start + step + 1) / step
    };
    if width <= 0 {
        return random_empty_range(start as i64, stop as i64, step as i64);
    }
    let offset = random_offset(rng, width) as i128;
    encode_int((start + step * offset) as i64)
}

fn random_choice(rng: &value::RandomState, sequence: i64) -> i64 {
    if let Some(items) = value::as_list(sequence) {
        if items.is_empty() {
            return exc::raise_with_message(INDEX_ERROR, "Cannot choose from an empty sequence");
        }
        let index = rng.below(items.len() as u64) as usize;
        return items[index];
    }
    if let Some(items) = value::as_tuple(sequence) {
        if items.is_empty() {
            return exc::raise_with_message(INDEX_ERROR, "Cannot choose from an empty sequence");
        }
        let index = rng.below(items.len() as u64) as usize;
        return items[index];
    }
    if let Some(text) = value::as_str(sequence) {
        let count = text.chars().count();
        if count == 0 {
            return exc::raise_with_message(INDEX_ERROR, "Cannot choose from an empty sequence");
        }
        let index = rng.below(count as u64) as usize;
        let picked = text.chars().nth(index).unwrap_or('\0').to_string();
        return value::str_alloc(picked);
    }
    if let Some(bytes) = value::as_bytes(sequence) {
        if bytes.is_empty() {
            return exc::raise_with_message(INDEX_ERROR, "Cannot choose from an empty sequence");
        }
        let index = rng.below(bytes.len() as u64) as usize;
        return encode_int(bytes[index] as i64);
    }
    random_type_error(&format!(
        "choice() argument must be a sequence, not {}",
        value::type_name(sequence)
    ))
}

fn random_shuffle(rng: &value::RandomState, sequence: i64) -> i64 {
    let Some(mut items) = value::list_items_mut(sequence) else {
        return random_type_error(&format!(
            "shuffle() argument must be a list, not {}",
            value::type_name(sequence)
        ));
    };
    let len = items.len();
    for index in (1..len).rev() {
        let other = rng.below(index as u64 + 1) as usize;
        items.swap(index, other);
    }
    NONE
}

fn random_uniform(rng: &value::RandomState, args: &[i64]) -> i64 {
    let low = match number(args[0]) {
        Ok(v) => v,
        Err(err) => return err,
    };
    let high = match number(args[1]) {
        Ok(v) => v,
        Err(err) => return err,
    };
    value::float_new(low + (high - low) * rng.next_f64())
}

/// Shared implementation of the module-level functions and `Random` methods.
/// `seed` without an argument and `seed(None)` reseed from entropy.
fn random_operation(rng: &value::RandomState, function: &str, args: &[i64]) -> i64 {
    match function {
        "seed" => {
            match args.first().copied() {
                None => rng.reset_entropy(),
                Some(seed) if seed == NONE => rng.reset_entropy(),
                Some(seed) => match seed_u64(seed) {
                    Ok(seed) => rng.reset(seed),
                    Err(err) => return err,
                },
            }
            NONE
        }
        "random" => value::float_new(rng.next_f64()),
        "randint" => random_randint(rng, args),
        "randrange" => random_randrange(rng, args),
        "choice" => random_choice(rng, args[0]),
        "uniform" => random_uniform(rng, args),
        "shuffle" => random_shuffle(rng, args[0]),
        _ => missing_attribute("random", function),
    }
}

fn random_call(function: &str, args: &[i64]) -> i64 {
    if function == "Random" {
        let seed = match args.first().copied() {
            None => None,
            Some(seed) if seed == NONE => None,
            Some(seed) => match seed_u64(seed) {
                Ok(seed) => Some(seed),
                Err(err) => return err,
            },
        };
        return value::random_new(seed);
    }
    random_operation(&GLOBAL_RANDOM, function, args)
}

/// Arity check for `Random` methods (the module-level functions are checked
/// against the central spec before dispatch).
fn check_random_arity(method: &str, argc: usize, min: usize, max: usize) -> Option<i64> {
    if (min..=max).contains(&argc) {
        return None;
    }
    let detail = if min == max {
        format!("{min}")
    } else if argc < min {
        format!("at least {min}")
    } else {
        format!("at most {max}")
    };
    Some(random_type_error(&format!(
        "{method}() takes {detail} argument(s) ({argc} given)"
    )))
}

/// Dynamic `Random` method dispatch (`rng.seed(...)` etc.).
pub(crate) fn random_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(rng) = value::as_random(obj) else {
        return random_type_error("invalid Random receiver");
    };
    let (min, max) = match method {
        "seed" => (0, 1),
        "random" => (0, 0),
        "randint" => (2, 2),
        "randrange" => (1, 3),
        "choice" => (1, 1),
        "uniform" => (2, 2),
        "shuffle" => (1, 1),
        _ => {
            return exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("'Random' object has no attribute '{method}'"),
            )
        }
    };
    if let Some(err) = check_random_arity(method, args.len(), min, max) {
        return err;
    }
    random_operation(rng, method, args)
}

// -------------------------------------------------------------------------
// pathlib
// -------------------------------------------------------------------------

/// `pathlib.Path(path=None)`, `pathlib.cwd()` and `pathlib.home()`.
///
/// A keyword call arrives with `None` in every omitted slot (the frontend
/// fills the gaps), so `None` selects the documented default.
fn pathlib_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Path" => {
            let arg = args.first().copied().unwrap_or(NONE);
            if arg == NONE {
                return value::path_new(".".to_string());
            }
            if let Some(text) = value::as_str(arg) {
                return value::path_new(text.to_string());
            }
            if let Some(state) = value::as_path(arg) {
                return value::path_new(state.path().to_string());
            }
            exc::raise_with_message(
                TYPE_ERROR,
                &format!("Path() argument must be str, not {}", value::type_name(arg)),
            )
        }
        "cwd" => match std::env::current_dir() {
            Ok(path) => value::path_new(path.to_string_lossy().into_owned()),
            Err(error) => exc::raise_with_message(OS_ERROR, &format!("cwd: {error}")),
        },
        // `HOME` may be unset in minimal environments; CPython raises
        // `RuntimeError`, but the documented fallback here is `"."`.
        "home" => {
            let home = std::env::var_os("HOME")
                .map(|path| path.to_string_lossy().into_owned())
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| ".".to_string());
            value::path_new(home)
        }
        _ => missing_attribute("pathlib", function),
    }
}

// -------------------------------------------------------------------------
// platform
// -------------------------------------------------------------------------

/// Reads a kernel pseudo-file, trimming the trailing newline. Missing files
/// yield the empty string (portable fallback).
fn read_trimmed(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn platform_system() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows",
        other => other,
    }
}

fn platform_machine() -> &'static str {
    std::env::consts::ARCH
}

fn platform_release() -> String {
    if cfg!(target_os = "linux") {
        read_trimmed("/proc/sys/kernel/osrelease")
    } else {
        String::new()
    }
}

fn platform_version() -> String {
    if cfg!(target_os = "linux") {
        read_trimmed("/proc/sys/kernel/version")
    } else {
        String::new()
    }
}

fn platform_node() -> String {
    if let Ok(node) = std::env::var("HOSTNAME") {
        if !node.is_empty() {
            return node;
        }
    }
    if cfg!(target_os = "linux") {
        let node = read_trimmed("/proc/sys/kernel/hostname");
        if !node.is_empty() {
            return node;
        }
    }
    String::new()
}

fn platform_call(function: &str) -> i64 {
    match function {
        "system" => value::str_from_bytes(platform_system().as_bytes()),
        "machine" => value::str_from_bytes(platform_machine().as_bytes()),
        "release" => value::str_alloc(platform_release()),
        "version" => value::str_alloc(platform_version()),
        "node" => value::str_alloc(platform_node()),
        "platform" => value::str_alloc(format!(
            "{}-{}-{}",
            platform_system(),
            platform_release(),
            platform_machine()
        )),
        "python_version" => value::str_from_bytes(PYTHON_VERSION_STRING.as_bytes()),
        "python_implementation" => value::str_from_bytes(b"PyX"),
        _ => missing_attribute("platform", function),
    }
}

// -------------------------------------------------------------------------
// statistics
// -------------------------------------------------------------------------

fn statistics_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn statistic_number(v: i64) -> Result<f64, i64> {
    match value::num_of(v) {
        Some(Num::Int(integer)) => Ok(integer as f64),
        Some(Num::Float(float)) => Ok(float),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("data must be numeric, not {}", value::type_name(v)),
        )),
    }
}

/// Materialises the numeric view of a `list`, `tuple`, `range` or `bytes`.
fn statistics_values(data: i64) -> Result<Vec<f64>, i64> {
    if let Some(items) = value::as_list(data) {
        return items.iter().map(|&item| statistic_number(item)).collect();
    }
    if let Some(items) = value::as_tuple(data) {
        return items.iter().map(|&item| statistic_number(item)).collect();
    }
    if let Some(bytes) = value::as_bytes(data) {
        return Ok(bytes.iter().map(|&byte| byte as f64).collect());
    }
    if let Some((start, stop, step)) = value::as_range(data) {
        let mut values = Vec::new();
        let mut current = start as i128;
        let stop = stop as i128;
        let step = step as i128;
        while (step > 0 && current < stop) || (step < 0 && current > stop) {
            values.push(current as f64);
            current += step;
        }
        return Ok(values);
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "data must be a list, tuple, range or bytes, not {}",
            value::type_name(data)
        ),
    ))
}

fn statistics_mean(data: i64, function: &str) -> i64 {
    let values = match statistics_values(data) {
        Ok(values) => values,
        Err(err) => return err,
    };
    if values.is_empty() {
        return statistics_error(&format!("{function} requires at least one data point"));
    }
    let sum: f64 = values.iter().sum();
    value::float_new(sum / values.len() as f64)
}

fn statistics_median(data: i64) -> i64 {
    let mut values = match statistics_values(data) {
        Ok(values) => values,
        Err(err) => return err,
    };
    if values.is_empty() {
        return statistics_error("median requires at least one data point");
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = values.len() / 2;
    let median = if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    };
    value::float_new(median)
}

fn statistics_deviation(data: i64, sample: bool) -> i64 {
    let function = if sample { "stdev" } else { "pstdev" };
    let values = match statistics_values(data) {
        Ok(values) => values,
        Err(err) => return err,
    };
    if values.is_empty() {
        return statistics_error(&format!("{function} requires at least one data point"));
    }
    if sample && values.len() < 2 {
        return statistics_error("stdev requires at least two data points");
    }
    let count = values.len() as f64;
    let mean = values.iter().sum::<f64>() / count;
    let squared: f64 = values.iter().map(|value| (*value - mean).powi(2)).sum();
    let divisor = if sample { count - 1.0 } else { count };
    value::float_new((squared / divisor).sqrt())
}

fn statistics_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "mean" => statistics_mean(args[0], "mean"),
        "fmean" => statistics_mean(args[0], "fmean"),
        "median" => statistics_median(args[0]),
        "pstdev" => statistics_deviation(args[0], false),
        "stdev" => statistics_deviation(args[0], true),
        _ => missing_attribute("statistics", function),
    }
}

// -------------------------------------------------------------------------
// math
// -------------------------------------------------------------------------

fn domain_error() -> i64 {
    exc::raise_with_message(VALUE_ERROR, "math domain error")
}

fn range_error() -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, "math range error")
}

/// Numeric view of a `math` argument: int, bool and float are accepted.
fn number(v: i64) -> Result<f64, i64> {
    match value::num_of(v) {
        Some(Num::Int(i)) => Ok(i as f64),
        Some(Num::Float(f)) => Ok(f),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("must be real number, not {}", value::type_name(v)),
        )),
    }
}

fn math_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "sqrt" => match number(args[0]) {
            Ok(x) if x < 0.0 => domain_error(),
            Ok(x) => value::float_new(x.sqrt()),
            Err(err) => err,
        },
        "sin" | "cos" | "tan" => match number(args[0]) {
            // CPython rejects infinite inputs for the trigonometric family.
            Ok(x) if x.is_infinite() => domain_error(),
            Ok(x) => {
                let result = match function {
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    _ => x.tan(),
                };
                value::float_new(result)
            }
            Err(err) => err,
        },
        "asin" | "acos" => match number(args[0]) {
            Ok(x) => {
                if !x.is_nan() && !(-1.0..=1.0).contains(&x) {
                    domain_error()
                } else if function == "asin" {
                    value::float_new(x.asin())
                } else {
                    value::float_new(x.acos())
                }
            }
            Err(err) => err,
        },
        "atan" => match number(args[0]) {
            Ok(x) => value::float_new(x.atan()),
            Err(err) => err,
        },
        "exp" => match number(args[0]) {
            Ok(x) => {
                let result = x.exp();
                if result.is_infinite() && x.is_finite() {
                    range_error()
                } else {
                    value::float_new(result)
                }
            }
            Err(err) => err,
        },
        "log" | "log10" => match number(args[0]) {
            Ok(x) if !x.is_nan() && x <= 0.0 => domain_error(),
            Ok(x) => {
                let result = if function == "log" { x.ln() } else { x.log10() };
                value::float_new(result)
            }
            Err(err) => err,
        },
        "pow" => {
            let base = match number(args[0]) {
                Ok(value) => value,
                Err(err) => return err,
            };
            let exponent = match number(args[1]) {
                Ok(value) => value,
                Err(err) => return err,
            };
            if base == 0.0 && exponent < 0.0 {
                return domain_error();
            }
            if base < 0.0 && exponent.is_finite() && exponent.fract() != 0.0 {
                return domain_error();
            }
            let result = base.powf(exponent);
            if result.is_infinite() && base.is_finite() && exponent.is_finite() {
                range_error()
            } else {
                value::float_new(result)
            }
        }
        "fabs" => match number(args[0]) {
            Ok(x) => value::float_new(x.abs()),
            Err(err) => err,
        },
        "floor" | "ceil" | "trunc" => match number(args[0]) {
            Ok(x) => float_to_int(function, x),
            Err(err) => err,
        },
        _ => missing_attribute("math", function),
    }
}

/// Converts an integral float to a tagged int, matching CPython's
/// `math.floor`/`ceil`/`trunc` error for non-finite inputs.
fn float_to_int(function: &str, x: f64) -> i64 {
    if x.is_nan() {
        return exc::raise_with_message(OVERFLOW_ERROR, "cannot convert float NaN to integer");
    }
    if x.is_infinite() {
        return exc::raise_with_message(OVERFLOW_ERROR, "cannot convert float infinity to integer");
    }
    let rounded = match function {
        "floor" => x.floor(),
        "ceil" => x.ceil(),
        _ => x.trunc(),
    };
    if rounded < i64::MIN as f64 || rounded > i64::MAX as f64 {
        return exc::raise_with_message(OVERFLOW_ERROR, "integer overflow");
    }
    ops::int_result(rounded as i64)
}

// -------------------------------------------------------------------------
// functools
// -------------------------------------------------------------------------

/// `functools.reduce(function, iterable[, initial])`.
fn reduce(args: &[i64]) -> i64 {
    let function = args[0];
    if value::as_function(function).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "reduce() arg 1 must be a function");
    }
    let iterator = iter::iter_new(args[1]);
    if failed() {
        return NONE;
    }

    let mut accumulator;
    if args.len() == 3 {
        accumulator = args[2];
    } else {
        let mut first = NONE;
        let status = iter::iter_next(iterator, &mut first);
        if failed() {
            return NONE;
        }
        if status == 0 {
            return exc::raise_with_message(
                TYPE_ERROR,
                "reduce() of empty iterable with no initial value",
            );
        }
        accumulator = first;
    }

    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if failed() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let call_args = [accumulator, item];
        let next = builtins::call(function, 2, call_args.as_ptr());
        if failed() {
            return NONE;
        }
        accumulator = next;
    }
    accumulator
}

// -------------------------------------------------------------------------
// operator
// -------------------------------------------------------------------------

fn operator_call(function: &str, args: &[i64]) -> i64 {
    let binary = match function {
        "add" => Some(ops::OP_ADD),
        "sub" => Some(ops::OP_SUB),
        "mul" => Some(ops::OP_MUL),
        "truediv" => Some(ops::OP_DIV),
        "floordiv" => Some(ops::OP_FLOOR_DIV),
        "mod" => Some(ops::OP_MOD),
        "pow" => Some(ops::OP_POW),
        "eq" => Some(ops::OP_EQ),
        "ne" => Some(ops::OP_NE),
        "lt" => Some(ops::OP_LT),
        "le" => Some(ops::OP_LE),
        "gt" => Some(ops::OP_GT),
        "ge" => Some(ops::OP_GE),
        _ => None,
    };
    if let Some(op) = binary {
        return ops::binop(op, args[0], args[1]);
    }
    match function {
        "neg" => ops::unop(ops::OP_NEG, args[0]),
        "not_" => ops::unop(ops::OP_NOT, args[0]),
        "truth" => value::bool_value(value::truthy(args[0])),
        _ => missing_attribute("operator", function),
    }
}

// -------------------------------------------------------------------------
// time
// -------------------------------------------------------------------------

/// Epoch for `perf_counter`/`monotonic` (and their `_ns` variants), relative
/// to the first time-module call in this process.
static CLOCK_BASE: OnceLock<Instant> = OnceLock::new();

fn clock_base() -> &'static Instant {
    CLOCK_BASE.get_or_init(Instant::now)
}

fn monotonic_seconds() -> f64 {
    clock_base().elapsed().as_secs_f64()
}

fn monotonic_nanos() -> i64 {
    nanos_value(clock_base().elapsed().as_nanos())
}

fn wall_clock() -> Duration {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration,
        Err(err) => err.duration(),
    }
}

fn wall_clock_nanos() -> i64 {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH);
    match since_epoch {
        Ok(duration) => nanos_value(duration.as_nanos()),
        // A pre-1970 clock: represent as a negative nanosecond count.
        Err(err) => {
            let nanos = err.duration().as_nanos().min(value::MAX_INT as u128);
            -value::encode_int(nanos as i64).unwrap_or(value::MAX_INT << 3)
        }
    }
}

/// Encodes a nanosecond count as a tagged int.
///
/// The tagged representation only holds `2^60 - 1` (`value::MAX_INT`). The
/// current UNIX epoch in nanoseconds (~1.8e18) already exceeds that limit, so
/// `time_ns` saturates instead of raising: the documented Phase-1 limit
/// applies, and the ABI still returns a positive int.
fn nanos_value(nanos: u128) -> i64 {
    let clamped = nanos.min(value::MAX_INT as u128);
    // `clamped` is representable by construction.
    value::encode_int(clamped as i64).unwrap_or(value::MAX_INT << 3)
}

fn time_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "time" => value::float_new(wall_clock().as_secs_f64()),
        "perf_counter" | "monotonic" => value::float_new(monotonic_seconds()),
        "time_ns" => wall_clock_nanos(),
        "perf_counter_ns" | "monotonic_ns" => monotonic_nanos(),
        "sleep" => match number(args[0]) {
            Ok(seconds) => {
                if seconds.is_nan() || seconds < 0.0 {
                    exc::raise_with_message(VALUE_ERROR, "sleep length must be non-negative")
                } else if !seconds.is_finite() || seconds >= u64::MAX as f64 {
                    exc::raise_with_message(OVERFLOW_ERROR, "sleep length is too large")
                } else {
                    // Park for the collector while sleeping; no runtime lock
                    // is held across the block.
                    heap::blocking_enter();
                    std::thread::sleep(Duration::from_secs_f64(seconds));
                    heap::blocking_leave();
                    NONE
                }
            }
            Err(err) => err,
        },
        _ => missing_attribute("time", function),
    }
}

// -------------------------------------------------------------------------
// _thread (no GIL)
// -------------------------------------------------------------------------
//
// `start_new_thread` switches the process to the shared heap (see
// `crate::heap`) before the new thread exists. Threads then run in parallel:
// there is no GIL, so a list/dict that two threads mutate must be protected by
// a user lock. Runtime internals (heap registry, thread registry, exception
// state) are either mutex-protected or thread-local.
//
// Locking rules that prevent deadlocks:
// * the heap mutex is never held while user code runs and never taken by
//   `join`, lock acquisition or `time.sleep`;
// * the thread registry lock is released before `join` blocks;
// * blocking operations park the calling thread for the stop-the-world
//   collector (`heap::blocking_enter` / `blocking_leave`), which never holds
//   a runtime lock across the block.

/// Registry of spawned threads that have not been joined yet.
///
/// The lock is held only for map operations. `join` removes its handle and
/// releases the lock before blocking. Threads that are never joined leave a
/// finished `JoinHandle` here until the process exits; that is an inert,
/// small value, and a later `join` still reports a clean result.
static THREADS: LazyLock<Mutex<HashMap<i64, JoinHandle<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Locks the thread registry, ignoring poisoning (the map stays consistent
/// because no user code runs under the lock).
fn lock_registry() -> MutexGuard<'static, HashMap<i64, JoinHandle<()>>> {
    THREADS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records that a spawned thread has left its closure.
///
/// A `Drop` guard (rather than a plain call at the end) keeps the live-thread
/// count, the Python-visible `finished` flag and the collector registration
/// correct even if the thread unwinds with a Rust panic;
/// `heap::leave_thread` marks the registration exited and wakes a waiting
/// collector.
struct ThreadExit {
    finished: Option<Arc<AtomicBool>>,
}

impl Drop for ThreadExit {
    fn drop(&mut self) {
        if let Some(finished) = &self.finished {
            finished.store(true, Ordering::Release);
        }
        heap::leave_thread();
    }
}

/// Drops the join handles inherited by a forked child. The threads they
/// referred to exist only in the parent, so the child must not join them.
pub(crate) fn after_fork_child() {
    lock_registry().clear();
}

fn thread_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "start_new_thread" => start_new_thread(args[0], args[1]),
        "join" => join_thread(args[0]),
        "get_ident" => encode_int(heap::current_thread_id()),
        "allocate_lock" => value::lock_new(),
        _ => missing_attribute("_thread", function),
    }
}

/// Validates the callable/arity pair shared by `_thread.start_new_thread` and
/// `threading.Thread`. Returns the `TypeError` message on mismatch.
fn spawn_arity_error(function: i64, argc: usize) -> Option<String> {
    let func = value::as_function(function)?;
    // `arity` is exact for generated wrappers. Native module functions store
    // their upper bound there, so consult the central spec for the lower
    // bound as well.
    match &func.body {
        value::FunctionBody::Wrapper { .. } => (func.arity != argc)
            .then(|| format!("function takes {} arguments ({argc} given)", func.arity)),
        value::FunctionBody::Native { module, name } => {
            stdlib::function(module, name).and_then(|spec| {
                if argc >= spec.min_args && argc <= spec.max_args {
                    return None;
                }
                let detail = if spec.min_args == spec.max_args {
                    format!("{} arguments", spec.max_args)
                } else {
                    format!("{} to {} arguments", spec.min_args, spec.max_args)
                };
                Some(format!("{module}.{name}() takes {detail} ({argc} given)"))
            })
        }
        // Builtins validate their own arity inside the call dispatcher.
        value::FunctionBody::Builtin(_) => None,
    }
}

/// Spawns an OS thread that calls `function` with `argv`.
///
/// Shared by `_thread.start_new_thread` and `threading.Thread.start`. The
/// heap is migrated to shared mode before the child exists, the child
/// registers itself (with the callable and arguments as explicit GC roots)
/// before running user code, and the returned id keys the join registry.
/// `finished` is set by the child just before it leaves the runtime, allowing
/// `Thread.is_alive` to observe termination.
pub(crate) fn spawn_thread(
    function: i64,
    argv: Vec<i64>,
    finished: Option<Arc<AtomicBool>>,
) -> Result<i64, String> {
    // All objects created so far (including `function` and every argument)
    // move into the shared heap before a second thread can observe them.
    heap::enable_shared();

    let id = heap::new_thread_id();
    let argc = argv.len();
    // Stable argument buffer: the boxed slice lives in the closure until the
    // thread has finished. Its tagged contents are not on either thread's
    // native stack, so they are registered as explicit roots below.
    let argv: Box<[i64]> = argv.into_boxed_slice();
    // The callable and the argument values are only reachable from this Rust
    // closure until the generated wrapper receives them; neither thread's
    // native stack necessarily holds them. Register them as explicit roots of
    // the child together with its GC registration.
    let mut roots = Vec::with_capacity(argc + 1);
    roots.push(function);
    roots.extend_from_slice(&argv);
    // Count the thread before it starts so the sweep can see the upcoming
    // thread (the collector proper waits on the GC registration table).
    heap::enter_thread();
    let (registered_tx, registered_rx) = std::sync::mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name(format!("pyx-thread-{id}"))
        .spawn(move || {
            heap::register_thread(id, roots);
            // Wake the spawner: the child is now visible to the collector
            // with stack bounds and roots before it reaches its first
            // safepoint.
            let _ = registered_tx.send(());
            heap::safepoint();
            let _exit = ThreadExit { finished };
            // SAFETY: `argv` is alive in this closure for the whole call and
            // `argc` matches its length; the wrapper contract lets the callee
            // read `argc` tagged values from the pointer (NULL is passed for
            // zero arguments by `builtins::call`).
            let _ = builtins::call(function, argc, argv.as_ptr());
            // An exception cannot cross the thread boundary: report it like
            // an uncaught exception, as CPython's `_thread` does.
            if exc::pending() {
                let _ = exc::uncaught();
            }
        });
    match spawned {
        Ok(handle) => {
            // Wait until the child registered itself (and thereby installed
            // its roots). Until then the collector cannot sweep the closure
            // arguments.
            let _ = registered_rx.recv();
            lock_registry().insert(id, handle);
            Ok(id)
        }
        Err(_) => {
            // The child never ran, so only the pre-counted live thread is
            // rolled back; the spawner's own registration stays intact.
            heap::leave_unstarted_thread();
            Err("can't start new thread".to_string())
        }
    }
}

/// `_thread.start_new_thread(fn, args)`.
///
/// Validates the callable and the argument sequence (arity is checked before
/// spawning so callers see a `TypeError` instead of a traceback on stderr),
/// then delegates to [`spawn_thread`]. Returns the thread id as a tagged
/// integer.
fn start_new_thread(function: i64, args_value: i64) -> i64 {
    if value::as_function(function).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "start_new_thread() argument 1 must be callable, not {}",
                value::type_name(function)
            ),
        );
    }
    let argv: Vec<i64> = if let Some(items) = value::as_list(args_value) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(args_value) {
        items.to_vec()
    } else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "start_new_thread() argument 2 must be a tuple, not {}",
                value::type_name(args_value)
            ),
        );
    };
    if let Some(message) = spawn_arity_error(function, argv.len()) {
        return exc::raise_with_message(TYPE_ERROR, &message);
    }
    match spawn_thread(function, argv, None) {
        Ok(id) => encode_int(id),
        Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
    }
}

/// Joins a spawned thread by id, parking for the collector while it blocks.
/// The handle is removed from the registry; a second join is an error.
pub(crate) fn join_spawned(id: i64) -> Result<(), String> {
    let handle = lock_registry().remove(&id);
    let Some(handle) = handle else {
        return Err("invalid thread id".to_string());
    };
    // The registry lock is released; park for the collector while the join
    // blocks, then wait for a running collection before returning to user
    // code.
    heap::blocking_enter();
    let result = handle.join();
    heap::blocking_leave();
    match result {
        Ok(()) => Ok(()),
        Err(_) => Err("thread terminated with an exception".to_string()),
    }
}

/// `_thread.join(id)` — PyX extension; CPython's `_thread` has no join.
///
/// Unknown and already-joined ids raise `RuntimeError`.
fn join_thread(handle_value: i64) -> i64 {
    let Some(id) = value::as_integer(handle_value) else {
        return exc::raise_with_message(RUNTIME_ERROR, "invalid thread id");
    };
    match join_spawned(id) {
        Ok(()) => NONE,
        Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
    }
}

// -------------------------------------------------------------------------
// threading / queue / multiprocessing / pickle
// -------------------------------------------------------------------------

/// Fallback for constructor slots the frontend omitted (`None` means default).
fn optional(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

/// Validates a callable constructor argument.
fn callable_argument(context: &str, value_arg: i64) -> Result<i64, i64> {
    if value::as_function(value_arg).is_some() {
        Ok(value_arg)
    } else {
        Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{context} 'target' must be callable, not {}",
                value::type_name(value_arg)
            ),
        ))
    }
}

/// `args=None` becomes an empty argument list; tuples and lists are copied.
fn argument_sequence(context: &str, value_arg: i64) -> Result<Vec<i64>, i64> {
    if value_arg == NONE {
        return Ok(Vec::new());
    }
    if let Some(items) = value::as_list(value_arg) {
        return Ok(items.to_vec());
    }
    if let Some(items) = value::as_tuple(value_arg) {
        return Ok(items.to_vec());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{context} 'args' must be a tuple or list, not {}",
            value::type_name(value_arg)
        ),
    ))
}

/// `name=None` selects the next default name (`Thread-N` / `Process-N`).
fn thread_name(
    context: &str,
    value_arg: i64,
    prefix: &str,
    next: &AtomicUsize,
) -> Result<String, i64> {
    if value_arg == NONE {
        return Ok(format!("{prefix}-{}", next.fetch_add(1, Ordering::Relaxed)));
    }
    match value::as_str(value_arg) {
        Some(name) => Ok(name.to_string()),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{context} 'name' must be a str, not {}",
                value::type_name(value_arg)
            ),
        )),
    }
}

/// `maxsize <= 0` (and `None`) mean unbounded.
fn maxsize_argument(value_arg: i64) -> Result<usize, i64> {
    if value_arg == NONE {
        return Ok(0);
    }
    let Some(maxsize) = value::as_integer(value_arg) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object cannot be interpreted as an integer",
                value::type_name(value_arg)
            ),
        ));
    };
    if maxsize <= 0 {
        return Ok(0);
    }
    Ok(usize::try_from(maxsize).unwrap_or(usize::MAX))
}

fn default_thread_counter() -> &'static AtomicUsize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    &NEXT
}

fn default_process_counter() -> &'static AtomicUsize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    &NEXT
}

fn threading_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Thread" => {
            let target = match callable_argument("Thread()", optional(args, 0)) {
                Ok(target) => target,
                Err(err) => return err,
            };
            let argv = match argument_sequence("Thread()", optional(args, 1)) {
                Ok(argv) => argv,
                Err(err) => return err,
            };
            let name = match thread_name(
                "Thread()",
                optional(args, 2),
                "Thread",
                default_thread_counter(),
            ) {
                Ok(name) => name,
                Err(err) => return err,
            };
            value::thread_new(target, argv, name)
        }
        "Lock" => value::lock_new(),
        "RLock" => value::lock_new_reentrant(),
        "Event" => value::event_new(),
        "get_ident" => encode_int(heap::current_thread_id()),
        "active_count" => encode_int(heap::active_threads() as i64),
        _ => missing_attribute("threading", function),
    }
}

fn queue_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Queue" => match maxsize_argument(optional(args, 0)) {
            Ok(maxsize) => value::queue_new(maxsize),
            Err(err) => err,
        },
        _ => missing_attribute("queue", function),
    }
}

/// Validates `Pool(processes)`: `None` selects the CPU count, any other value
/// must be a positive integer.
fn pool_processes(value_arg: i64) -> Result<usize, i64> {
    if value_arg == NONE {
        return Ok(crate::pool::default_processes());
    }
    let Some(processes) = value::as_integer(value_arg) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "Pool() 'processes' must be an integer, not {}",
                value::type_name(value_arg)
            ),
        ));
    };
    if processes <= 0 {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "Number of processes must be at least 1",
        ));
    }
    Ok(usize::try_from(processes).unwrap_or(usize::MAX))
}

fn multiprocessing_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Process" => {
            let target = match callable_argument("Process()", optional(args, 0)) {
                Ok(target) => target,
                Err(err) => return err,
            };
            let argv = match argument_sequence("Process()", optional(args, 1)) {
                Ok(argv) => argv,
                Err(err) => return err,
            };
            let name = match thread_name(
                "Process()",
                optional(args, 2),
                "Process",
                default_process_counter(),
            ) {
                Ok(name) => name,
                Err(err) => return err,
            };
            value::process_new(target, argv, name)
        }
        "Queue" => match maxsize_argument(optional(args, 0)) {
            Ok(maxsize) => match process::new_queue(maxsize) {
                Ok(queue) => queue,
                Err(message) => {
                    exc::raise_with_message(RUNTIME_ERROR, &format!("Queue(): {message}"))
                }
            },
            Err(err) => err,
        },
        "Pool" => {
            let processes = match pool_processes(optional(args, 0)) {
                Ok(processes) => processes,
                Err(err) => return err,
            };
            match crate::pool::new_pool(processes) {
                Ok(pool) => pool,
                Err(message) => {
                    exc::raise_with_message(RUNTIME_ERROR, &format!("Pool(): {message}"))
                }
            }
        }
        "cpu_count" => encode_int(cpu_count()),
        "get_start_method" => value::str_alloc(process::get_start_method()),
        "set_start_method" => {
            let Some(method) = value::as_str(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "set_start_method() argument must be str, not {}",
                        value::type_name(args[0])
                    ),
                );
            };
            match process::set_start_method(method) {
                Ok(()) => NONE,
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            }
        }
        _ => missing_attribute("multiprocessing", function),
    }
}

fn pickle_error(error: serialize::PickleError, loading: bool) -> i64 {
    let message = if loading {
        error.loads_message()
    } else {
        error.dumps_message()
    };
    let tag = match error {
        serialize::PickleError::Unsupported(_) => TYPE_ERROR,
        serialize::PickleError::TooDeep | serialize::PickleError::Corrupt => VALUE_ERROR,
    };
    exc::raise_with_message(tag, &message)
}

fn pickle_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "dumps" => match serialize::encode(args[0], false) {
            Ok(bytes) => value::str_alloc(serialize::to_hex(&bytes)),
            Err(error) => pickle_error(error, false),
        },
        "loads" => {
            let Some(text) = value::as_str(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "loads() argument must be str, not {}",
                        value::type_name(args[0])
                    ),
                );
            };
            let Some(bytes) = serialize::from_hex(text) else {
                return exc::raise_with_message(VALUE_ERROR, "invalid pickle data");
            };
            match serialize::decode(&bytes, false) {
                Ok(decoded) => decoded,
                Err(error) => pickle_error(error, true),
            }
        }
        _ => missing_attribute("pickle", function),
    }
}

// -------------------------------------------------------------------------
// csv
// -------------------------------------------------------------------------

fn csv_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "reader" => crate::csv::reader_new(args[0]),
        "writer" => crate::csv::writer_new(args[0]),
        "DictReader" => crate::csv::dict_reader_new(args[0]),
        "DictWriter" => crate::csv::dict_writer_new(args[0], args[1]),
        _ => missing_attribute("csv", function),
    }
}

// -------------------------------------------------------------------------
// json
// -------------------------------------------------------------------------

/// Maximum container nesting accepted by both the serializer and the parser.
/// Bounds recursion on the native stack; cycles and over-deep documents are
/// reported instead of overflowing.
const MAX_JSON_DEPTH: usize = 128;

fn serialize_error(v: i64) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "Object of type {} is not JSON serializable",
            value::type_name(v)
        ),
    )
}

fn circular_error() -> i64 {
    exc::raise_with_message(VALUE_ERROR, "Circular reference detected")
}

fn json_parse_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn json_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "dumps" => json_dumps(args[0]),
        "loads" => json_loads(args[0]),
        "dump" => json_dump(args[0], args[1]),
        "load" => json_load(args[0]),
        _ => missing_attribute("json", function),
    }
}

/// Validates the `file` argument of `json.dump`/`json.load`.
fn json_file(file: i64, function: &str) -> Result<&'static value::FileState, i64> {
    match value::as_file(file) {
        Some(state) => Ok(state),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{function}() argument 'file' must be a file object, not {}",
                value::type_name(file)
            ),
        )),
    }
}

/// `json.dump(obj, file)`: serialises like [`json_dumps`] and writes the
/// result with a single `FileState::write` call.
fn json_dump(v: i64, file: i64) -> i64 {
    let state = match json_file(file, "dump") {
        Ok(state) => state,
        Err(err) => return err,
    };
    let mut out = String::new();
    let mut stack: Vec<i64> = Vec::new();
    if let Err(err) = dumps_value(v, 0, &mut stack, &mut out) {
        return err;
    }
    match state.write(&out) {
        Ok(_) => NONE,
        Err(error) => value::raise_file_error(error),
    }
}

/// `json.load(file)`: reads the remaining file text and parses it with the
/// same parser as [`json_loads`].
fn json_load(file: i64) -> i64 {
    let state = match json_file(file, "load") {
        Ok(state) => state,
        Err(err) => return err,
    };
    match state.read(None) {
        Ok(source) => parse_json(source),
        Err(error) => value::raise_file_error(error),
    }
}

/// `json.dumps(value)`: CPython defaults (`", "`/`": "` separators, insertion
/// order, `ensure_ascii=True`).
fn json_dumps(v: i64) -> i64 {
    let mut out = String::new();
    let mut stack: Vec<i64> = Vec::new();
    match dumps_value(v, 0, &mut stack, &mut out) {
        Ok(()) => value::str_alloc(out),
        Err(err) => err,
    }
}

/// Appends the JSON form of `v`. Only heap objects created through
/// [`dumps_value`]'s errors can trigger a collection here, so the borrows into
/// containers taken below are never read again after an allocation.
fn dumps_value(v: i64, depth: usize, stack: &mut Vec<i64>, out: &mut String) -> Result<(), i64> {
    if v == NONE {
        out.push_str("null");
        return Ok(());
    }
    if v == TRUE {
        out.push_str("true");
        return Ok(());
    }
    if v == FALSE {
        out.push_str("false");
        return Ok(());
    }
    if let Some(n) = value::as_integer(v) {
        out.push_str(&n.to_string());
        return Ok(());
    }
    if let Some(s) = value::as_str(v) {
        write_json_string(s, out);
        return Ok(());
    }
    if let Some(f) = value::as_float(v) {
        out.push_str(&json_float(f));
        return Ok(());
    }
    if let Some(items) = value::as_list(v).or_else(|| value::as_tuple(v)) {
        if depth >= MAX_JSON_DEPTH || stack.contains(&v) {
            return Err(circular_error());
        }
        stack.push(v);
        out.push('[');
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            dumps_value(*item, depth + 1, stack, out)?;
        }
        out.push(']');
        stack.pop();
        return Ok(());
    }
    if let Some(pairs) = value::as_dict(v) {
        if depth >= MAX_JSON_DEPTH || stack.contains(&v) {
            return Err(circular_error());
        }
        stack.push(v);
        out.push('{');
        for (index, (key, item)) in pairs.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&json_key(*key)?);
            out.push_str(": ");
            dumps_value(*item, depth + 1, stack, out)?;
        }
        out.push('}');
        stack.pop();
        return Ok(());
    }
    Err(serialize_error(v))
}

/// Renders a dict key. Scalars are accepted and always emitted as a quoted
/// JSON string; other types raise CPython's key `TypeError`.
fn json_key(key: i64) -> Result<String, i64> {
    if let Some(s) = value::as_str(key) {
        let mut out = String::new();
        write_json_string(s, &mut out);
        return Ok(out);
    }
    let scalar = if key == NONE {
        Some("null".to_string())
    } else if key == TRUE {
        Some("true".to_string())
    } else if key == FALSE {
        Some("false".to_string())
    } else if let Some(n) = value::as_integer(key) {
        Some(n.to_string())
    } else {
        value::as_float(key).map(json_float)
    };
    match scalar {
        Some(text) => {
            let mut out = String::with_capacity(text.len() + 2);
            out.push('"');
            out.push_str(&text);
            out.push('"');
            Ok(out)
        }
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "keys must be str, int, float, bool or None, not {}",
                value::type_name(key)
            ),
        )),
    }
}

/// CPython's json float rendering: shortest round-tripping representation
/// with `NaN`/`Infinity` spellings.
fn json_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    value::format_float(f)
}

/// Appends `s` as a JSON string with `ensure_ascii=True`: ASCII control
/// characters, `"` and `\\` use escapes, everything outside printable ASCII is
/// written as `\uXXXX` (surrogate pairs above the BMP).
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 || (ch as u32) >= 0x7f => {
                write_unicode_escape(ch as u32, out);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn write_unicode_escape(code: u32, out: &mut String) {
    use std::fmt::Write as _;
    if code <= 0xffff {
        // Writing into a String cannot fail.
        let _ = write!(out, "\\u{code:04x}");
        return;
    }
    let rest = code - 0x10000;
    let high = 0xd800 + (rest >> 10);
    let low = 0xdc00 + (rest & 0x3ff);
    let _ = write!(out, "\\u{high:04x}\\u{low:04x}");
}

/// `json.loads(text)`: parses a `str` into runtime values. Containers are
/// allocated empty first and held in tagged locals, then filled incrementally,
/// so a collection between two elements still traces the partial container.
fn json_loads(v: i64) -> i64 {
    let Some(text) = value::as_str(v) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "the JSON object must be str, bytes or bytearray, not {}",
                value::type_name(v)
            ),
        );
    };
    // Copy the input before the first allocation: the parser must not depend
    // on a reference into a heap object while new objects are being created.
    parse_json(text.to_string())
}

/// Parses a complete JSON document (shared by `loads` and file `load`).
fn parse_json(source: String) -> i64 {
    let mut parser = JsonParser { source, pos: 0 };
    match parser.parse_document() {
        Ok(result) => result,
        Err(err) => err,
    }
}

struct JsonParser {
    source: String,
    pos: usize,
}

impl JsonParser {
    fn bytes(&self) -> &[u8] {
        self.source.as_bytes()
    }

    /// Current byte, or `None` at end of input.
    fn byte(&self) -> Option<u8> {
        self.bytes().get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.byte(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn consume(&mut self, word: &str) -> bool {
        // `pos` is always on a char boundary and `word` is ASCII.
        if self.source[self.pos..].starts_with(word) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), i64> {
        if self.consume(word) {
            Ok(())
        } else {
            Err(json_parse_error("Expecting value"))
        }
    }

    fn parse_document(&mut self) -> Result<i64, i64> {
        self.skip_whitespace();
        if self.byte().is_none() {
            return Err(json_parse_error("Expecting value"));
        }
        let result = self.parse_value(0)?;
        self.skip_whitespace();
        if self.pos < self.bytes().len() {
            return Err(json_parse_error("Extra data"));
        }
        Ok(result)
    }

    fn parse_value(&mut self, depth: usize) -> Result<i64, i64> {
        self.skip_whitespace();
        match self.byte() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => {
                let text = self.parse_string()?;
                Ok(value::str_alloc(text))
            }
            Some(b't') => {
                self.literal("true")?;
                Ok(TRUE)
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(FALSE)
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(NONE)
            }
            Some(b'N') => {
                self.literal("NaN")?;
                Ok(value::float_new(f64::NAN))
            }
            Some(b'I') => {
                self.literal("Infinity")?;
                Ok(value::float_new(f64::INFINITY))
            }
            Some(b'-') if self.source[self.pos..].starts_with("-Infinity") => {
                self.pos += "-Infinity".len();
                Ok(value::float_new(f64::NEG_INFINITY))
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(json_parse_error("Expecting value")),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<i64, i64> {
        if depth >= MAX_JSON_DEPTH {
            return Err(json_parse_error("Maximum nesting depth exceeded"));
        }
        self.pos += 1; // '['
        let array = value::list_new(Vec::new());
        self.skip_whitespace();
        if self.byte() == Some(b']') {
            self.pos += 1;
            return Ok(array);
        }
        loop {
            let item = self.parse_value(depth + 1)?;
            if let Some(mut items) = value::list_items_mut(array) {
                items.push(item);
                crate::heap::write_barrier_edge(array, item);
            }
            self.skip_whitespace();
            match self.byte() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(array);
                }
                _ => return Err(json_parse_error("Expecting ',' delimiter")),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<i64, i64> {
        if depth >= MAX_JSON_DEPTH {
            return Err(json_parse_error("Maximum nesting depth exceeded"));
        }
        self.pos += 1; // '{'
        let object = value::dict_new(Vec::new());
        self.skip_whitespace();
        if self.byte() == Some(b'}') {
            self.pos += 1;
            return Ok(object);
        }
        loop {
            self.skip_whitespace();
            if self.byte() != Some(b'"') {
                return Err(json_parse_error(
                    "Expecting property name enclosed in double quotes",
                ));
            }
            let raw_key = self.parse_string()?;
            // Root the key in a tagged local while the value is parsed.
            let key = value::str_alloc(raw_key);
            self.skip_whitespace();
            if self.byte() != Some(b':') {
                return Err(json_parse_error("Expecting ':' delimiter"));
            }
            self.pos += 1;
            let item = self.parse_value(depth + 1)?;
            if let Some(mut pairs) = value::dict_pairs_mut(object) {
                crate::container::dict_insert_barrier(object, &mut pairs, key, item);
            }
            self.skip_whitespace();
            match self.byte() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(object);
                }
                _ => return Err(json_parse_error("Expecting ',' delimiter")),
            }
        }
    }

    fn parse_number(&mut self) -> Result<i64, i64> {
        let start = self.pos;
        if self.byte() == Some(b'-') {
            self.pos += 1;
        }
        match self.byte() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.byte(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(json_parse_error("Expecting value")),
        }
        let mut is_float = false;
        if self.byte() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            let digits = self.pos;
            while matches!(self.byte(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == digits {
                return Err(json_parse_error("Expecting value"));
            }
        }
        if matches!(self.byte(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.byte(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let digits = self.pos;
            while matches!(self.byte(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == digits {
                return Err(json_parse_error("Expecting value"));
            }
        }
        let text = &self.source[start..self.pos];
        if is_float {
            match text.parse::<f64>() {
                Ok(f) => Ok(value::float_new(f)),
                Err(_) => Err(json_parse_error("Expecting value")),
            }
        } else {
            match text.parse::<i64>() {
                Ok(n) => Ok(encode_int(n)),
                // The runtime has no arbitrary-precision integers; values
                // outside `i64` cannot be represented.
                Err(_) => Err(exc::raise_with_message(OVERFLOW_ERROR, "integer overflow")),
            }
        }
    }

    /// Parses a quoted string. `self.pos` must point at the opening quote.
    fn parse_string(&mut self) -> Result<String, i64> {
        self.pos += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.byte() else {
                return Err(json_parse_error("Unterminated string starting at"));
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return match String::from_utf8(out) {
                        Ok(text) => Ok(text),
                        Err(_) => Err(json_parse_error("Invalid UTF-8 sequence")),
                    };
                }
                b'\\' => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                0x00..=0x1f => {
                    return Err(json_parse_error("Invalid control character at"));
                }
                _ => {
                    // Copy one complete UTF-8 scalar verbatim. The input is a
                    // Rust `String`, so `pos` is always on a char boundary.
                    let ch = self.source[self.pos..].chars().next().unwrap_or('\u{fffd}');
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// Handles the escape sequence after a backslash (`self.pos` is past it).
    fn parse_escape(&mut self, out: &mut Vec<u8>) -> Result<(), i64> {
        let Some(byte) = self.byte() else {
            return Err(json_parse_error("Unterminated string starting at"));
        };
        self.pos += 1;
        match byte {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let Some(code) = self.parse_hex4() else {
                    return Err(json_parse_error("Invalid \\uXXXX escape"));
                };
                let scalar = if (0xd800..=0xdbff).contains(&code) {
                    // A high surrogate must be followed by a low surrogate;
                    // lone and invalid pairs become U+FFFD because Rust strings
                    // cannot store surrogate code points.
                    let saved = self.pos;
                    if self.consume("\\u") {
                        match self.parse_hex4() {
                            Some(low) if (0xdc00..=0xdfff).contains(&low) => {
                                0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00)
                            }
                            _ => {
                                self.pos = saved;
                                0xfffd
                            }
                        }
                    } else {
                        0xfffd
                    }
                } else if (0xdc00..=0xdfff).contains(&code) {
                    0xfffd
                } else {
                    code
                };
                push_scalar(out, scalar);
            }
            _ => return Err(json_parse_error("Invalid \\escape")),
        }
        Ok(())
    }

    /// Reads exactly four hex digits, leaving `pos` untouched on failure.
    fn parse_hex4(&mut self) -> Option<u32> {
        let bytes = self.bytes();
        if self.pos + 4 > bytes.len() {
            return None;
        }
        let mut code = 0u32;
        for index in 0..4 {
            let digit = (bytes[self.pos + index] as char).to_digit(16)?;
            code = code * 16 + digit;
        }
        self.pos += 4;
        Some(code)
    }
}

fn push_scalar(out: &mut Vec<u8>, scalar: u32) {
    let ch = char::from_u32(scalar).unwrap_or('\u{fffd}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

// -------------------------------------------------------------------------
// os.path
// -------------------------------------------------------------------------

/// Borrows a `str` or `Path` argument for an `os.path` function.
///
/// On failure a `TypeError` is pending and `None` is returned, so callers
/// only have to return the `None` dummy value.
fn os_path_arg(function: &str, arg: i64) -> Option<&'static str> {
    if let Some(path) = value::as_path_like(arg) {
        return Some(path);
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{}() argument must be a path string, not {}",
            function,
            value::type_name(arg)
        ),
    );
    None
}

/// Lexically normalises an absolute path: drops `.` components and resolves
/// `..` without touching the filesystem (like `os.path.normpath`).
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            std::path::Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            std::path::Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// `posixpath.basename` semantics: the text after the last separator,
/// including the empty result for a trailing separator.
fn path_basename(path: &str) -> String {
    match path.rfind(std::path::MAIN_SEPARATOR) {
        Some(index) => path[index + 1..].to_string(),
        None => path.to_string(),
    }
}

/// `posixpath.dirname` semantics: the text up to the last separator with
/// trailing separators removed, except for a pure root.
fn path_dirname(path: &str) -> String {
    let separator = std::path::MAIN_SEPARATOR;
    let Some(index) = path.rfind(separator) else {
        return String::new();
    };
    let head = &path[..=index];
    if head.chars().all(|ch| ch == separator) {
        return head.to_string();
    }
    head.trim_end_matches(separator).to_string()
}

/// Splits a trailing extension. A leading dot in the file name is part of the
/// name, and a trailing dot is an (empty) extension, like `os.path.splitext`.
fn path_splitext(path: &str) -> (String, String) {
    let separator = std::path::MAIN_SEPARATOR;
    let start = path.rfind(separator).map_or(0, |index| index + 1);
    let Some(dot) = path[start..].rfind('.') else {
        return (path.to_string(), String::new());
    };
    if dot == 0 {
        return (path.to_string(), String::new());
    }
    let cut = start + dot;
    (path[..cut].to_string(), path[cut..].to_string())
}

fn os_path_join(args: &[i64]) -> i64 {
    let mut joined = String::new();
    for arg in args {
        let Some(part) = os_path_arg("join", *arg) else {
            return NONE;
        };
        if std::path::Path::new(part).is_absolute() {
            joined.clear();
            joined.push_str(part);
        } else if joined.is_empty() {
            joined.push_str(part);
        } else {
            if !joined.ends_with(std::path::MAIN_SEPARATOR) {
                joined.push(std::path::MAIN_SEPARATOR);
            }
            joined.push_str(part);
        }
    }
    value::str_alloc(joined)
}

fn os_path_predicate(function: &str, value: i64, accept: fn(&std::fs::Metadata) -> bool) -> i64 {
    let Some(path) = os_path_arg(function, value) else {
        return NONE;
    };
    match std::fs::metadata(path) {
        Ok(metadata) if accept(&metadata) => TRUE,
        Ok(_) => FALSE,
        Err(_) => FALSE,
    }
}

fn os_path_abspath(value: i64) -> i64 {
    let Some(path) = os_path_arg("abspath", value) else {
        return NONE;
    };
    let path = std::path::Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) if path.as_os_str().is_empty() => cwd,
            Ok(cwd) => cwd.join(path),
            Err(error) => {
                return exc::raise_with_message(OS_ERROR, &format!("abspath: {error}"));
            }
        }
    };
    value::str_alloc(normalize_path(&absolute).to_string_lossy().into_owned())
}

fn os_path_split_pair(value: i64) -> i64 {
    let Some(path) = os_path_arg("split", value) else {
        return NONE;
    };
    let head = value::str_alloc(path_dirname(path));
    let tail = value::str_alloc(path_basename(path));
    value::tuple_new(vec![head, tail])
}

fn os_path_splitext_pair(value: i64) -> i64 {
    let Some(path) = os_path_arg("splitext", value) else {
        return NONE;
    };
    let (root, extension) = path_splitext(path);
    let root = value::str_alloc(root);
    let extension = value::str_alloc(extension);
    value::tuple_new(vec![root, extension])
}

/// `os.path`: pure string manipulation plus filesystem predicates.
fn os_path_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "join" => os_path_join(args),
        "basename" => match os_path_arg("basename", args[0]) {
            Some(path) => value::str_alloc(path_basename(path)),
            None => NONE,
        },
        "dirname" => match os_path_arg("dirname", args[0]) {
            Some(path) => value::str_alloc(path_dirname(path)),
            None => NONE,
        },
        "exists" => os_path_predicate("exists", args[0], |_| true),
        "isfile" => os_path_predicate("isfile", args[0], |metadata| metadata.is_file()),
        "isdir" => os_path_predicate("isdir", args[0], |metadata| metadata.is_dir()),
        "abspath" => os_path_abspath(args[0]),
        "splitext" => os_path_splitext_pair(args[0]),
        "split" => os_path_split_pair(args[0]),
        _ => missing_attribute("os.path", function),
    }
}

// -------------------------------------------------------------------------
// os
// -------------------------------------------------------------------------

fn os_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "getpid" => encode_int(i64::from(std::process::id())),
        "getppid" => encode_int(parent_pid()),
        "cpu_count" => encode_int(cpu_count()),
        "getcwd" => match std::env::current_dir() {
            Ok(path) => value::str_alloc(path.to_string_lossy().into_owned()),
            Err(err) => {
                exc::raise_with_message(crate::exc::RUNTIME_ERROR, &format!("getcwd: {err}"))
            }
        },
        "getenv" => getenv(args),
        "fork" => match process::fork() {
            Ok(0) => {
                // The child only has the calling thread; rebuild the runtime
                // bookkeeping before returning 0 to user code.
                process::child_after_fork();
                encode_int(0)
            }
            Ok(pid) => encode_int(pid),
            Err(message) => exc::raise_with_message(RUNTIME_ERROR, &format!("fork: {message}")),
        },
        "waitpid" => {
            let Some(pid) = value::as_integer(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("pid must be an integer, not {}", value::type_name(args[0])),
                );
            };
            let Some(options) = value::as_integer(args[1]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "options must be an integer, not {}",
                        value::type_name(args[1])
                    ),
                );
            };
            match process::os_waitpid(pid, options) {
                Ok((reaped, status)) => {
                    let reaped = encode_int(reaped);
                    let status = encode_int(status);
                    if failed() {
                        return NONE;
                    }
                    value::tuple_new(vec![reaped, status])
                }
                Err(message) => {
                    exc::raise_with_message(RUNTIME_ERROR, &format!("waitpid: {message}"))
                }
            }
        }
        "_exit" => {
            let Some(code) = value::as_integer(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "_exit() argument must be an integer, not {}",
                        value::type_name(args[0])
                    ),
                );
            };
            process::exit_now(code as i32)
        }
        "remove" => {
            let Some(path) = value::as_path_like(args[0]) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "remove() argument must be a path string, not {}",
                        value::type_name(args[0])
                    ),
                );
            };
            match std::fs::remove_file(path) {
                Ok(()) => NONE,
                Err(error) => exc::raise_with_message(
                    crate::exc::OS_ERROR,
                    &format!("remove: {error}: '{}'", path),
                ),
            }
        }
        "rename" => {
            let (Some(from), Some(to)) =
                (value::as_path_like(args[0]), value::as_path_like(args[1]))
            else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "rename() arguments must be path strings",
                );
            };
            match std::fs::rename(from, to) {
                Ok(()) => NONE,
                Err(error) => {
                    exc::raise_with_message(crate::exc::OS_ERROR, &format!("rename: {error}"))
                }
            }
        }
        _ => missing_attribute("os", function),
    }
}

// -------------------------------------------------------------------------
// io
// -------------------------------------------------------------------------

/// `io.open(file, mode="r", encoding=None, newline=None, errors=None)`.
///
/// UTF-8 text mode (`r`, `w`, `a`, `x`, optionally with `t`) and binary mode
/// (the same letters with `b`) are implemented. A keyword call arrives with
/// `None` in every omitted slot (the frontend fills the gaps), so `None` means
/// "use the default" for every optional argument. Universal-newline
/// translation is not implemented: `newline` is accepted and ignored for text
/// mode, as are `errors`. Binary mode rejects `encoding`, `newline` and
/// `errors` with `ValueError`, like CPython.
fn io_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "open" => io_open(args),
        _ => missing_attribute("io", function),
    }
}

pub(crate) fn io_open(args: &[i64]) -> i64 {
    let Some(path) = value::as_str(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "open() argument 'file' must be str, not {}",
                value::type_name(args[0])
            ),
        );
    };
    let path = path.to_string();

    let mode_text = match args.get(1).copied() {
        None | Some(NONE) => "r".to_string(),
        Some(mode) => match value::as_str(mode) {
            Some(text) => text.to_string(),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "open() argument 'mode' must be str, not {}",
                        value::type_name(mode)
                    ),
                )
            }
        },
    };
    let mode = match parse_mode(&mode_text) {
        Ok(mode) => mode,
        Err(message) => return exc::raise_with_message(VALUE_ERROR, &message),
    };

    if mode.binary {
        // CPython rejects every text-only argument in binary mode instead of
        // ignoring it, so a caller cannot assume an encoding was applied.
        for (slot, name, article) in [
            (2usize, "encoding", "an"),
            (3usize, "newline", "a"),
            (4usize, "errors", "an"),
        ] {
            if let Some(&arg) = args.get(slot) {
                if arg != NONE {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("binary mode doesn't take {article} {name} argument"),
                    );
                }
            }
        }
    } else {
        if let Some(arg) = args.get(2).copied() {
            if arg != NONE {
                match value::as_str(arg) {
                    Some(encoding) => {
                        let normalized = encoding.to_ascii_lowercase();
                        if normalized != "utf-8" && normalized != "utf8" {
                            return exc::raise_with_message(
                                VALUE_ERROR,
                                &format!("unsupported encoding: '{encoding}'"),
                            );
                        }
                    }
                    None => {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!(
                                "open() argument 'encoding' must be str or None, not {}",
                                value::type_name(arg)
                            ),
                        )
                    }
                }
            }
        }
        // `newline` and `errors` are accepted for API compatibility; text mode
        // reads and writes the file unchanged, so they only need type checking.
        for (slot, name) in [(3usize, "newline"), (4usize, "errors")] {
            if let Some(&arg) = args.get(slot) {
                if arg != NONE && value::as_str(arg).is_none() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "open() argument '{name}' must be str or None, not {}",
                            value::type_name(arg)
                        ),
                    );
                }
            }
        }
    }

    match (mode.base, mode.binary) {
        ('r', binary) => match std::fs::read(&path) {
            Ok(content) if binary => value::file_new_read_bytes(path, content),
            Ok(content) => match String::from_utf8(content) {
                Ok(text) => value::file_new_read(path, text),
                Err(error) => exc::raise_with_message(
                    VALUE_ERROR,
                    &format!("invalid UTF-8 in input: '{path}': {error}"),
                ),
            },
            Err(error) => os_failure(&path, &error),
        },
        (base, binary) => {
            let opened = match base {
                'w' => std::fs::File::create(&path),
                'a' => std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path),
                'x' => std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path),
                // `parse_mode` only returns one of the four letters above.
                _ => {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("invalid mode: '{mode_text}'"),
                    )
                }
            };
            match opened {
                Ok(file) => value::file_new_write(path, base, file, binary),
                Err(error) => os_failure(&path, &error),
            }
        }
    }
}

/// Raises `OSError` for a failed `open`, naming the path.
fn os_failure(path: &str, error: &std::io::Error) -> i64 {
    exc::raise_with_message(OS_ERROR, &format!("{error}: '{path}'"))
}

/// Parsed `io.open` mode string.
struct OpenMode {
    /// One of `r`, `w`, `a`, `x`.
    base: char,
    /// `true` for `b`, `false` for text (the default and explicit `t`).
    binary: bool,
}

/// Validates a mode string and returns its base letter plus binary flag.
///
/// Exactly one base letter and at most one of `b`/`t` are allowed (so `bt` is
/// invalid); `b` and `t` may appear in any position relative to the base.
/// Read/write (`+`) mode is still unsupported and rejected.
fn parse_mode(mode: &str) -> Result<OpenMode, String> {
    let mut base = None;
    let mut binary = None;
    for ch in mode.chars() {
        match ch {
            'r' | 'w' | 'a' | 'x' => {
                if base.is_some() {
                    return Err(format!("invalid mode: '{mode}'"));
                }
                base = Some(ch);
            }
            'b' | 't' => {
                if binary.is_some() {
                    return Err(format!("invalid mode: '{mode}'"));
                }
                binary = Some(ch == 'b');
            }
            '+' => return Err("read/write mode is not supported".to_string()),
            _ => return Err(format!("invalid mode: '{mode}'")),
        }
    }
    let base = base.ok_or_else(|| format!("invalid mode: '{mode}'"))?;
    Ok(OpenMode {
        base,
        binary: binary.unwrap_or(false),
    })
}

fn cpu_count() -> i64 {
    std::thread::available_parallelism()
        .map(|count| count.get() as i64)
        .unwrap_or(1)
}

#[cfg(unix)]
fn parent_pid() -> i64 {
    i64::from(std::os::unix::process::parent_id())
}

#[cfg(not(unix))]
fn parent_pid() -> i64 {
    // No portable parent-process query; report 0 rather than guessing.
    0
}

/// `os.getenv(name[, default])`: the environment value, else the default,
/// else `None`.
fn getenv(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("str expected, not {}", value::type_name(args[0])),
        );
    };
    // Copy before querying: the lookup cannot allocate PyX objects, but the
    // owned name keeps the intent obvious and avoids borrowing across calls.
    let name = name.to_string();
    match std::env::var(&name) {
        Ok(result) => value::str_alloc(result),
        Err(_) => {
            if args.len() == 2 {
                args[1]
            } else {
                NONE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exc::{ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
    use crate::value::{encode_int, FALSE, TRUE};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn string(s: &str) -> i64 {
        value::str_from_bytes(s.as_bytes())
    }

    fn as_float(v: i64) -> f64 {
        value::as_float(v).expect("float result")
    }

    fn as_int(v: i64) -> i64 {
        value::as_integer(v).expect("int result")
    }

    fn message() -> String {
        value::to_string(exc::pending_value())
    }

    fn get(module: &str, name: &str) -> i64 {
        module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
    }

    fn call(module: &str, function: &str, args: &[i64]) -> i64 {
        module_call(
            module.as_ptr(),
            module.len(),
            function.as_ptr(),
            function.len(),
            args.len(),
            args.as_ptr(),
        )
    }

    fn call_value(function: i64, args: &[i64]) -> i64 {
        crate::builtins::call(function, args.len(), args.as_ptr())
    }

    /// Number of native-call cache hits observed on this thread.
    fn cache_hits() -> usize {
        NATIVE_CACHE.with(|cache| cache.borrow().hits)
    }

    #[test]
    fn math_constants_and_functions() {
        exc::init();
        assert!((as_float(get("math", "pi")) - std::f64::consts::PI).abs() < 1e-12);
        assert_eq!(as_float(get("math", "e")), std::f64::consts::E);
        assert!(as_float(get("math", "tau")) > 6.0);
        assert!(as_float(get("math", "inf")).is_infinite());
        assert!(as_float(get("math", "nan")).is_nan());

        assert_eq!(as_float(call("math", "sqrt", &[int(9)])), 3.0);
        assert_eq!(as_float(call("math", "sqrt", &[int(16)])), 4.0);
        assert_eq!(as_float(call("math", "pow", &[int(2), int(10)])), 1024.0);
        assert!((as_float(call("math", "log", &[get("math", "e")])) - 1.0).abs() < 1e-12);
        assert_eq!(as_int(call("math", "floor", &[value::float_new(2.7)])), 2);
        assert_eq!(as_int(call("math", "ceil", &[value::float_new(2.1)])), 3);
        assert_eq!(as_int(call("math", "trunc", &[value::float_new(-2.7)])), -2);
        assert_eq!(
            as_float(call("math", "fabs", &[value::float_new(-3.5)])),
            3.5
        );
        assert_eq!(as_float(call("math", "sin", &[int(0)])), 0.0);
        assert_eq!(as_float(call("math", "cos", &[int(0)])), 1.0);
        assert_eq!(as_float(call("math", "exp", &[int(0)])), 1.0);
    }

    #[test]
    fn math_domain_and_type_errors() {
        exc::init();
        assert_eq!(call("math", "sqrt", &[int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "log", &[int(0)]), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "asin", &[value::float_new(2.0)]), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(
            call("math", "sin", &[value::float_new(f64::INFINITY)]),
            NONE
        );
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "sqrt", &[string("x")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "must be real number, not str");
        exc::init();
        assert_eq!(call("math", "pow", &[int(-1), value::float_new(0.5)]), NONE);
        assert_eq!(message(), "math domain error");
        exc::init();
        assert_eq!(call("math", "exp", &[value::float_new(1000.0)]), NONE);
        assert_eq!(exc::pending_tag(), crate::exc::OVERFLOW_ERROR as i64);
        assert_eq!(message(), "math range error");
    }

    #[test]
    fn math_arity_errors() {
        exc::init();
        assert_eq!(call("math", "sqrt", &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "math.sqrt() takes 1 arguments (0 given)");
        exc::init();
        assert_eq!(call("math", "pow", &[int(1)]), NONE);
        assert_eq!(message(), "math.pow() takes 2 arguments (1 given)");
    }

    #[test]
    fn native_call_cache_hits_for_alternating_names() {
        exc::init();
        let module = "math";
        let sqrt = "sqrt";
        let floor = "floor";
        let sin = "sin";
        let sqrt_arg = int(9);
        let floor_arg = value::float_new(2.7);
        let sin_arg = int(0);

        // Warm the cache (one miss per name) and confirm the results.
        assert_eq!(as_float(call(module, sqrt, &[sqrt_arg])), 3.0);
        assert_eq!(as_int(call(module, floor, &[floor_arg])), 2);
        assert_eq!(as_float(call(module, sin, &[sin_arg])), 0.0);
        let warmed = cache_hits();

        // Alternating call sites with the same names must all hit the cache
        // and keep returning identical results.
        for _ in 0..32 {
            assert_eq!(as_float(call(module, sqrt, &[sqrt_arg])), 3.0);
            assert_eq!(as_int(call(module, floor, &[floor_arg])), 2);
            assert_eq!(as_float(call(module, sin, &[sin_arg])), 0.0);
        }
        assert_eq!(cache_hits() - warmed, 32 * 3);
        assert!(!exc::pending());
    }

    #[test]
    fn native_call_cache_handles_fresh_name_buffers() {
        exc::init();

        // Fresh heap buffers per call: the cache must not assume that equal
        // names imply equal pointers.
        for _ in 0..8 {
            let module = String::from("math");
            let function = String::from("sqrt");
            let argument = int(16);
            let result = module_call(
                module.as_bytes().as_ptr(),
                module.len(),
                function.as_bytes().as_ptr(),
                function.len(),
                1,
                &argument,
            );
            assert_eq!(as_float(result), 4.0);
        }

        // One stack buffer whose contents change between calls: the
        // pointer/length pair repeats with different bytes, so the cache must
        // fall through to a fresh resolution instead of trusting the pointer.
        let module = *b"math";
        let argument = value::float_new(-2.7);
        let mut function = [0u8; 5];
        function.copy_from_slice(b"floor");
        assert_eq!(
            as_int(module_call(
                module.as_ptr(),
                module.len(),
                function.as_ptr(),
                function.len(),
                1,
                &argument
            )),
            -3
        );
        function.copy_from_slice(b"trunc");
        assert_eq!(
            as_int(module_call(
                module.as_ptr(),
                module.len(),
                function.as_ptr(),
                function.len(),
                1,
                &argument
            )),
            -2
        );
        function.copy_from_slice(b"fabs!");
        assert_eq!(
            module_call(
                module.as_ptr(),
                module.len(),
                function.as_ptr(),
                function.len(),
                1,
                &argument
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'math' has no attribute 'fabs!'");
    }

    #[test]
    fn native_call_cache_preserves_unknown_and_arity_errors() {
        exc::init();
        let module = "math";
        // An unknown member is never cached but still raises AttributeError.
        assert_eq!(call(module, "nope", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'math' has no attribute 'nope'");

        // Arity validation runs on the cached spec on hit paths only.
        exc::init();
        let sqrt = "sqrt";
        let argument = int(4);
        assert_eq!(as_float(call(module, sqrt, &[argument])), 2.0);
        assert_eq!(call(module, sqrt, &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "math.sqrt() takes 1 arguments (0 given)");
        exc::init();
        assert_eq!(call(module, sqrt, &[argument, argument]), NONE);
        assert_eq!(message(), "math.sqrt() takes 1 arguments (2 given)");
    }

    #[test]
    fn native_function_values_use_the_call_cache() {
        exc::init();
        let sqrt = get("math", "sqrt");
        let argument = int(25);
        assert_eq!(as_float(call_value(sqrt, &[argument])), 5.0);
        let warmed = cache_hits();
        for _ in 0..16 {
            assert_eq!(as_float(call_value(sqrt, &[argument])), 5.0);
        }
        // `FunctionBody::Native` names are `&'static str`, so every call
        // reaches the cache through the same pointer/length key.
        assert_eq!(cache_hits() - warmed, 16);
        assert!(!exc::pending());
    }

    #[test]
    fn producer_functions_build_function_values() {
        exc::init();
        let sqrt = get("math", "sqrt");
        assert_eq!(value::type_name(sqrt), "function");
        assert_eq!(as_float(call_value(sqrt, &[int(25)])), 5.0);

        // Native function as a map callback.
        let values = value::list_new(vec![int(1), int(4), int(9)]);
        let mapped = crate::builtins::map(sqrt, values);
        let items = value::as_list(mapped).expect("list");
        assert_eq!(items.len(), 3);
        assert_eq!(as_float(items[0]), 1.0);
        assert_eq!(as_float(items[2]), 3.0);

        // Native function as a filter predicate (`operator.truth`).
        let truth = get("operator", "truth");
        let candidates = value::list_new(vec![int(0), int(2), NONE, string("x")]);
        let filtered = crate::builtins::filter(truth, candidates);
        let items = value::as_list(filtered).expect("list");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn reduce_cases_and_errors() {
        exc::init();
        let add = get("operator", "add");
        let values = value::list_new(vec![int(1), int(2), int(3), int(4)]);
        assert_eq!(as_int(call_value(reduce_fn(), &[add, values])), 10);

        let empty = value::list_new(Vec::new());
        assert_eq!(call_value(reduce_fn(), &[add, empty]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "reduce() of empty iterable with no initial value"
        );

        exc::init();
        assert_eq!(as_int(call_value(reduce_fn(), &[add, empty, int(10)])), 10);
        exc::init();
        let values = value::list_new(vec![int(1), int(2)]);
        assert_eq!(as_int(call_value(reduce_fn(), &[add, values, int(10)])), 13);

        exc::init();
        assert_eq!(
            call_value(reduce_fn(), &[int(1), value::list_new(Vec::new())]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "reduce() arg 1 must be a function");
    }

    fn reduce_fn() -> i64 {
        get("functools", "reduce")
    }

    #[test]
    fn operator_functions_cover_all_specs() {
        exc::init();
        assert_eq!(as_int(call("operator", "add", &[int(2), int(3)])), 5);
        assert_eq!(as_int(call("operator", "sub", &[int(2), int(3)])), -1);
        assert_eq!(as_int(call("operator", "mul", &[int(4), int(5)])), 20);
        assert_eq!(
            as_float(call("operator", "truediv", &[int(1), int(2)])),
            0.5
        );
        assert_eq!(as_int(call("operator", "floordiv", &[int(7), int(2)])), 3);
        assert_eq!(as_int(call("operator", "mod", &[int(7), int(2)])), 1);
        assert_eq!(as_int(call("operator", "pow", &[int(2), int(5)])), 32);
        assert_eq!(call("operator", "eq", &[int(1), int(1)]), TRUE);
        assert_eq!(call("operator", "ne", &[int(1), int(2)]), TRUE);
        assert_eq!(call("operator", "lt", &[int(1), int(2)]), TRUE);
        assert_eq!(call("operator", "le", &[int(2), int(2)]), TRUE);
        assert_eq!(call("operator", "gt", &[int(3), int(2)]), TRUE);
        assert_eq!(call("operator", "ge", &[int(2), int(3)]), FALSE);
        assert_eq!(as_int(call("operator", "neg", &[int(5)])), -5);
        assert_eq!(call("operator", "not_", &[int(0)]), TRUE);
        assert_eq!(call("operator", "truth", &[int(0)]), FALSE);
        assert_eq!(call("operator", "truth", &[string("x")]), TRUE);
        assert!(!exc::pending());
    }

    #[test]
    fn time_functions_and_sleep() {
        exc::init();
        let start = as_float(call("time", "perf_counter", &[]));
        assert_eq!(call("time", "sleep", &[value::float_new(0.001)]), NONE);
        assert!(!exc::pending());
        let end = as_float(call("time", "perf_counter", &[]));
        assert!(end >= start);
        assert!(as_float(call("time", "monotonic", &[])) >= 0.0);
        assert!(as_float(call("time", "time", &[])) > 0.0);
        assert!(as_int(call("time", "time_ns", &[])) > 0);
        assert!(as_int(call("time", "perf_counter_ns", &[])) >= 0);
        assert!(as_int(call("time", "monotonic_ns", &[])) >= 0);
        exc::init();
        assert_eq!(call("time", "sleep", &[int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
    }

    #[test]
    fn sys_constants() {
        exc::init();
        let argv = get("sys", "argv");
        let items = value::as_list(argv).expect("list");
        assert_eq!(items.len(), 1);
        assert_eq!(value::as_str(items[0]), Some("pyx"));

        assert_eq!(value::as_str(get("sys", "platform")), Some(platform()));
        let version = value::as_str(get("sys", "version")).expect("version string");
        assert_eq!(
            version,
            format!(
                "{PYTHON_VERSION_STRING} (PyX {})",
                env!("CARGO_PKG_VERSION")
            )
        );
        let info = value::as_tuple(get("sys", "version_info")).expect("tuple");
        assert_eq!(info.len(), 5);
        assert_eq!(as_int(info[0]), i64::from(PYTHON_VERSION.0));
        assert_eq!(as_int(info[1]), i64::from(PYTHON_VERSION.1));
        assert_eq!(as_int(info[2]), i64::from(PYTHON_VERSION.2));
        assert_eq!(value::as_str(info[3]), Some("final"));
        assert_eq!(as_int(info[4]), 0);
        assert_eq!(
            value::as_str(get("sys", "pyx_version")),
            Some(env!("CARGO_PKG_VERSION"))
        );
        let pyx_info = value::as_tuple(get("sys", "pyx_version_info")).expect("tuple");
        assert_eq!(pyx_info.len(), 5);
        assert_eq!(as_int(pyx_info[0]), 0);
        assert_eq!(as_int(pyx_info[1]), 1);
        assert_eq!(as_int(pyx_info[2]), 0);
        assert_eq!(value::as_str(pyx_info[3]), Some("dev"));
        assert_eq!(as_int(pyx_info[4]), 0);
        assert_eq!(as_int(get("sys", "maxsize")), value::MAX_INT);
    }

    #[test]
    fn platform_reports_compat_target() {
        exc::init();
        assert_eq!(
            value::as_str(call("platform", "python_version", &[])),
            Some(PYTHON_VERSION_STRING)
        );
        assert_eq!(
            value::as_str(call("platform", "python_implementation", &[])),
            Some("PyX")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_modules_and_members() {
        exc::init();
        assert_eq!(get("numpy", "sqrt"), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'numpy' has no attribute 'sqrt'");

        exc::init();
        assert_eq!(get("math", "nope"), NONE);
        assert_eq!(message(), "module 'math' has no attribute 'nope'");

        exc::init();
        assert_eq!(call("math", "nope", &[]), NONE);
        assert_eq!(message(), "module 'math' has no attribute 'nope'");

        exc::init();
        assert_eq!(call("numpy", "sqrt", &[int(4)]), NONE);
        assert_eq!(message(), "module 'numpy' has no attribute 'sqrt'");

        // Constants are not callable through the module-call path.
        exc::init();
        assert_eq!(call("sys", "maxsize", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
    }

    #[test]
    fn abi_module_entries() {
        exc::init();
        let sqrt = crate::abi::pyx_module_get(b"math".as_ptr(), 4, b"sqrt".as_ptr(), 4);
        let argument = int(36);
        assert_eq!(as_float(crate::abi::pyx_call(sqrt, 1, &argument)), 6.0);
        let direct =
            crate::abi::pyx_module_call(b"math".as_ptr(), 4, b"sqrt".as_ptr(), 4, 1, &argument);
        assert_eq!(as_float(direct), 6.0);

        let values = value::list_new(vec![int(1), int(9)]);
        let mapped = crate::abi::pyx_map(sqrt, values);
        let items = value::as_list(mapped).expect("list");
        assert_eq!(as_float(items[1]), 3.0);
        exc::init();
        assert_eq!(
            crate::abi::pyx_module_call(
                b"math".as_ptr(),
                4,
                b"nope".as_ptr(),
                4,
                0,
                std::ptr::null(),
            ),
            NONE
        );
        assert_eq!(message(), "module 'math' has no attribute 'nope'");
    }

    #[test]
    fn native_functions_survive_gc_in_containers() {
        exc::init();
        let sqrt = get("math", "sqrt");
        let fabs = get("math", "fabs");
        let functions = value::list_new(vec![sqrt, fabs]);
        crate::abi::pyx_gc_collect();
        let items = value::as_list(functions).expect("list");
        assert_eq!(as_float(call_value(items[0], &[int(16)])), 4.0);
        assert_eq!(
            as_float(call_value(items[1], &[value::float_new(-2.5)])),
            2.5
        );
    }

    // ---------------------------------------------------------------------
    // json
    // ---------------------------------------------------------------------

    fn dumps(v: i64) -> String {
        let out = call("json", "dumps", &[v]);
        assert!(!exc::pending(), "dumps raised: {}", message());
        value::as_str(out).expect("str result").to_string()
    }

    fn loads(source: &str) -> i64 {
        let out = call("json", "loads", &[string(source)]);
        assert!(!exc::pending(), "loads raised: {}", message());
        out
    }

    #[test]
    fn json_dumps_scalars() {
        exc::init();
        assert_eq!(dumps(NONE), "null");
        assert_eq!(dumps(TRUE), "true");
        assert_eq!(dumps(FALSE), "false");
        assert_eq!(dumps(int(-7)), "-7");
        assert_eq!(dumps(int(0)), "0");
        assert_eq!(dumps(value::float_new(2.5)), "2.5");
        assert_eq!(dumps(value::float_new(2.0)), "2.0");
        assert_eq!(dumps(value::float_new(1e20)), "1e+20");
        assert_eq!(dumps(value::float_new(1e-5)), "1e-05");
        assert_eq!(dumps(value::float_new(1e16)), "1e+16");
        assert_eq!(dumps(value::float_new(0.0)), "0.0");
        assert_eq!(dumps(value::float_new(-0.0)), "-0.0");
        assert_eq!(dumps(value::float_new(f64::INFINITY)), "Infinity");
        assert_eq!(dumps(value::float_new(f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(dumps(value::float_new(f64::NAN)), "NaN");
        assert_eq!(dumps(string("")), "\"\"");
        assert_eq!(dumps(string("plain")), "\"plain\"");
    }

    #[test]
    fn json_dumps_string_escaping() {
        exc::init();
        assert_eq!(dumps(string("a\"b\nc")), "\"a\\\"b\\nc\"");
        assert_eq!(dumps(string("a\\b")), "\"a\\\\b\"");
        assert_eq!(dumps(string("/")), "\"/\"");
        assert_eq!(dumps(string("\u{08}\u{0c}\r\t")), "\"\\b\\f\\r\\t\"");
        // Printable ASCII stays raw; DEL and controls use \uXXXX.
        assert_eq!(dumps(string("a\u{7f}b\u{01}c")), "\"a\\u007fb\\u0001c\"");
        // Non-ASCII is escaped (ensure_ascii=True).
        assert_eq!(dumps(string("é€")), "\"\\u00e9\\u20ac\"");
        assert_eq!(dumps(string("😀")), "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn json_dumps_containers() {
        exc::init();
        let list = value::list_new(vec![int(1), value::float_new(2.5), NONE, TRUE]);
        assert_eq!(dumps(list), "[1, 2.5, null, true]");
        assert_eq!(
            dumps(value::tuple_new(vec![int(1), string("a")])),
            "[1, \"a\"]"
        );
        assert_eq!(dumps(value::list_new(Vec::new())), "[]");
        assert_eq!(dumps(value::tuple_new(Vec::new())), "[]");
        assert_eq!(dumps(value::dict_new(Vec::new())), "{}");

        // Insertion order is preserved.
        let dict = value::dict_new(vec![(string("b"), int(1)), (string("a"), int(2))]);
        assert_eq!(dumps(dict), "{\"b\": 1, \"a\": 2}");

        let nested = value::dict_new(vec![(
            string("a"),
            value::list_new(vec![
                int(1),
                value::dict_new(vec![(string("b"), string("c"))]),
            ]),
        )]);
        assert_eq!(dumps(nested), "{\"a\": [1, {\"b\": \"c\"}]}");
    }

    #[test]
    fn json_dumps_dict_keys() {
        exc::init();
        let dict = value::dict_new(vec![
            (string("k"), int(0)),
            (int(3), int(1)),
            (value::float_new(1.5), int(2)),
            (TRUE, int(3)),
            (FALSE, int(4)),
            (NONE, int(5)),
            (value::float_new(1e20), int(6)),
            (value::float_new(f64::NAN), int(7)),
        ]);
        assert_eq!(
            dumps(dict),
            "{\"k\": 0, \"3\": 1, \"1.5\": 2, \"true\": 3, \"false\": 4, \"null\": 5, \
             \"1e+20\": 6, \"NaN\": 7}"
        );

        exc::init();
        let bad_key = value::dict_new(vec![(value::tuple_new(vec![int(1)]), int(2))]);
        assert_eq!(call("json", "dumps", &[bad_key]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "keys must be str, int, float, bool or None, not tuple"
        );
    }

    #[test]
    fn json_dumps_errors_and_guards() {
        exc::init();
        assert_eq!(call("json", "dumps", &[get("math", "sqrt")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "Object of type function is not JSON serializable"
        );

        exc::init();
        let set = value::set_new(vec![int(1)]);
        assert_eq!(call("json", "dumps", &[set]), NONE);
        assert_eq!(message(), "Object of type set is not JSON serializable");

        exc::init();
        assert_eq!(call("json", "dumps", &[value::range_new(0, 3, 1)]), NONE);
        assert_eq!(message(), "Object of type range is not JSON serializable");

        exc::init();
        let list = value::list_new(vec![int(1)]);
        let iterator = value::iter_new(list);
        assert_eq!(call("json", "dumps", &[iterator]), NONE);
        assert_eq!(
            message(),
            "Object of type iterator is not JSON serializable"
        );

        // Self reference is reported, never rendered recursively.
        exc::init();
        let cycle = value::list_new(Vec::new());
        if let Some(mut items) = value::list_items_mut(cycle) {
            items.push(cycle);
        }
        assert_eq!(call("json", "dumps", &[cycle]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "Circular reference detected");

        // Depth guard.
        exc::init();
        let mut deep = value::list_new(Vec::new());
        for _ in 0..MAX_JSON_DEPTH + 2 {
            deep = value::list_new(vec![deep]);
        }
        assert_eq!(call("json", "dumps", &[deep]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "Circular reference detected");

        // A chain just below the limit still serializes.
        exc::init();
        let mut ok = value::list_new(Vec::new());
        for _ in 0..MAX_JSON_DEPTH - 1 {
            ok = value::list_new(vec![ok]);
        }
        let text = dumps(ok);
        assert_eq!(text.matches('[').count(), text.matches(']').count());
        assert!(text.starts_with("[["));
    }

    #[test]
    fn json_loads_scalars_and_containers() {
        exc::init();
        assert_eq!(loads("null"), NONE);
        assert_eq!(loads("true"), TRUE);
        assert_eq!(loads("false"), FALSE);
        assert_eq!(as_int(loads("42")), 42);
        assert_eq!(as_int(loads("-3")), -3);
        assert_eq!(as_int(loads("0")), 0);
        assert_eq!(as_float(loads("2.5")), 2.5);
        assert_eq!(as_float(loads("1e2")), 100.0);
        assert_eq!(as_float(loads("1E+2")), 100.0);
        assert_eq!(as_float(loads("-0.5")), -0.5);
        assert!(as_float(loads("NaN")).is_nan());
        assert_eq!(as_float(loads("Infinity")), f64::INFINITY);
        assert_eq!(as_float(loads("-Infinity")), f64::NEG_INFINITY);
        assert_eq!(value::as_str(loads("\"hello\"")), Some("hello"));
        assert_eq!(value::as_str(loads("  \"hi\"  ")), Some("hi"));

        assert_eq!(
            value::to_string(loads("[1, \"a\", true, null, 2.5]")),
            "[1, 'a', True, None, 2.5]"
        );
        let array = loads(" [1, 2 , 3 ] ");
        let items = value::as_list(array).expect("list");
        assert_eq!(items.len(), 3);
        assert_eq!(as_int(items[2]), 3);
        assert!(value::as_list(loads("[]")).expect("list").is_empty());
        assert!(value::as_dict(loads("{}")).expect("dict").is_empty());

        // Duplicate keys: last wins, first insertion position.
        assert_eq!(value::to_string(loads("{\"a\": 1, \"a\": 2}")), "{'a': 2}");
        assert_eq!(
            value::to_string(loads("{\"a\": [1, {\"b\": \"c\"}]}")),
            "{'a': [1, {'b': 'c'}]}"
        );
    }

    #[test]
    fn json_loads_string_escapes() {
        exc::init();
        assert_eq!(value::as_str(loads("\"a\\\\b\"")), Some("a\\b"));
        assert_eq!(value::as_str(loads("\"\\/\"")), Some("/"));
        assert_eq!(value::as_str(loads("\"\\\"\"")), Some("\""));
        assert_eq!(
            value::as_str(loads("\"\\b\\f\\n\\r\\t\"")),
            Some("\u{08}\u{0c}\n\r\t")
        );
        assert_eq!(value::as_str(loads("\"\\u00e9\\u20ac\"")), Some("é€"));
        // Raw non-ASCII passes through.
        assert_eq!(value::as_str(loads("\"é€\"")), Some("é€"));
        // Surrogate pairs combine; lone surrogates degrade to U+FFFD.
        assert_eq!(
            value::as_str(loads("\"\\ud83d\\ude00\"")),
            Some("\u{1f600}")
        );
        assert_eq!(value::as_str(loads("\"\\ud83d\"")), Some("\u{fffd}"));
        assert_eq!(
            value::as_str(loads("\"\\ud83d\\u0041\"")),
            Some("\u{fffd}A")
        );
    }

    #[test]
    fn json_loads_errors() {
        let case = |source: &str, expected: &str| {
            exc::init();
            assert_eq!(
                call("json", "loads", &[string(source)]),
                NONE,
                "input {source:?}"
            );
            assert_eq!(exc::pending_tag(), VALUE_ERROR as i64, "input {source:?}");
            assert_eq!(message(), expected, "input {source:?}");
        };
        case("", "Expecting value");
        case("   ", "Expecting value");
        case("{", "Expecting property name enclosed in double quotes");
        case("{,}", "Expecting property name enclosed in double quotes");
        case("[1,", "Expecting value");
        case("[1,]", "Expecting value");
        case("[1 2]", "Expecting ',' delimiter");
        case("{\"a\" 1}", "Expecting ':' delimiter");
        case("{\"a\": 1", "Expecting ',' delimiter");
        case("1 2", "Extra data");
        case("\"abc", "Unterminated string starting at");
        case("\"a\\q\"", "Invalid \\escape");
        case("\"a\nb\"", "Invalid control character at");
        case("tru", "Expecting value");

        exc::init();
        let deep = "[".repeat(MAX_JSON_DEPTH + 5) + &"]".repeat(MAX_JSON_DEPTH + 5);
        assert_eq!(call("json", "loads", &[string(&deep)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(message(), "Maximum nesting depth exceeded");

        exc::init();
        assert_eq!(call("json", "loads", &[int(5)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "the JSON object must be str, bytes or bytearray, not int"
        );

        exc::init();
        assert_eq!(call("json", "loads", &[NONE]), NONE);
        assert_eq!(
            message(),
            "the JSON object must be str, bytes or bytearray, not NoneType"
        );
    }

    #[test]
    fn json_roundtrip() {
        exc::init();
        let source = "{\"name\": \"PyX\", \"n\": 3, \"ok\": true, \"items\": [1, 2.5, null], \
                      \"nested\": {\"x\": \"y\"}}";
        let parsed = loads(source);
        assert_eq!(dumps(parsed), source);
        let dumped = dumps(parsed);
        assert_eq!(dumps(loads(&dumped)), dumped);
    }

    #[test]
    fn json_nested_survives_collection() {
        exc::init();
        let source = "{\"a\": [1, {\"b\": [\"x\", \"y\", {\"c\": null}]}], \"d\": true}";
        let parsed = call("json", "loads", &[string(source)]);
        assert!(!exc::pending(), "loads raised: {}", message());
        // Force a collection while the parsed value is only rooted on the
        // native stack, then serialize the surviving graph.
        crate::abi::pyx_gc_collect();
        let dumped = call("json", "dumps", &[parsed]);
        assert!(!exc::pending(), "dumps raised: {}", message());
        assert_eq!(value::to_string(dumped), source);

        // Dumping after the collector ran again is still stable.
        crate::abi::pyx_gc_collect();
        assert_eq!(value::to_string(dumped), source);
    }

    // ---------------------------------------------------------------------
    // os
    // ---------------------------------------------------------------------

    #[test]
    fn os_functions_and_constants() {
        exc::init();
        assert!(as_int(call("os", "getpid", &[])) > 0);
        assert!(as_int(call("os", "getppid", &[])) >= 0);
        assert!(as_int(call("os", "cpu_count", &[])) >= 1);
        assert!(!value::as_str(call("os", "getcwd", &[]))
            .unwrap_or("")
            .is_empty());

        assert_eq!(
            value::as_str(get("os", "name")),
            Some(if cfg!(windows) { "nt" } else { "posix" })
        );
        assert_eq!(
            value::as_str(get("os", "sep")),
            Some(if cfg!(windows) { "\\" } else { "/" })
        );

        assert_eq!(
            call("os", "getenv", &[string("DEFINITELY_NOT_SET_PYX")]),
            NONE
        );
        assert_eq!(
            value::as_str(call(
                "os",
                "getenv",
                &[string("DEFINITELY_NOT_SET_PYX"), string("fallback")]
            )),
            Some("fallback")
        );

        std::env::set_var("PYX_TEST_OS_ENV", "value");
        assert_eq!(
            value::as_str(call("os", "getenv", &[string("PYX_TEST_OS_ENV")])),
            Some("value")
        );
        std::env::remove_var("PYX_TEST_OS_ENV");

        exc::init();
        assert_eq!(call("os", "getenv", &[int(5)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "str expected, not int");

        exc::init();
        assert_eq!(call("os", "getpid", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "os.getpid() takes 0 arguments (1 given)");

        exc::init();
        assert_eq!(get("os", "nope"), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'os' has no attribute 'nope'");
    }

    #[test]
    fn asyncio_eager_helpers() {
        exc::init();
        // `run(value)` returns the already-computed argument.
        assert_eq!(call("asyncio", "run", &[int(7)]), int(7));
        assert!(!exc::pending());

        // `gather(iterable)` materialises the elements in order.
        let values = value::list_new(vec![int(1), int(2), int(3)]);
        let gathered = call("asyncio", "gather", &[values]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_list(gathered).expect("list").to_vec(),
            vec![int(1), int(2), int(3)]
        );

        // `gather` over a non-iterable fails like any other iteration.
        exc::init();
        assert_eq!(call("asyncio", "gather", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);

        // `sleep` blocks the calling thread and returns None.
        exc::init();
        assert_eq!(call("asyncio", "sleep", &[value::float_new(0.001)]), NONE);
        assert!(!exc::pending());
        assert_eq!(call("asyncio", "sleep", &[int(0)]), NONE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(call("asyncio", "sleep", &[int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);

        // Unsupported APIs raise AttributeError instead of pretending to be
        // coroutines.
        exc::init();
        assert_eq!(call("asyncio", "create_task", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "module 'asyncio' has no attribute 'create_task'");
    }
}
