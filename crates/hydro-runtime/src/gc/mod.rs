//! Conservative mark-&-sweep garbage collector.
//!
//! [`roots`] holds the precise shadow-stack root registry that generated code
//! registers its frames with (P2-06/P2-07); [`heap`] is the collector itself.
//! The collector marks those frames exactly and excludes their extents from
//! the conservative scan; `HYDRO_GC_PRECISE=1` disables the conservative
//! stack/register scan entirely (strict mode). Persistent root slots in the
//! thread registration cover long-lived Rust-side owners.
//!
//! Weak references (`_weakref.ReferenceType`) live in the heap's weak side
//! table: they record a target address without a traced edge, so they never
//! keep the target alive, and the collector clears them when the target is
//! swept. `getweakrefs` pins the references it materialises until the result
//! list is reachable.

pub mod heap;
pub mod roots;
