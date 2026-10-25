//! Fixed-size slab allocator for collector cells.
//!
//! Every object the collector owns is a [`PyxObj`]. Although its payload enum
//! has many variants, the *object* itself has one fixed Rust layout and one
//! fixed alignment: growing payloads (`String`, `Vec`) keep their buffers on
//! the system allocator. One size class is therefore enough, and the pool
//! documents that explicitly — variable object size classes remain future
//! work.
//!
//! Cells are grouped into slabs that are allocated with
//! `Layout::array::<PyxObj>(SLAB_OBJECTS)` and released with exactly the same
//! layout. A slab is never handed back to the system allocator while the pool
//! lives: released cells are pushed onto a free list and reused by the next
//! allocation, so a sweep or `free_all` does not cause allocator traffic.
//!
//! # Ownership
//!
//! * [`ObjectPool::allocate`] returns an uninitialised cell and initialises it
//!   with `ptr::write`.
//! * [`ObjectPool::release`] drops the payload with `ptr::drop_in_place` and
//!   returns the cell to the free list. It must be called exactly once per
//!   allocation.
//! * The shared collector splits those two steps (`drop_object` outside the
//!   heap lock, `reclaim` inside the short colour-reset window) because
//!   dropping a large payload must never run under the registry lock.
//! * `Drop` and [`ObjectPool::clear`] deallocate every slab; the caller must
//!   have released every cell before that, otherwise only the payload is
//!   leaked (the slab memory itself is still freed).
//!
//! The pool is not synchronised; the owning [`super::HeapState`] serialises
//! access (a thread-local heap before `_thread` is used, the `GLOBAL` mutex
//! afterwards).

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::ptr;
use std::sync::atomic::AtomicU8;

use crate::gc::heap::WHITE;
use crate::value::{ObjData, PyxObj};

/// Cells per slab. Small enough that a mostly empty heap wastes little, large
/// enough that the slab bookkeeping and the system allocation are amortised.
const SLAB_OBJECTS: usize = 64;

/// One system allocation holding `SLAB_OBJECTS` contiguous cells.
struct Slab {
    /// First cell; the remaining cells are `ptr.add(1 .. SLAB_OBJECTS)`.
    ptr: *mut PyxObj,
    /// The layout used for `alloc`; the only layout valid for `dealloc`.
    layout: Layout,
}

/// Read-only allocation statistics ([`ObjectPool::stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PoolStats {
    /// Slabs currently owned by the pool.
    pub(crate) slabs: usize,
    /// Cells across all slabs (`slabs * SLAB_OBJECTS`).
    pub(crate) capacity: usize,
    /// Cells handed out and not yet reclaimed.
    pub(crate) live: usize,
    /// Allocations served from previously released cells since the pool was
    /// created (not from a fresh slab's untouched reserve). Cumulative and
    /// monotonic: `clear` does not reset it.
    pub(crate) reused: usize,
}

/// Slab pool of fixed-size [`PyxObj`] cells with explicit free lists.
pub(crate) struct ObjectPool {
    /// Every slab this pool owns; each cell belongs to exactly one slab.
    slabs: Vec<Slab>,
    /// Cells never handed out yet (the remainder of a freshly grown slab).
    fresh: Vec<*mut PyxObj>,
    /// Cells handed out before and released again. Allocations prefer this
    /// list over `fresh`, so a sweep's cells are recycled before the pool's
    /// untouched reserve and LIFO reuse stays observable.
    free: Vec<*mut PyxObj>,
    /// Allocated cells not released yet (`objects.len()` plus objects that
    /// were allocated during a sweep and are still registered).
    live: usize,
    /// Cumulative count of allocations served from `free`, i.e. from cells
    /// that had been handed out and released before. Cells popped from the
    /// untouched remainder of a fresh slab are new allocations, not reuse.
    reused: usize,
}

impl ObjectPool {
    pub(crate) fn new() -> ObjectPool {
        ObjectPool {
            slabs: Vec::new(),
            fresh: Vec::new(),
            free: Vec::new(),
            live: 0,
            reused: 0,
        }
    }

    /// A consistent snapshot; only reads, so it may be taken while the pool
    /// is borrowed through its owner.
    pub(crate) fn stats(&self) -> PoolStats {
        PoolStats {
            slabs: self.slabs.len(),
            capacity: self.slabs.len() * SLAB_OBJECTS,
            live: self.live,
            reused: self.reused,
        }
    }

    /// Hands out an initialised cell, growing the pool by one slab when the
    /// free list is empty.
    ///
    /// OOM calls [`handle_alloc_error`], which aborts; the allocator never
    /// panics or returns a null pointer.
    pub(crate) fn allocate(&mut self, kind: u32, data: ObjData) -> *mut PyxObj {
        let (cell, recycled) = match self.free.pop() {
            Some(cell) => (cell, true),
            None => match self.fresh.pop() {
                Some(cell) => (cell, false),
                None => (self.grow(), false),
            },
        };
        if recycled {
            self.reused += 1;
        }
        // SAFETY: `cell` is owned by this pool and currently uninitialised
        // (it was either never handed out or its payload was dropped on
        // release). `ptr::write` initialises the whole object without reading
        // the old bytes.
        unsafe {
            ptr::write(
                cell,
                PyxObj {
                    kind,
                    marked: AtomicU8::new(WHITE),
                    data,
                },
            );
        }
        self.live += 1;
        cell
    }

    /// Drops the payload of a cell without reading or freeing the cell again.
    ///
    /// Used by the shared sweep outside every lock. The caller must call
    /// [`ObjectPool::reclaim`] afterwards, exactly once.
    ///
    /// # Safety
    ///
    /// `cell` must be a live allocation of this pool that has not been
    /// dropped or reclaimed yet.
    pub(crate) unsafe fn drop_object(cell: *mut PyxObj) {
        debug_assert!(!cell.is_null(), "null pool cell");
        // SAFETY: guaranteed by the caller; dropping only releases Rust-owned
        // payloads and never touches other PyX objects.
        unsafe { ptr::drop_in_place(cell) };
    }

    /// Returns an already-dropped cell to the free list.
    ///
    /// This never dereferences the cell: the pool only remembers the address,
    /// so it works after the payload was dropped outside the heap lock.
    pub(crate) fn reclaim(&mut self, cell: *mut PyxObj) {
        debug_assert!(!cell.is_null(), "null pool cell");
        // Defensive: a duplicate release would otherwise underflow the
        // counter; the free list would still catch the double-push as a
        // double allocation later, so keep the debug build loud instead.
        debug_assert!(self.live > 0, "reclaim without a live allocation");
        if self.live > 0 {
            self.live -= 1;
        }
        self.free.push(cell);
    }

    /// Drops the payload and returns the cell to the free list.
    ///
    /// # Safety
    ///
    /// `cell` must be a live allocation of this pool and must not be released
    /// a second time.
    pub(crate) unsafe fn release(&mut self, cell: *mut PyxObj) {
        // SAFETY: forwarded from the caller.
        unsafe { Self::drop_object(cell) };
        self.reclaim(cell);
    }

    /// Deallocates every slab and clears the free list. All cells must have
    /// been released; the payload of any still-live cell is leaked (its slab
    /// memory is freed, so it must never be dereferenced again).
    pub(crate) fn clear(&mut self) {
        self.fresh.clear();
        self.free.clear();
        for slab in self.slabs.drain(..) {
            // SAFETY: `slab.ptr` was allocated by `grow` with exactly
            // `slab.layout`, and the pool no longer hands out any of its
            // cells after this point.
            unsafe { dealloc(slab.ptr as *mut u8, slab.layout) };
        }
        self.live = 0;
        // `reused` stays monotonic: it is a lifetime counter, not a live
        // measurement.
    }

    /// Takes ownership of another pool's slabs, free cells and counters.
    ///
    /// Used when the process switches to the shared heap: the migrated
    /// objects stay valid because the shared pool now owns the slabs that
    /// contain them, and the thread-local pool must never free that memory.
    pub(crate) fn absorb(&mut self, other: &mut ObjectPool) {
        self.slabs.append(&mut other.slabs);
        self.fresh.append(&mut other.fresh);
        self.free.append(&mut other.free);
        self.live += other.live;
        self.reused += other.reused;
        other.live = 0;
        other.reused = 0;
    }

    /// Allocates one slab and queues every cell but the first on the free
    /// list; the first cell is returned for the caller's object.
    fn grow(&mut self) -> *mut PyxObj {
        // `Layout::array` only fails on arithmetic overflow, which cannot
        // happen for the fixed-size `PyxObj`; the message is defensive.
        let layout = Layout::array::<PyxObj>(SLAB_OBJECTS)
            .expect("PyxObj slab layout does not fit in usize");
        // SAFETY: `layout` has a non-zero size (SLAB_OBJECTS > 0).
        let base = unsafe { alloc(layout) } as *mut PyxObj;
        if base.is_null() {
            // Aborts by contract; never returns to the collector.
            handle_alloc_error(layout);
        }
        debug_assert_eq!(
            base as usize % std::mem::align_of::<PyxObj>(),
            0,
            "slab base is not aligned for PyxObj"
        );
        self.slabs.push(Slab { ptr: base, layout });
        // Push 1..SLAB_OBJECTS; the order is irrelevant because all cells are
        // indistinguishable until they are initialised.
        for index in 1..SLAB_OBJECTS {
            // SAFETY: `index < SLAB_OBJECTS` and the slab holds exactly that
            // many `PyxObj` cells.
            self.fresh.push(unsafe { base.add(index) });
        }
        base
    }
}

impl Drop for ObjectPool {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const KIND_TEST: u32 = 0;

    fn string_data(value: &str) -> ObjData {
        ObjData::Str(value.to_owned())
    }

    #[test]
    fn allocate_initialises_the_cell_and_alignment() {
        let mut pool = ObjectPool::new();
        let cell = pool.allocate(KIND_TEST, string_data("hello"));

        assert!(!cell.is_null());
        assert_eq!(
            cell as usize % std::mem::align_of::<PyxObj>(),
            0,
            "pool cell is not aligned for PyxObj"
        );
        assert_eq!(
            cell as usize % 8,
            0,
            "collector requires 8-byte aligned objects"
        );
        // SAFETY: `cell` is a live allocation of `pool`.
        unsafe {
            assert_eq!((*cell).kind, KIND_TEST);
            assert_eq!(
                (*cell).marked.load(std::sync::atomic::Ordering::Relaxed),
                WHITE
            );
            match &(*cell).data {
                ObjData::Str(text) => assert_eq!(text, "hello"),
                _ => panic!("wrong payload written"),
            }
        }
        assert_eq!(pool.stats().live, 1);
        // SAFETY: `cell` is live and released exactly once before `Drop`.
        unsafe { pool.release(cell) };
    }

    #[test]
    fn release_then_allocate_reuses_the_same_cell() {
        let mut pool = ObjectPool::new();
        let first = pool.allocate(KIND_TEST, string_data("first"));
        // SAFETY: `first` was allocated above and is released exactly once.
        unsafe { pool.release(first) };

        assert_eq!(pool.stats().live, 0);
        assert_eq!(pool.stats().slabs, 1);

        let second = pool.allocate(KIND_TEST, string_data("second"));
        assert_eq!(second, first, "free list did not recycle the cell");
        let stats = pool.stats();
        assert_eq!(stats.reused, 1, "reuse was not accounted");
        assert_eq!(stats.live, 1);
        assert_eq!(stats.slabs, 1, "a free cell must not grow the pool");
        // SAFETY: `second` is live.
        unsafe {
            match &(*second).data {
                ObjData::Str(text) => assert_eq!(text, "second"),
                _ => panic!("recycled cell kept its old payload"),
            }
            // SAFETY: `second` is live and released exactly once.
            pool.release(second);
        }
    }

    #[test]
    fn stats_are_monotonic_and_reuse_is_counted() {
        let mut pool = ObjectPool::new();
        let count = SLAB_OBJECTS * 3 + 1;
        let mut cells = Vec::with_capacity(count);
        for index in 0..count {
            cells.push(pool.allocate(KIND_TEST, string_data(&index.to_string())));
        }

        let unique: HashSet<usize> = cells.iter().map(|&cell| cell as usize).collect();
        assert_eq!(unique.len(), count, "pool handed out a duplicate cell");

        let stats = pool.stats();
        assert_eq!(stats.slabs, 4);
        assert_eq!(stats.capacity, 4 * SLAB_OBJECTS);
        assert_eq!(stats.live, count);
        assert_eq!(stats.reused, 0, "fresh slabs were counted as reuse");

        for &cell in &cells {
            // SAFETY: every `cell` is live and released exactly once.
            unsafe { pool.release(cell) };
        }
        let stats = pool.stats();
        assert_eq!(stats.live, 0);
        assert_eq!(stats.slabs, 4, "release must keep slabs for reuse");
        assert_eq!(stats.capacity, 4 * SLAB_OBJECTS);
        assert_eq!(stats.reused, 0);

        let mut reused = Vec::with_capacity(count);
        for _ in 0..count {
            reused.push(pool.allocate(KIND_TEST, string_data("reused")));
        }
        let stats = pool.stats();
        assert_eq!(stats.reused, count);
        assert_eq!(stats.live, count);
        assert_eq!(stats.slabs, 4, "free list was exhausted too early");

        // LIFO free list: the last released cell is handed out first.
        for index in 0..count {
            assert_eq!(reused[index], cells[count - 1 - index]);
        }

        for &cell in &reused {
            // SAFETY: every `cell` is live and released exactly once.
            unsafe { pool.release(cell) };
        }
        pool.clear();
        let stats = pool.stats();
        assert_eq!(stats.slabs, 0);
        assert_eq!(stats.capacity, 0);
        assert_eq!(stats.live, 0);
        assert_eq!(
            stats.reused, count,
            "clear must not reset the lifetime count"
        );
    }

    #[test]
    fn clear_frees_slabs_and_reallocation_grows_again() {
        let mut pool = ObjectPool::new();
        let cell = pool.allocate(KIND_TEST, string_data("value"));
        // SAFETY: released exactly once before `clear`.
        unsafe { pool.release(cell) };
        pool.clear();

        assert_eq!(pool.stats(), PoolStats::default());
        let fresh = pool.allocate(KIND_TEST, string_data("fresh"));
        let stats = pool.stats();
        assert_eq!(stats.slabs, 1);
        assert_eq!(stats.live, 1);
        assert_eq!(stats.reused, 0, "cleared free list must not report reuse");
        // SAFETY: `fresh` is live and released exactly once before `Drop`.
        unsafe { pool.release(fresh) };
    }

    #[test]
    fn absorb_transfers_slab_ownership_and_free_cells() {
        let mut source = ObjectPool::new();
        let live = source.allocate(KIND_TEST, string_data("live"));
        let freed = source.allocate(KIND_TEST, string_data("freed"));
        // SAFETY: `freed` is live and released exactly once.
        unsafe { source.release(freed) };

        let mut target = ObjectPool::new();
        let own = target.allocate(KIND_TEST, string_data("own"));
        target.absorb(&mut source);

        // The source no longer owns anything and must not free the slab.
        assert_eq!(source.stats(), PoolStats::default());

        let stats = target.stats();
        assert_eq!(stats.slabs, 2);
        assert_eq!(stats.capacity, 2 * SLAB_OBJECTS);
        assert_eq!(stats.live, 2);

        // A cell allocated by the source can be released through the target.
        // SAFETY: `live` is still an outstanding allocation, now owned here.
        unsafe { target.release(live) };
        assert_eq!(target.stats().live, 1);

        // A cell allocated by the source, released through the target, is at
        // the top of the merged free list and is recycled next.
        let recycled = target.allocate(KIND_TEST, string_data("recycled"));
        assert_eq!(recycled, live);
        assert_eq!(target.stats().reused, 1);

        // SAFETY: `own` and `recycled` are live; release them so `clear` in
        // `Drop` does not leak payloads.
        unsafe {
            target.release(own);
            target.release(recycled);
        }
    }
}
