//! Interpreter frames and precise GC root management.
//!
//! # Root discipline
//!
//! A [`Frame`] registers two shadow frames with the runtime collector:
//!
//! * the fixed local-slot array (`capacity = function.locals.len()`), pushed
//!   before any runtime call and popped when the activation exits; and
//! * a growable temporary-value stack ([`Scratch`]) used for every
//!   intermediate expression value.
//!
//! The rule throughout the interpreter is that a tagged value is written to
//! one of those slots *before* any later evaluation step can allocate. Nested
//! calls push their own frames, so the root stack is strictly LIFO: a child
//! frame is always popped before its parent resumes. `Scratch::truncate` keeps
//! the frame's scanned prefix tight, and growth detaches the old frame,
//! allocates a larger Rust buffer and re-registers — Rust allocation never
//! triggers a Hydro collection, so no value is lost in the move.
//!
//! Values passed to a runtime call from a temporary `Vec` are additionally
//! covered by the caller frames' roots (every element was produced by an
//! expression that owns a scratch slot), matching the generated code's
//! `gc_scope_push` usage.

use std::sync::Arc;

use hydro_ir::{Function, Program};
use hydro_runtime::abi;

/// Control-flow result of executing an IR statement or expression.
#[derive(Debug)]
pub enum Signal {
    /// `return value`; the payload is already parked in the frame's scratch
    /// stack so it survives `finally` bodies and handler contexts.
    Return(i64),
    Break,
    Continue,
    /// An exception is pending in the runtime; the handler stack has not been
    /// inspected yet.
    Raise,
}

pub type Exec = Result<(), Signal>;

/// Growable rooted value stack for one interpreter frame.
pub struct Scratch {
    slots: Vec<i64>,
    token: usize,
    used: usize,
}

impl Scratch {
    const INITIAL: usize = 16;

    fn new() -> Scratch {
        let mut slots = vec![0i64; Self::INITIAL];
        let none = hydro_runtime::host::none();
        slots.fill(none);
        let token = abi::hydro_gc_frame_push(slots.as_mut_ptr(), slots.len());
        abi::hydro_gc_frame_used(token, 0);
        Scratch {
            slots,
            token,
            used: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.used
    }

    pub fn push(&mut self, value: i64) {
        if self.used == self.slots.len() {
            self.grow();
        }
        self.slots[self.used] = value;
        self.used += 1;
        abi::hydro_gc_frame_used(self.token, self.used);
    }

    pub fn truncate(&mut self, mark: usize) {
        if mark < self.used {
            self.used = mark;
            abi::hydro_gc_frame_used(self.token, self.used);
        }
    }

    pub fn value(&self, index: usize) -> i64 {
        self.slots[index]
    }

    pub fn store(&mut self, index: usize, value: i64) {
        self.slots[index] = value;
    }

    fn grow(&mut self) {
        let capacity = self.slots.len() * 2;
        abi::hydro_gc_frame_pop(self.token);
        let mut slots = vec![0i64; capacity];
        let none = hydro_runtime::host::none();
        slots.fill(none);
        slots[..self.used].copy_from_slice(&self.slots[..self.used]);
        self.token = abi::hydro_gc_frame_push(slots.as_mut_ptr(), capacity);
        self.slots = slots;
        abi::hydro_gc_frame_used(self.token, self.used);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.token != 0 {
            abi::hydro_gc_frame_pop(self.token);
        }
    }
}

/// One interpreter activation.
pub struct Frame {
    pub program: Arc<Program>,
    pub base: u64,
    /// IR function this frame executes (main/module body has id `Program::main`).
    pub function: hydro_ir::FunctionId,
    pub locals: Vec<i64>,
    /// Whether each local slot has been assigned by source-level code since
    /// the frame started. Session flushing uses this to distinguish a real
    /// `name = None` binding from an untouched local slot (imports bind
    /// through `GlobalSet` and leave the local slot at its initial `None`).
    pub assigned: Vec<bool>,
    /// Extra rooted slot holding the value of a `return` in flight (the
    /// statement's scratch marks are released before enclosing `finally`
    /// bodies run).
    pub return_slot: usize,
    local_token: usize,
    pub scratch: Scratch,
    /// Exception captured by an active `except` arm of this function (bare
    /// `raise` must not observe a caller's handler).
    pub handled: Vec<i64>,
    /// Exception parked by an active `finally` of this function.
    pub finally_slots: Vec<i64>,
    /// True for a REPL/`-c` session fragment's module body: a trailing
    /// top-level expression statement records its value for echoing.
    pub echo: bool,
    /// Nesting depth of statement execution within this frame: `0` while the
    /// frame's top-level body is about to run a statement, `1` inside that
    /// statement and `2` or more inside a nested block.
    pub stmt_depth: usize,
}

impl Frame {
    pub fn new(
        program: Arc<Program>,
        base: u64,
        function: hydro_ir::FunctionId,
        info: &Function,
    ) -> Frame {
        let return_slot = info.locals.len();
        let mut locals = vec![0i64; return_slot + 1];
        let assigned = vec![false; return_slot + 1];
        let local_token = abi::hydro_gc_frame_push(locals.as_mut_ptr(), locals.len());
        // `frame_push` filled every slot with tagged `None`; no explicit
        // initialisation is needed for locals that the body never writes.
        let scratch = Scratch::new();
        Frame {
            program,
            base,
            function,
            locals,
            assigned,
            return_slot,
            local_token,
            scratch,
            handled: Vec::new(),
            finally_slots: Vec::new(),
            echo: false,
            stmt_depth: 0,
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // Scratch was registered after the locals, so it is popped first.
        // Inner frames are already gone (strict LIFO).
        let scratch = std::mem::replace(&mut self.scratch, Scratch::empty());
        drop(scratch);
        abi::hydro_gc_frame_pop(self.local_token);
    }
}

impl Scratch {
    /// A placeholder that owns no registration, only used to move the real
    /// scratch out of a frame during `Drop` without invoking its destructor
    /// twice.
    fn empty() -> Scratch {
        Scratch {
            slots: Vec::new(),
            token: 0,
            used: 0,
        }
    }
}
