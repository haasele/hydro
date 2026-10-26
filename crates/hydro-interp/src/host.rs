//! Host function registry and the runtime dispatcher.
//!
//! Every IR function can be materialised as a runtime function value
//! (`hydro_runtime::host::function_new`) whose body is a `FunctionBody::Host`
//! token. This module owns the process-wide token table and the callback the
//! runtime invokes for such values, which is what makes interpreted functions
//! usable from native code: `sorted(key=...)`, class methods, constructors,
//! `functools.partial`, `threading.Thread` and generator/coroutine fibers.
//!
//! A callback runs on a fresh [`Interp`] frame stack. Module globals live in
//! the runtime global table (IR `GlobalGet`/`GlobalSet`), so independent frame
//! stacks observe the same module state, including callbacks arriving on a
//! `threading.Thread`'s OS thread.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use hydro_ir::{FunctionId, Program};
use hydro_runtime::abi;
use hydro_runtime::host as runtime_host;

use crate::{Interp, Signal};

/// One registered IR function: the program that owns it and its id.
#[derive(Clone)]
struct RegistryEntry {
    program: Arc<Program>,
    base: u64,
    function: FunctionId,
}

struct Registry {
    entries: Vec<RegistryEntry>,
    bases: HashMap<usize, u64>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        entries: Vec::new(),
        bases: HashMap::new(),
    })
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Handle for one program registered with the interpreter runtime.
#[derive(Clone)]
pub struct ProgramId {
    pub(crate) program: Arc<Program>,
    pub(crate) base: u64,
}

impl ProgramId {
    pub fn program(&self) -> &Arc<Program> {
        &self.program
    }

    /// Token base of this program; `base + FunctionId` addresses a function.
    pub fn base(&self) -> u64 {
        self.base
    }
}

/// Registers every function of `program` and returns the token base.
///
/// Registering the same program twice returns the same base. The dispatcher is
/// installed on first use; installing is idempotent.
pub fn install_program(program: Arc<Program>) -> u64 {
    let key = Arc::as_ptr(&program) as usize;
    let mut registry = registry();
    if let Some(&base) = registry.bases.get(&key) {
        return base;
    }
    let base = registry.entries.len() as u64;
    for id in 0..program.functions.len() {
        registry.entries.push(RegistryEntry {
            program: program.clone(),
            base,
            function: id as FunctionId,
        });
    }
    registry.bases.insert(key, base);
    drop(registry);
    runtime_host::set_dispatch(dispatch);
    base
}

fn lookup(token: u64) -> Option<RegistryEntry> {
    let registry = registry();
    registry.entries.get(token as usize).cloned()
}

/// Runtime callback: enters the interpreter for one `Host` function value.
fn dispatch(token: u64, args: &[i64]) -> i64 {
    let Some(entry) = lookup(token) else {
        return raise_runtime_error("interpreter function token is not registered");
    };
    let mut interp = Interp::new("");
    match interp.invoke_host(entry.program, entry.base, entry.function, args) {
        Ok(value) => value,
        Err(Signal::Raise) => runtime_host::none(),
        Err(_) => raise_runtime_error("interpreter function returned an invalid control signal"),
    }
}

fn raise_runtime_error(message: &str) -> i64 {
    abi::hydro_raise_msg(
        hydro_ir::ExcKind::RuntimeError.tag(),
        message.as_ptr(),
        message.len(),
    );
    runtime_host::none()
}
