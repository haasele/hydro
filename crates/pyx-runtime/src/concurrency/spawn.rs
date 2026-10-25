//! Function registry and child entry point for the `spawn` start method.
//!
//! A spawned child is a fresh process (`exec`), so wrapper addresses from the
//! parent are meaningless there. Generated `pyx_main` code registers every
//! program function by name before the module body runs; a child reads the
//! payload written by `multiprocessing.Process.start`, resolves the target
//! function through this registry and invokes it directly. The module body is
//! never executed in the child.
//!
//! The payload file is named by `PYX_SPAWN_PAYLOAD`. Without it,
//! [`spawn_child`] returns `-1`, which tells the generated `pyx_main` to run
//! the normal module body.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::value;
use crate::{exc, process, serialize};

/// Registry mapping a Python-visible function name to its wrapper entry point
/// and exact positional arity.
static FUNCTIONS: LazyLock<Mutex<HashMap<String, (usize, usize)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn functions() -> MutexGuard<'static, HashMap<String, (usize, usize)>> {
    FUNCTIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Drops every registration (a new program run starts with a fresh registry)
/// and installs the runtime's own native entries. A spawned pool worker is
/// started exactly like a generated function, so its entry point must be
/// resolvable by name before the module body registers user functions.
pub(crate) fn reset() {
    functions().clear();
    register(
        crate::pool::WORKER_NAME,
        crate::pool::pool_worker_entry as *const () as usize,
        2,
    );
}

/// Registers one generated wrapper. `wrapper` must be non-zero.
pub(crate) fn register(name: &str, wrapper: usize, arity: usize) {
    if wrapper == 0 {
        return;
    }
    functions().insert(name.to_string(), (wrapper, arity));
}

/// ABI-side registration: copies the name out of generated stack storage.
///
/// Invalid pointers or non-UTF-8 names are ignored; generated code always
/// passes a valid buffer, and a malformed registration must not panic.
pub(crate) fn register_from_abi(
    name_ptr: *const u8,
    name_len: usize,
    wrapper: usize,
    arity: usize,
) {
    if name_ptr.is_null() || wrapper == 0 {
        return;
    }
    // SAFETY: generated code passes a readable `name_len`-byte name buffer.
    let bytes = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
    let Ok(name) = std::str::from_utf8(bytes) else {
        return;
    };
    register(name, wrapper, arity);
}

/// Resolves a registered function to its wrapper address and arity.
pub(crate) fn lookup(name: &str) -> Option<(usize, usize)> {
    functions().get(name).copied()
}

/// Every registered function as `(name, wrapper, arity)`, sorted by name.
///
/// The snapshot lets the test runner enumerate generated functions without
/// holding the registry lock while user code runs.
pub(crate) fn registered() -> Vec<(String, usize, usize)> {
    let mut entries: Vec<(String, usize, usize)> = functions()
        .iter()
        .map(|(name, (wrapper, arity))| (name.clone(), *wrapper, *arity))
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

/// Serializes unit tests that reset or consult the global registry.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Entry point called by generated `pyx_main` before the module body.
///
/// Returns `-1` when this process was not started by `Process.start` in
/// `spawn` mode. Otherwise the payload is consumed, the target runs, and the
/// process exit code (`0` success, `1` error) is returned.
pub(crate) fn spawn_child() -> i32 {
    let Some(path) = std::env::var_os("PYX_SPAWN_PAYLOAD").filter(|value| !value.is_empty()) else {
        return -1;
    };
    match run_payload(Path::new(&path)) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("pyx: spawn: {message}");
            process::flush_stdio();
            1
        }
    }
}

fn run_payload(path: &Path) -> Result<i32, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read payload '{}': {error}", path.display()))?;
    // The child owns the file once it has been started; remove it before the
    // target runs so an interrupted target does not leave it behind.
    let _ = fs::remove_file(path);

    let payload = serialize::decode(&bytes, true).map_err(|error| error.loads_message())?;
    let items = value::as_tuple(payload)
        .or_else(|| value::as_list(payload))
        .ok_or_else(|| "spawn payload must be a [name, args] pair".to_string())?;
    if items.len() != 2 {
        return Err("spawn payload must be a [name, args] pair".to_string());
    }
    let Some(name) = value::as_str(items[0]) else {
        return Err("spawn target name must be a str".to_string());
    };
    let args = value::as_tuple(items[1])
        .or_else(|| value::as_list(items[1]))
        .ok_or_else(|| "spawn arguments must be a tuple or list".to_string())?;
    let Some((wrapper, arity)) = lookup(name) else {
        return Err(format!(
            "function '{name}' is not registered in the spawned process"
        ));
    };
    if arity != args.len() {
        return Err(format!(
            "function '{name}' takes {arity} argument(s) but {} were given",
            args.len()
        ));
    }

    let argv: Box<[i64]> = args.to_vec().into_boxed_slice();
    // SAFETY: the registry only holds addresses of generated wrappers whose
    // ABI is `extern "C" fn(*const i64) -> i64` with exactly `arity`
    // arguments.
    let target: extern "C" fn(*const i64) -> i64 = unsafe { std::mem::transmute(wrapper) };
    let _ = target(argv.as_ptr());

    process::flush_stdio();
    if exc::pending() {
        let _ = exc::uncaught();
        process::flush_stdio();
        Ok(1)
    } else {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    /// Counts payload files so concurrently written tests cannot collide.
    static NEXT_PAYLOAD: AtomicU64 = AtomicU64::new(0);
    static LAST_ARG: AtomicI64 = AtomicI64::new(-1);

    extern "C" fn test_worker(argv: *const i64) -> i64 {
        // SAFETY: the test registers this wrapper with arity 1.
        let args = unsafe { std::slice::from_raw_parts(argv, 1) };
        let arg = value::as_integer(args[0]).unwrap_or(-1);
        LAST_ARG.store(arg, Ordering::Release);
        0
    }

    #[test]
    fn registry_lookup_and_reset() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset();
        assert_eq!(lookup("worker"), None);
        register("worker", 0x1234, 2);
        assert_eq!(lookup("worker"), Some((0x1234, 2)));
        register("zero", 0, 0);
        assert_eq!(lookup("zero"), None);
        reset();
        assert_eq!(lookup("worker"), None);
    }

    #[test]
    fn spawn_child_without_payload_returns_neg_one() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var("PYX_SPAWN_PAYLOAD");
        assert_eq!(crate::abi::pyx_spawn_child(), -1);
    }

    #[test]
    fn spawn_child_runs_registered_function() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset();
        register("worker", test_worker as *const () as usize, 1);

        let payload = value::tuple_new(vec![
            value::str_alloc("worker".to_string()),
            value::tuple_new(vec![value::encode_int(41).unwrap()]),
        ]);
        let bytes = serialize::encode(payload, true).expect("payload encodes");
        let sequence = NEXT_PAYLOAD.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-spawn-test-{}-{sequence}.payload",
            std::process::id()
        ));
        fs::write(&path, &bytes).expect("payload written");

        std::env::set_var("PYX_SPAWN_PAYLOAD", &path);
        let code = crate::abi::pyx_spawn_child();
        std::env::remove_var("PYX_SPAWN_PAYLOAD");
        reset();

        assert_eq!(code, 0);
        assert_eq!(LAST_ARG.load(Ordering::Acquire), 41);
        assert!(!path.exists(), "the child removes the payload file");
    }

    #[test]
    fn spawn_child_rejects_unknown_function() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset();
        let payload = value::tuple_new(vec![
            value::str_alloc("missing".to_string()),
            value::tuple_new(Vec::new()),
        ]);
        let bytes = serialize::encode(payload, true).expect("payload encodes");
        let sequence = NEXT_PAYLOAD.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-spawn-test-{}-{sequence}.payload",
            std::process::id()
        ));
        fs::write(&path, &bytes).expect("payload written");

        std::env::set_var("PYX_SPAWN_PAYLOAD", &path);
        let code = crate::abi::pyx_spawn_child();
        std::env::remove_var("PYX_SPAWN_PAYLOAD");

        assert_eq!(code, 1);
        assert!(!path.exists());
    }
}
