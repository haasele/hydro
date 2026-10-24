//! Tagged value model, operators, containers, iteration, builtins, methods
//! and exceptions.
//!
//! Every module here keeps its historical crate-root path (`crate::value`,
//! `crate::ops`, ...) through re-exports in `lib.rs`.

pub(crate) mod builtins;
pub(crate) mod container;
pub(crate) mod exc;
pub(crate) mod fiber;
pub(crate) mod format;
pub(crate) mod iter;
pub(crate) mod methods;
pub(crate) mod ops;
pub(crate) mod value;
