//! Persistent session support for the REPL and `pyx -c`/`pyx -i`.
//!
//! A [`crate::Interp`] keeps its programs and its frame stack for the whole
//! process lifetime. The frontend's `SessionCompiler` compiles one fragment at
//! a time against a shared namespace and reports the source-level bindings of
//! each fragment; [`Interp::run_fragment`] is the runtime half of that
//! contract:
//!
//! * the fragment's newly imported module bodies (initializers) run first,
//!   exactly once per session;
//! * the fragment's module body then runs in a frame;
//! * on both the normal and the exceptional exit path the body's locals for
//!   `bound_names` are copied into the module global table, so names assigned
//!   by the fragment stay visible to later fragments and to the functions and
//!   classes it defined;
//! * when echo is enabled and the fragment ends in a top-level expression
//!   statement, that expression's value is installed as `_` and returned.
//!
//! The value is returned only after it was stored in the global table: module
//! globals are GC roots, so the returned tagged value stays valid after the
//! fragment's frame (and its precise root range) is gone.
//!
//! # Echo rules
//!
//! Only the *trailing* top-level expression statement of the fragment
//! produces a value. An assignment, `if`, `for`, `while`, `try`, `def` or
//! `class` statement clears the pending value, so `x = 1` and an `if` block
//! never echo. Nested expression statements (inside a block) are not echoed
//! either, matching interactive CPython. A `None` result echoes nothing and
//! leaves `_` untouched.
//!
//! # Limitations
//!
//! The session namespace is the module global table keyed by
//! [`Interp::module`]; it is not a `__main__` module object, so `dir()`,
//! `globals()` and module introspection do not see the session bindings
//! (that is a runtime-module limitation, not a session one). Programs
//! registered for earlier fragments stay registered for the life of the
//! process, so session memory grows with the number of fragments.

use std::sync::Arc;

use pyx_ir::{FunctionId, Program};

use crate::{FragmentMeta, Interp};

/// Outcome of running one session fragment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FragmentOutcome {
    /// Whether the fragment's module body ran to completion. `false` means an
    /// uncaught exception is pending for the caller to report (and clear)
    /// with `pyx_uncaught`.
    pub completed: bool,
    /// Value of the fragment's trailing top-level expression, already
    /// installed as `_` in the session global table. `None` when the fragment
    /// does not end in an echoable expression or the value is `None`.
    pub value: Option<i64>,
}

impl Interp {
    /// Registers `program` with this session and runs one session fragment.
    ///
    /// `initializers` are the module bodies this fragment introduced (in
    /// dependency order); modules imported by an earlier fragment are linked
    /// but not re-initialized. `bound_names` are the fragment's source-level
    /// module bindings, flushed into the global table when the module body
    /// exits. `echo` enables the trailing-expression echo described in the
    /// module documentation.
    ///
    /// The caller reports a pending exception after a `completed == false`
    /// result; the session remains usable and the flushed bindings persist.
    pub fn run_fragment(
        &mut self,
        program: Arc<Program>,
        initializers: &[FunctionId],
        bound_names: &[String],
        echo: bool,
    ) -> FragmentOutcome {
        let main = program.main;
        let id = self.add_program(program);
        self.last_value = None;

        for &initializer in initializers {
            // Imported module bodies share the session's global table but
            // never echo and have no fragment bindings of their own.
            let meta = FragmentMeta {
                bound_names: &[],
                echo: false,
            };
            if self
                .invoke_meta(id.program.clone(), id.base, initializer, &[], Some(meta))
                .is_err()
            {
                return FragmentOutcome {
                    completed: false,
                    value: None,
                };
            }
        }

        let meta = FragmentMeta { bound_names, echo };
        match self.invoke_meta(id.program.clone(), id.base, main, &[], Some(meta)) {
            Ok((_, value)) => FragmentOutcome {
                completed: true,
                value,
            },
            Err(_) => FragmentOutcome {
                completed: false,
                value: None,
            },
        }
    }
}
