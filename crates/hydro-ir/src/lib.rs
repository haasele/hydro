//! Hydro typed HIR — the single contract between frontend and codegen.
//!
//! The frontend produces a [`Program`] with fully resolved local slots, function
//! ids and [`Type`]s. The codegen must not perform name resolution or type
//! inference; it only lowers this tree to machine code.
//!
//! Invariants:
//! - [`Program::main`] points at the entry module body: a function named
//!   `<module>` with no parameters and return type [`Type::None`]. Single-
//!   source programs always have it at index `0`; multi-module programs emit
//!   imported module bodies first and list them in [`Program::initializers`].
//! - Every [`LocalId`] indexes the enclosing function's `locals` vector.
//!   Parameters are locals too and appear first in `locals`.
//! - Each [`FunctionId`] indexes `Program::functions`.
//! - `line` is 1-based and refers to the source file given by
//!   [`Program::source_name`].
//! - Nested functions are closures: [`StmtKind::Closure`] captures the
//!   current values of enclosing locals, or the cell for bindings mutated
//!   through `nonlocal` and for `def` names a nested closure reads
//!   (recursion, mutual recursion). A class carries its base expressions in
//!   source order
//!   (the runtime resolves the MRO) and an optional `metaclass` value; its
//!   methods are plain, property, classmethod or staticmethod entries.

pub mod compat;
mod ir;
mod model;
pub mod stdlib;

#[cfg(test)]
mod tests;

pub use crate::compat::{PYTHON_ABI_TAG, PYTHON_VERSION, PYTHON_VERSION_STRING};
pub use crate::ir::{
    ClassAttr, ClassMethod, CompClause, CompTarget, DeleteTarget, Expr, ExprKind, Function,
    Handler, LocalInfo, MethodKind, Param, ParamKind, Program, Stmt, StmtKind,
    TemplateInterpolation, UnpackTarget,
};
pub use crate::model::{
    BinOp, BoolOp, Builtin, Callee, Const, ExcKind, FunctionId, LocalId, Type, UnOp,
};
