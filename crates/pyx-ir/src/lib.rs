//! PyX typed HIR — the single contract between frontend and codegen.
//!
//! The frontend produces a [`Program`] with fully resolved local slots, function
//! ids and [`Type`]s. The codegen must not perform name resolution or type
//! inference; it only lowers this tree to machine code.
//!
//! Invariants:
//! - `functions[0]` is the module body, named `<module>`, with no parameters
//!   and return type [`Type::None`]. [`Program::main`] points at it.
//! - Every [`LocalId`] indexes the enclosing function's `locals` vector.
//!   Parameters are locals too and appear first in `locals`.
//! - Each [`FunctionId`] indexes `Program::functions`.
//! - `line` is 1-based and refers to the source file given by
//!   [`Program::source_name`].
//! - Nested functions are closures by value: [`StmtKind::Closure`] captures
//!   the current values of enclosing locals. Classes are limited to a single
//!   optional base and plain methods.

pub mod compat;
mod ir;
mod model;
pub mod stdlib;

#[cfg(test)]
mod tests;

pub use crate::compat::{PYTHON_ABI_TAG, PYTHON_VERSION, PYTHON_VERSION_STRING};
pub use crate::ir::{
    ClassMethod, CompClause, CompTarget, Expr, ExprKind, Function, Handler, LocalInfo, Param,
    Program, Stmt, StmtKind,
};
pub use crate::model::{
    BinOp, BoolOp, Builtin, Callee, Const, ExcKind, FunctionId, LocalId, Type, UnOp,
};
