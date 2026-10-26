//! Production tree-walk interpreter for the Hydro typed IR (P2-14).
//!
//! The interpreter executes [`hydro_ir::Program`] directly against the shared
//! runtime ABI (`hydro_runtime::abi`). It is deliberately not a second value
//! model: every value is a runtime tagged `i64`, every operator goes through
//! `hydro_binop`/`hydro_unop`, every container is a runtime object, and every
//! allocation is bracketed by precise GC roots registered through the
//! `hydro_gc_frame_push`/`hydro_gc_scope_push` runtime ABI.
//!
//! # Execution model
//!
//! One [`Interp`] owns the frame stack for its session. Calling an IR
//! function pushes a [`frame::Frame`]: a fixed-size local slot array
//! registered with `frame_push`, a growable temporary-value root frame and the
//! handler-context baseline captured at entry. Values live in those slots or
//! in a temporary slot until the expression that owns them finishes, so a
//! collection triggered by any later runtime call always sees every live value.
//!
//! Direct calls to program functions push a VM frame. Indirect calls,
//! keyword/starred/method/module/class calls, native callbacks and
//! generator/coroutine bodies all go through the runtime: IR functions are
//! materialised as [`hydro_runtime::host::function_new`] values, whose
//! `FunctionBody::Host` body calls the process-wide
//! dispatcher installed by [`install_program`]. Generator and coroutine
//! functions run on runtime fibers by passing an interpreter trampoline to
//! `hydro_generator_new`/`hydro_coroutine_new`.
//!
//! Imports are resolved by the frontend; the interpreter runs
//! [`Program::initializers`] once, in dependency order, before
//! [`Program::main`].
//!
//! # Persistent sessions
//!
//! [`Interp::run_fragment`] executes one incrementally compiled REPL/`-c`
//! fragment in a long-lived session: the newly imported module bodies run
//! first, the fragment's module body follows, and its source-level bindings
//! are flushed into the module global table on both the normal and the
//! exceptional exit path. With echo enabled the value of a trailing top-level
//! expression is installed as `_` and returned in the
//! [`FragmentOutcome`]. A plain [`Interp::run`] is unchanged: it runs one
//! whole program and flushes nothing.
//!
//! # Panics
//!
//! Malformed IR is reported through the runtime exception mechanism
//! (`RuntimeError`), never a Rust panic on user input.

mod eval;
mod frame;
mod host;
mod session;
mod stmt;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use hydro_ir::{FunctionId, Program};
use hydro_runtime::abi;
use hydro_runtime::host as runtime_host;

pub use frame::{Exec, Signal};
pub use host::{install_program, ProgramId};
pub use session::FragmentOutcome;

/// Per-run options for one session fragment ([`Interp::run_fragment`]).
///
/// A plain [`Interp::run`]/[`Interp::call`] passes `None`: nothing is flushed
/// and no expression is echoed.
pub(crate) struct FragmentMeta<'a> {
    /// Source-level module bindings flushed into the module global table when
    /// the fragment's module body exits, whether it completed or raised.
    pub bound_names: &'a [String],
    /// Whether the final top-level expression value becomes `_`.
    pub echo: bool,
}

/// One interpreter session: program table, current module name and frame
/// stack.
pub struct Interp {
    /// Programs available to this session, with their token bases.
    programs: Vec<ProgramId>,
    /// Module key used by session globals (the CLI uses `__main__`).
    pub module: String,
    frames: Vec<frame::Frame>,
    /// Value of the last top-level expression statement of the module body
    /// currently running with echo enabled. Reset before every other
    /// top-level statement, so only a trailing expression yields a value.
    last_value: Option<i64>,
}

impl Interp {
    pub fn new(module: impl Into<String>) -> Interp {
        Interp {
            programs: Vec::new(),
            module: module.into(),
            frames: Vec::new(),
            last_value: None,
        }
    }

    /// Registers `program` (globally) and adds it to this session.
    pub fn add_program(&mut self, program: Arc<Program>) -> ProgramId {
        let base = install_program(program.clone());
        let id = ProgramId { program, base };
        self.programs.push(id.clone());
        id
    }

    /// Runs the program's initializers once and then its module body.
    ///
    /// Returns `false` when an uncaught exception is pending; the caller
    /// decides how to report it (`hydro_uncaught` for the CLI). Each initializer
    /// stops the run on the first exception, exactly like generated
    /// `hydro_main`.
    pub fn run(&mut self, id: &ProgramId) -> bool {
        for &initializer in &id.program.initializers {
            if self
                .invoke_bound(id.program.clone(), id.base, initializer, &[])
                .is_err()
            {
                return false;
            }
        }
        self.invoke_bound(id.program.clone(), id.base, id.program.main, &[])
            .is_ok()
    }

    /// Calls a program function directly from Rust with already-bound slots.
    pub fn call(
        &mut self,
        id: &ProgramId,
        function: FunctionId,
        bound_slots: &[i64],
    ) -> Result<i64, Signal> {
        self.invoke_bound(id.program.clone(), id.base, function, bound_slots)
    }
}

impl Interp {
    pub(crate) fn current_frame(&self) -> &frame::Frame {
        self.frames
            .last()
            .expect("interpreter frame stack must not be empty during evaluation")
    }

    pub(crate) fn current_frame_mut(&mut self) -> &mut frame::Frame {
        self.frames
            .last_mut()
            .expect("interpreter frame stack must not be empty during evaluation")
    }

    pub(crate) fn local(&self, id: hydro_ir::LocalId) -> i64 {
        self.current_frame().locals[id as usize]
    }

    pub(crate) fn set_local(&mut self, id: hydro_ir::LocalId, value: i64) {
        let frame = self.current_frame_mut();
        frame.locals[id as usize] = value;
        if let Some(assigned) = frame.assigned.get_mut(id as usize) {
            *assigned = true;
        }
    }

    /// Mirrors a runtime global write into every live module-body frame.
    ///
    /// Module-level names are locals of function 0 in the IR, while a
    /// `global name` write inside a function updates only the runtime global
    /// table. CPython exposes one module namespace, so a later module-body
    /// read must observe that write. Module-body frames are the only frames
    /// whose locals are the module namespace.
    pub(crate) fn sync_module_global(&mut self, name: &str, value: i64) {
        for frame in &mut self.frames {
            if frame.function != frame.program.main {
                continue;
            }
            let Some(index) = frame
                .program
                .functions
                .get(frame.function as usize)
                .and_then(|function| function.locals.iter().position(|local| local.name == name))
            else {
                continue;
            };
            frame.locals[index] = value;
            frame.assigned[index] = true;
        }
    }

    pub(crate) fn scratch_push(&mut self, value: i64) {
        self.current_frame_mut().scratch.push(value);
    }

    pub(crate) fn scratch_mark(&self) -> usize {
        self.current_frame().scratch.len()
    }

    pub(crate) fn scratch_release(&mut self, mark: usize) {
        self.current_frame_mut().scratch.truncate(mark);
    }

    pub(crate) fn scratch_value(&self, index: usize) -> i64 {
        self.current_frame().scratch.value(index)
    }

    pub(crate) fn scratch_store(&mut self, index: usize, value: i64) {
        self.current_frame_mut().scratch.store(index, value);
    }

    /// Roots a `return` value in the frame while enclosing `finally` bodies
    /// and handler contexts run.
    pub(crate) fn set_return_value(&mut self, value: i64) {
        let slot = self.current_frame().return_slot;
        self.current_frame_mut().locals[slot] = value;
    }

    /// Reports a runtime pending exception as an unwind signal.
    pub(crate) fn check<T>(&self, value: T) -> Result<T, Signal> {
        if abi::hydro_exc_pending() != 0 {
            Err(Signal::Raise)
        } else {
            Ok(value)
        }
    }

    pub(crate) fn type_error(&self, message: &str) -> Signal {
        abi::hydro_raise_msg(
            hydro_ir::ExcKind::TypeError.tag(),
            message.as_ptr(),
            message.len(),
        );
        Signal::Raise
    }

    pub(crate) fn runtime_error(&self, message: &str) -> Signal {
        abi::hydro_raise_msg(
            hydro_ir::ExcKind::RuntimeError.tag(),
            message.as_ptr(),
            message.len(),
        );
        Signal::Raise
    }

    /// Runtime entry point used by the host dispatcher.
    pub(crate) fn invoke_host(
        &mut self,
        program: Arc<Program>,
        base: u64,
        function: FunctionId,
        slots: &[i64],
    ) -> Result<i64, Signal> {
        self.invoke_bound(program, base, function, slots)
    }

    /// Pushes a frame for `function`, runs its body and returns its result.
    ///
    /// `slots` must contain exactly one value per declared parameter
    /// (captures included); the caller has already bound defaults.
    fn invoke_bound(
        &mut self,
        program: Arc<Program>,
        base: u64,
        function_id: FunctionId,
        slots: &[i64],
    ) -> Result<i64, Signal> {
        Ok(self.invoke_meta(program, base, function_id, slots, None)?.0)
    }

    /// [`Self::invoke_bound`] with optional session-fragment bookkeeping.
    ///
    /// With `fragment`, the module body's source-bound locals are flushed into
    /// the module global table after the body finishes, on both the normal and
    /// the exceptional path, and (when `echo` is set) the value of a trailing
    /// top-level expression is installed as `_`. The returned expression value
    /// stays reachable through that global entry after the frame is popped.
    pub(crate) fn invoke_meta(
        &mut self,
        program: Arc<Program>,
        base: u64,
        function_id: FunctionId,
        slots: &[i64],
        fragment: Option<FragmentMeta<'_>>,
    ) -> Result<(i64, Option<i64>), Signal> {
        let func = &program.functions[function_id as usize];
        if slots.len() != func.params.len() {
            abi::hydro_arity_error(func.params.len() as i64, slots.len() as i64);
            return Err(Signal::Raise);
        }
        let handler_base = abi::hydro_exc_handler_depth();
        let mut frame = frame::Frame::new(program.clone(), base, function_id, func);
        if let Some(meta) = &fragment {
            // Only a session fragment's module body records expression values.
            frame.echo = meta.echo;
        }
        self.frames.push(frame);
        // Publish the bound parameter slots after the frame is registered.
        for (index, &value) in slots.iter().enumerate() {
            let frame = self.frames.last_mut().unwrap();
            frame.locals[index] = value;
            frame.assigned[index] = true;
        }

        // Function entry: poll the collector before touching the heap, then
        // publish the traceback frame.
        abi::hydro_safepoint();
        abi::hydro_frame_enter(func.name.as_ptr(), func.name.len());

        let result = self.exec_stmts(&func.body);

        let mut echoed = None;
        if let Some(meta) = fragment {
            // Mirror the module body's locals into the shared global table
            // before the frame (and their only roots) disappears. An aborted
            // fragment keeps the bindings it managed to assign.
            self.flush_fragment_globals(func, meta.bound_names);
            if meta.echo {
                if result.is_ok() {
                    echoed = self.take_expression_value();
                } else {
                    self.last_value = None;
                }
            }
        }

        // Leaving the activation unwinds handler contexts to the baseline,
        // then drops the shadow frame and the traceback entry.
        abi::hydro_exc_handler_truncate(handler_base);
        abi::hydro_frame_leave();
        self.frames.pop();

        match result {
            Ok(()) => Ok((runtime_host::none(), echoed)),
            Err(Signal::Return(value)) => Ok((value, echoed)),
            Err(Signal::Raise) => Err(Signal::Raise),
            Err(Signal::Break) | Err(Signal::Continue) => {
                self.runtime_error("'break' outside loop");
                Err(Signal::Raise)
            }
        }
    }

    /// Flushes `names` from the module body's locals into the global table.
    ///
    /// A name the body never assigned keeps its `None` slot value, matching
    /// the frontend's rule that every module-level binding of a session
    /// fragment is mirrored into the shared namespace.
    fn flush_fragment_globals(&self, info: &hydro_ir::Function, names: &[String]) {
        let Some(frame) = self.frames.last() else {
            return;
        };
        for name in names {
            let Some(index) = info.locals.iter().position(|local| local.name == *name) else {
                continue;
            };
            if !frame.assigned.get(index).copied().unwrap_or(false) {
                continue;
            }
            abi::hydro_global_set(
                self.module.as_ptr(),
                self.module.len(),
                name.as_ptr(),
                name.len(),
                frame.locals[index],
            );
        }
    }

    /// Installs the recorded expression value as `_` and returns it.
    ///
    /// `None` results (and a failed `_` store) echo nothing, matching
    /// CPython's display hook.
    fn take_expression_value(&mut self) -> Option<i64> {
        let value = self.last_value.take()?;
        if value == runtime_host::none() {
            return None;
        }
        abi::hydro_global_set(
            self.module.as_ptr(),
            self.module.len(),
            b"_".as_ptr(),
            1,
            value,
        );
        if abi::hydro_exc_pending() != 0 {
            abi::hydro_exc_clear();
            return None;
        }
        Some(value)
    }

    /// Looks up the program that owns the current frame.
    pub(crate) fn current_program(&self) -> (Arc<Program>, u64) {
        let frame = self.current_frame();
        (frame.program.clone(), frame.base)
    }
}
