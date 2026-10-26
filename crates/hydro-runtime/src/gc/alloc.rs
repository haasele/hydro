//! Size-class slab allocator for collector objects.
//!
//! Every object the collector owns has two allocations from the same pool:
//!
//! * a fixed 16-byte [`HydroObj`] header holding `kind`, the collector colour,
//!   the payload size class and the payload pointer;
//! * a payload block sized for the concrete payload type selected by `kind`
//!   (`String`, `f64`, `Vec<i64>`, `DictData`, `Box<FiberState>`, ...).
//!
//! The payload block is rounded up to one of [`SIZE_CLASSES`]; payloads
//! larger than the largest class (or needing an alignment above 16) fall back
//! to an exact system allocation and use class [`LARGE_CLASS`]. Payload
//! alignment is at least 8 bytes and every small class is laid out on a
//! 16-byte grid, so `align_of::<T>()` is always satisfied.
//!
//! Each class owns slabs allocated with one system layout and keeps a
//! `fresh` list (untouched cells of a slab) and a `free` list (cells handed
//! out and released again). Allocations prefer `free`, so a sweep recycles
//! its cells LIFO before the untouched reserve grows. Header cells use their
//! own independent slab class.
//!
//! # Ownership
//!
//! * [`ObjectPool::allocate`] takes a payload block, writes the concrete
//!   payload, takes a header and initialises it.
//! * [`ObjectPool::drop_object`] drops the payload in place (outside the heap
//!   lock in the shared collector path).
//! * [`ObjectPool::reclaim`] returns the payload block to its class free list
//!   and the header to the header free list. It must be called exactly once
//!   after `drop_object`.
//! * [`ObjectPool::release`] is the combined single-threaded path.
//! * `Drop` and [`ObjectPool::clear`] deallocate every slab; the caller must
//!   have released every cell, otherwise only the Rust payload is leaked (the
//!   slab memory itself is still freed).
//!
//! The pool is not synchronised; the owning [`super::HeapState`] serialises
//! access (a thread-local heap before `_thread` is used, the `GLOBAL` mutex
//! afterwards).

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::ptr;
use std::sync::atomic::AtomicU8;

use crate::gc::heap::WHITE;
use crate::value::{drop_payload, payload_layout, write_payload, HydroObj, ObjData};

/// Small payload size classes in bytes; index `i` is class id `i + 1`.
///
/// The progression covers common header-sized payloads (8/16/24/32) and
/// grows geometrically so the relative internal fragmentation stays bounded.
pub(crate) const SIZE_CLASSES: [usize; 18] = [
    8, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096,
];

/// Number of small payload classes.
const CLASS_COUNT: usize = SIZE_CLASSES.len();

/// Payload class id used for exact system allocations.
const LARGE_CLASS: u8 = 0;

/// Largest payload served from a size class.
const MAX_CLASS_SIZE: usize = SIZE_CLASSES[CLASS_COUNT - 1];

/// Minimum payload alignment; every class block is also 16-byte aligned.
const MIN_ALIGN: usize = 8;

/// Alignment of every size-class slab. 16 is enough for all payload types
/// (Rust primitives, `Box`, `String`, `Vec`, mutexes and regex state) and
/// keeps every class cell aligned even when the class size is not a multiple
/// of 16 (24, 48, ...).
const CLASS_ALIGN: usize = 16;

/// Target bytes per slab: chunks are sized so tiny classes do not waste a
/// full page per handful of cells and huge classes do not over-reserve.
const SLAB_TARGET_BYTES: usize = 64 * 1024;

/// Bounds for the number of cells in one slab, regardless of class size.
const MIN_SLAB_OBJECTS: usize = 16;
const MAX_SLAB_OBJECTS: usize = 512;

/// Fixed header size; asserted below.
const HEADER_SIZE: usize = std::mem::size_of::<HydroObj>();

const _: () = assert!(HEADER_SIZE == 16, "HydroObj header must stay 16 bytes");
const _: () = assert!(
    HEADER_SIZE.is_multiple_of(MIN_ALIGN),
    "HydroObj header must satisfy the minimum payload alignment"
);

/// Number of cells in a slab for a payload of `size` bytes.
fn slab_objects_for(size: usize) -> usize {
    (SLAB_TARGET_BYTES / size.max(1)).clamp(MIN_SLAB_OBJECTS, MAX_SLAB_OBJECTS)
}

/// The size class serving `layout`, or [`LARGE_CLASS`].
///
/// Selection is deterministic: the first class whose size covers the payload.
/// Over-aligned payloads (> 16) use the exact system fallback so their
/// alignment is never reduced by a slab.
fn class_for(layout: Layout) -> u8 {
    if layout.size() > MAX_CLASS_SIZE || layout.align() > CLASS_ALIGN {
        return LARGE_CLASS;
    }
    for (index, &size) in SIZE_CLASSES.iter().enumerate() {
        if layout.size() <= size {
            return (index + 1) as u8;
        }
    }
    LARGE_CLASS
}

/// Exact layout used by the large fallback (size 0 becomes 1).
fn large_layout(layout: Layout) -> Layout {
    Layout::from_size_align(layout.size().max(1), layout.align().max(MIN_ALIGN))
        .expect("large payload layout does not fit in usize")
}

/// One system allocation holding `slab_objects` contiguous cells of `size`
/// bytes, spaced `stride` (a multiple of [`CLASS_ALIGN`]) apart.
struct Slab {
    ptr: *mut u8,
    layout: Layout,
}

/// A single size class: its slab list and both free lists.
struct ClassPool {
    /// Payload size served by this class (unrounded).
    size: usize,
    /// Distance between two cells in a slab (size rounded up to the class
    /// alignment).
    stride: usize,
    /// Cells per slab.
    slab_objects: usize,
    slabs: Vec<Slab>,
    /// Cells never handed out yet.
    fresh: Vec<*mut u8>,
    /// Cells handed out before and released again; preferred for reuse.
    free: Vec<*mut u8>,
    /// Outstanding cells of this class.
    live: usize,
    /// Allocations served from `free` since the pool was created.
    reused: usize,
}

impl ClassPool {
    fn new(size: usize) -> ClassPool {
        let stride = size
            .max(MIN_ALIGN)
            .div_ceil(CLASS_ALIGN)
            .saturating_mul(CLASS_ALIGN);
        ClassPool {
            size,
            stride,
            slab_objects: slab_objects_for(stride),
            slabs: Vec::new(),
            fresh: Vec::new(),
            free: Vec::new(),
            live: 0,
            reused: 0,
        }
    }

    /// Hands out one uninitialised cell.
    fn take(&mut self) -> *mut u8 {
        let block = match self.free.pop() {
            Some(block) => {
                self.reused += 1;
                block
            }
            None => match self.fresh.pop() {
                Some(block) => block,
                None => self.grow(),
            },
        };
        self.live += 1;
        block
    }

    /// Returns a previously taken cell to the LIFO free list.
    fn give(&mut self, block: *mut u8) {
        debug_assert!(
            self.live > 0,
            "class cell released without a live allocation"
        );
        if self.live > 0 {
            self.live -= 1;
        }
        self.free.push(block);
    }

    /// Allocates one slab and queues every cell but the first for `take`.
    fn grow(&mut self) -> *mut u8 {
        let bytes = self
            .stride
            .checked_mul(self.slab_objects)
            .expect("slab size does not fit in usize");
        let layout =
            Layout::from_size_align(bytes, CLASS_ALIGN).expect("slab layout does not fit in usize");
        // SAFETY: `layout` has a non-zero size (`slab_objects >= 1`).
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            // Aborts by contract; never returns to the collector.
            handle_alloc_error(layout);
        }
        debug_assert_eq!(
            base as usize % CLASS_ALIGN,
            0,
            "slab base is not aligned for size-class payloads"
        );
        self.slabs.push(Slab { ptr: base, layout });
        // Push in reverse so `take` hands out cells in ascending address
        // order; the order is deterministic and keeps tests stable.
        for index in (1..self.slab_objects).rev() {
            // SAFETY: `index < slab_objects` and the slab holds exactly that
            // many `stride`-sized cells.
            self.fresh.push(unsafe { base.add(index * self.stride) });
        }
        base
    }

    /// Deallocates every slab; released cells are forgotten and `live` is
    /// reset. `reused` stays monotonic (lifetime counter).
    fn clear(&mut self) {
        self.fresh.clear();
        self.free.clear();
        for slab in self.slabs.drain(..) {
            // SAFETY: `slab.ptr` was allocated by `grow` with exactly
            // `slab.layout`, and the pool no longer hands out its cells.
            unsafe { dealloc(slab.ptr, slab.layout) };
        }
        self.live = 0;
    }

    /// Takes ownership of another same-class pool's slabs and free lists.
    fn absorb(&mut self, other: &mut ClassPool) {
        debug_assert_eq!(self.size, other.size, "cannot merge different size classes");
        self.slabs.append(&mut other.slabs);
        self.fresh.append(&mut other.fresh);
        self.free.append(&mut other.free);
        self.live += other.live;
        self.reused += other.reused;
        other.live = 0;
        other.reused = 0;
    }

    fn stats(&self) -> ClassStats {
        ClassStats {
            size: self.size,
            slab_objects: self.slab_objects,
            slabs: self.slabs.len(),
            capacity: self.slabs.len() * self.slab_objects,
            live: self.live,
            reused: self.reused,
            bytes: self.slabs.len() * self.slab_objects * self.stride,
        }
    }
}

/// Read-only statistics of one size class.
///
/// The snapshot is a diagnostics surface: the collector prints a subset and
/// the allocator unit tests assert on the remaining fields, so some of them
/// are only read under `cfg(test)`.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClassStats {
    /// Payload size of the class, in bytes.
    pub(crate) size: usize,
    /// Cells per slab.
    pub(crate) slab_objects: usize,
    /// Slabs currently owned by the class.
    pub(crate) slabs: usize,
    /// Cells across all slabs (`slabs * slab_objects`).
    pub(crate) capacity: usize,
    /// Cells handed out and not yet reclaimed.
    pub(crate) live: usize,
    /// Allocations served from the class free list. Cumulative and monotonic;
    /// `clear` does not reset it.
    pub(crate) reused: usize,
    /// Bytes owned by the class's slabs.
    pub(crate) bytes: usize,
}

/// Read-only allocation statistics ([`ObjectPool::stats`]).
///
/// Like [`ClassStats`], the full snapshot is consumed by the collector's
/// diagnostics and by the allocator unit tests; a release build reads only the
/// counters it prints.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PoolStats {
    /// Header slabs plus payload slabs.
    pub(crate) slabs: usize,
    /// Header cells plus payload cells.
    pub(crate) capacity: usize,
    /// Live heap objects (one header each).
    pub(crate) live: usize,
    /// Payload allocations served from a class free list (sum over classes).
    pub(crate) reused: usize,
    /// Bytes owned by all slabs (header and payload).
    pub(crate) bytes: usize,
    /// Bytes currently reserved for live payload blocks (class size, or the
    /// exact size for large allocations).
    pub(crate) payload_bytes: usize,
    /// Live large (system-allocated) payloads.
    pub(crate) large: usize,
    /// Large payload allocations ever made (cumulative until `clear`).
    pub(crate) large_total: usize,
    /// Bytes held by live large payloads.
    pub(crate) large_bytes: usize,
    /// High-water mark of `payload_bytes` since the last `clear`.
    pub(crate) peak_live_bytes: usize,
    /// Statistics of the fixed header class.
    pub(crate) header: ClassStats,
    /// Per-class statistics; index `i` is class id `i + 1`.
    pub(crate) classes: [ClassStats; CLASS_COUNT],
}

impl Default for PoolStats {
    fn default() -> PoolStats {
        PoolStats {
            slabs: 0,
            capacity: 0,
            live: 0,
            reused: 0,
            bytes: 0,
            payload_bytes: 0,
            large: 0,
            large_total: 0,
            large_bytes: 0,
            peak_live_bytes: 0,
            header: ClassStats::default(),
            classes: [ClassStats::default(); CLASS_COUNT],
        }
    }
}

/// Size-class pool of headers and payloads.
pub(crate) struct ObjectPool {
    /// Fixed-size object headers.
    headers: ClassPool,
    /// One slab/free-list set per payload size class.
    classes: [ClassPool; CLASS_COUNT],
    /// Live objects (mirror of `headers.live`).
    live: usize,
    /// Live payload bytes, including large payloads.
    live_bytes: usize,
    /// High-water mark of `live_bytes` since the last `clear`.
    peak_live_bytes: usize,
    /// Live large payloads.
    large_live: usize,
    /// Cumulative large payload count since the last `clear`.
    large_total: usize,
    /// Bytes held by live large payloads.
    large_bytes: usize,
}

impl ObjectPool {
    pub(crate) fn new() -> ObjectPool {
        ObjectPool {
            headers: ClassPool::new(HEADER_SIZE),
            classes: std::array::from_fn(|index| ClassPool::new(SIZE_CLASSES[index])),
            live: 0,
            live_bytes: 0,
            peak_live_bytes: 0,
            large_live: 0,
            large_total: 0,
            large_bytes: 0,
        }
    }

    /// A consistent snapshot; only reads, so it may be taken while the pool
    /// is borrowed through its owner.
    pub(crate) fn stats(&self) -> PoolStats {
        let mut classes = [ClassStats::default(); CLASS_COUNT];
        let mut slabs = 0;
        let mut capacity = 0;
        let mut reused = 0;
        let mut bytes = 0;
        for (slot, pool) in classes.iter_mut().zip(self.classes.iter()) {
            *slot = pool.stats();
            slabs += slot.slabs;
            capacity += slot.capacity;
            reused += slot.reused;
            bytes += slot.bytes;
        }
        let header = self.headers.stats();
        PoolStats {
            slabs: header.slabs + slabs,
            capacity: header.capacity + capacity,
            live: self.live,
            reused,
            bytes: header.bytes + bytes,
            payload_bytes: self.live_bytes,
            large: self.large_live,
            large_total: self.large_total,
            large_bytes: self.large_bytes,
            peak_live_bytes: self.peak_live_bytes,
            header,
            classes,
        }
    }

    /// Hands out an initialised header plus a payload block holding `data`.
    ///
    /// `kind` selects the concrete payload type; the class is derived from
    /// that type, never from the caller, so payload and class cannot diverge.
    ///
    /// OOM calls [`handle_alloc_error`], which aborts; the allocator never
    /// panics or returns a null pointer on valid input.
    pub(crate) fn allocate(&mut self, kind: u32, data: ObjData) -> *mut HydroObj {
        let layout = payload_layout(kind);
        let (payload, class) = self.take_block(layout);
        debug_assert!(
            (payload as usize).is_multiple_of(layout.align().max(MIN_ALIGN)),
            "payload block does not satisfy the payload alignment"
        );
        // SAFETY: `payload` is uninitialised storage for exactly the payload
        // type selected by `kind`; the block is large enough because the
        // class covers `payload_layout(kind)`.
        unsafe { write_payload(kind, data, payload) };

        let header = self.headers.take() as *mut HydroObj;
        debug_assert_eq!(
            header as usize % MIN_ALIGN,
            0,
            "header cell is not aligned for HydroObj"
        );
        // SAFETY: `header` is an uninitialised header cell owned by this
        // pool; `ptr::write` initialises it without reading old bytes.
        unsafe {
            ptr::write(
                header,
                HydroObj {
                    kind,
                    marked: AtomicU8::new(WHITE),
                    class,
                    data: payload,
                },
            );
        }
        self.live += 1;
        header
    }

    /// Drops the payload of a cell without freeing its blocks.
    ///
    /// Used by the shared sweep outside every lock. The caller must call
    /// [`ObjectPool::reclaim`] afterwards, exactly once.
    ///
    /// # Safety
    ///
    /// `cell` must be a live allocation of this pool whose payload has not
    /// been dropped or reclaimed yet.
    pub(crate) unsafe fn drop_object(cell: *mut HydroObj) {
        debug_assert!(!cell.is_null(), "null pool cell");
        // SAFETY: guaranteed by the caller; dropping only releases Rust-owned
        // payloads and never touches other Hydro objects.
        let (kind, payload) = unsafe { ((*cell).kind, (*cell).data) };
        unsafe { drop_payload(kind, payload) };
    }

    /// Returns an already-dropped cell to the class free list.
    ///
    /// This reads only the header (kind, class, payload pointer), so it works
    /// after the payload was dropped outside the heap lock.
    pub(crate) fn reclaim(&mut self, cell: *mut HydroObj) {
        debug_assert!(!cell.is_null(), "null pool cell");
        let (kind, class, payload) = unsafe { ((*cell).kind, (*cell).class, (*cell).data) };
        self.give_block(class, payload, payload_layout(kind));
        self.headers.give(cell as *mut u8);
        // Defensive: a duplicate release would otherwise underflow the
        // counter; the free list would still catch the double-push as a
        // double allocation later, so keep the debug build loud instead.
        debug_assert!(self.live > 0, "reclaim without a live allocation");
        if self.live > 0 {
            self.live -= 1;
        }
        debug_assert_eq!(self.live, self.headers.live);
    }

    /// Drops the payload and returns both blocks to their free lists.
    ///
    /// # Safety
    ///
    /// `cell` must be a live allocation of this pool and must not be released
    /// a second time.
    pub(crate) unsafe fn release(&mut self, cell: *mut HydroObj) {
        // SAFETY: forwarded from the caller.
        unsafe { Self::drop_object(cell) };
        self.reclaim(cell);
    }

    /// Deallocates every slab and clears the free lists. All cells must have
    /// been released; the payload of any still-live cell is leaked (its slab
    /// memory is freed, so it must never be dereferenced again).
    pub(crate) fn clear(&mut self) {
        self.headers.clear();
        for class in &mut self.classes {
            class.clear();
        }
        self.live = 0;
        self.live_bytes = 0;
        self.peak_live_bytes = 0;
        self.large_live = 0;
        self.large_total = 0;
        self.large_bytes = 0;
        // Per-class `reused` stays monotonic: it is a lifetime counter, not a
        // live measurement.
    }

    /// Takes ownership of another pool's slabs, free cells and counters.
    ///
    /// Used when the process switches to the shared heap: the migrated
    /// objects stay valid because the shared pool now owns every slab that
    /// contains them, and the thread-local pool must never free that memory.
    pub(crate) fn absorb(&mut self, other: &mut ObjectPool) {
        self.headers.absorb(&mut other.headers);
        for (target, source) in self.classes.iter_mut().zip(other.classes.iter_mut()) {
            target.absorb(source);
        }
        self.live += other.live;
        self.live_bytes += other.live_bytes;
        self.peak_live_bytes = self.peak_live_bytes.max(other.peak_live_bytes);
        self.large_live += other.large_live;
        self.large_total += other.large_total;
        self.large_bytes += other.large_bytes;

        other.live = 0;
        other.live_bytes = 0;
        other.peak_live_bytes = 0;
        other.large_live = 0;
        other.large_total = 0;
        other.large_bytes = 0;
    }

    /// Takes a payload block and accounts its live bytes.
    fn take_block(&mut self, layout: Layout) -> (*mut u8, u8) {
        let class = class_for(layout);
        let (block, bytes) = if class == LARGE_CLASS {
            let exact = large_layout(layout);
            // SAFETY: `exact` has a non-zero size.
            let block = unsafe { alloc(exact) };
            if block.is_null() {
                handle_alloc_error(exact);
            }
            self.large_live += 1;
            self.large_total += 1;
            self.large_bytes += exact.size();
            (block, exact.size())
        } else {
            let index = usize::from(class - 1);
            let pool = &mut self.classes[index];
            let bytes = pool.size;
            (pool.take(), bytes)
        };
        self.live_bytes += bytes;
        self.peak_live_bytes = self.peak_live_bytes.max(self.live_bytes);
        (block, class)
    }

    /// Returns a payload block to its class free list or the system.
    fn give_block(&mut self, class: u8, block: *mut u8, layout: Layout) {
        if class == LARGE_CLASS {
            let exact = large_layout(layout);
            // SAFETY: the block was allocated by `take_block` with exactly
            // this layout.
            unsafe { dealloc(block, exact) };
            self.large_live = self.large_live.saturating_sub(1);
            self.large_bytes = self.large_bytes.saturating_sub(exact.size());
            self.live_bytes = self.live_bytes.saturating_sub(exact.size());
            return;
        }
        let index = usize::from(class - 1);
        debug_assert!(index < CLASS_COUNT, "payload class {class} is out of range");
        let pool = &mut self.classes[index];
        let bytes = pool.size;
        pool.give(block);
        self.live_bytes = self.live_bytes.saturating_sub(bytes);
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
    use std::mem::align_of;

    use super::*;
    use crate::value::{KIND_BIGINT, KIND_FLOAT, KIND_LIST, KIND_RANGE, KIND_STR};

    fn string_data(value: &str) -> ObjData {
        ObjData::Str(value.to_owned())
    }

    /// Class id serving a payload of `size`/`align`.
    fn class_id(size: usize, align: usize) -> u8 {
        class_for(Layout::from_size_align(size, align).unwrap())
    }

    /// Stats of the class serving a payload of `size`/`align`.
    fn class_stats(pool: &ObjectPool, size: usize, align: usize) -> ClassStats {
        let id = class_id(size, align);
        assert_ne!(id, LARGE_CLASS, "expected a size class for {size} bytes");
        pool.stats().classes[usize::from(id - 1)]
    }

    #[test]
    fn every_kind_has_exactly_one_payload_layout() {
        // Kinds are dense: 0 ..= KIND_STR_BUILDER. `payload_layout` panics on
        // an unknown kind, so this walks the whole table and checks the class
        // either covers the payload or deliberately uses the large fallback.
        for kind in 0..=crate::value::KIND_STR_BUILDER {
            let layout = payload_layout(kind);
            assert!(layout.size() > 0, "kind {kind} has an empty payload");
            assert!(layout.align() >= 1);
            let class = class_for(layout);
            if class == LARGE_CLASS {
                assert!(
                    layout.size() > MAX_CLASS_SIZE || layout.align() > CLASS_ALIGN,
                    "kind {kind} was needlessly routed to the large fallback"
                );
            } else {
                assert!(
                    SIZE_CLASSES[usize::from(class - 1)] >= layout.size(),
                    "class {class} is too small for kind {kind}"
                );
            }
        }
    }

    #[test]
    fn size_class_selection_is_deterministic() {
        assert_eq!(class_id(1, 1), 1);
        assert_eq!(class_id(8, 8), 1);
        assert_eq!(class_id(9, 8), 2);
        assert_eq!(class_id(24, 8), 3);
        assert_eq!(class_id(25, 8), 4);
        assert_eq!(class_id(4096, 8), CLASS_COUNT as u8);
        assert_eq!(class_id(4097, 8), LARGE_CLASS);
        assert_eq!(
            class_id(64, 32),
            LARGE_CLASS,
            "over-aligned payloads use the exact fallback"
        );
        assert_eq!(
            class_id(0, 8),
            1,
            "empty payloads still get a minimum block"
        );

        // The table itself is strictly increasing and a multiple of the
        // minimum alignment.
        for window in SIZE_CLASSES.windows(2) {
            assert!(window[0] < window[1]);
        }
        for &size in &SIZE_CLASSES {
            assert_eq!(size % MIN_ALIGN, 0);
        }
    }

    #[test]
    fn kinds_map_to_the_expected_class_and_alignment() {
        let mut pool = ObjectPool::new();
        let float = pool.allocate(KIND_FLOAT, ObjData::Float(1.5));
        let string = pool.allocate(KIND_STR, string_data("hello"));
        let list = pool.allocate(KIND_LIST, ObjData::List(vec![1, 2, 3]));
        let bigint = pool.allocate(KIND_BIGINT, ObjData::BigInt(1.into()));
        let range = pool.allocate(
            KIND_RANGE,
            ObjData::Range {
                start: 0,
                stop: 3,
                step: 1,
            },
        );

        // SAFETY: all five cells are live allocations of `pool`.
        unsafe {
            assert_eq!(
                (*float).class,
                class_id(std::mem::size_of::<f64>(), align_of::<f64>())
            );
            assert_eq!(
                (*string).class,
                class_id(std::mem::size_of::<String>(), align_of::<String>())
            );
            assert_eq!(
                (*list).class,
                class_id(std::mem::size_of::<Vec<i64>>(), align_of::<Vec<i64>>())
            );
            assert_eq!(
                (*bigint).class,
                class_id(
                    std::mem::size_of::<num_bigint::BigInt>(),
                    align_of::<num_bigint::BigInt>()
                )
            );
            assert_eq!(
                (*range).class,
                class_id(
                    std::mem::size_of::<crate::value::RangeData>(),
                    align_of::<crate::value::RangeData>()
                )
            );
            for cell in [float, string, list, bigint, range] {
                assert_eq!((*cell).data as usize % MIN_ALIGN, 0);
                assert_eq!((*cell).data as usize % 8, 0);
            }
            assert_eq!((*float).data as usize % align_of::<f64>(), 0);
            assert_eq!((*string).data as usize % align_of::<String>(), 0);
            assert_eq!(
                (*range).data as usize % align_of::<crate::value::RangeData>(),
                0
            );
        }

        // Exhaustive payload reads through the safe accessor.
        // SAFETY: the cells are live.
        unsafe {
            assert!(matches!(
                (*float).data_ref(),
                crate::value::ObjDataRef::Float(value) if value == 1.5
            ));
            assert!(
                matches!((*string).data_ref(), crate::value::ObjDataRef::Str(s) if s.as_str() == "hello")
            );
            assert!(
                matches!((*list).data_ref(), crate::value::ObjDataRef::List(items) if items.as_slice() == [1, 2, 3])
            );
        }
        // SAFETY: every cell is live and released exactly once before `Drop`.
        unsafe {
            for cell in [float, string, list, bigint, range] {
                pool.release(cell);
            }
        }
    }

    #[test]
    fn payloads_and_headers_are_class_allocated_and_reclaimed() {
        let mut pool = ObjectPool::new();
        let first = pool.allocate(KIND_STR, string_data("first"));
        let second = pool.allocate(KIND_FLOAT, ObjData::Float(2.0));

        let stats = pool.stats();
        assert_eq!(stats.live, 2);
        assert_eq!(stats.header.live, 2);
        assert!(stats.slabs > 0);
        assert!(stats.bytes > 0);
        assert!(stats.payload_bytes >= std::mem::size_of::<String>() + std::mem::size_of::<f64>());
        assert_eq!(
            stats.peak_live_bytes, stats.payload_bytes,
            "peak must track the first high-water mark"
        );

        // SAFETY: both cells are live and released exactly once.
        unsafe {
            pool.release(first);
            pool.release(second);
        }
        let stats = pool.stats();
        assert_eq!(stats.live, 0);
        assert_eq!(stats.header.live, 0);
        assert_eq!(stats.payload_bytes, 0);
        assert!(stats.peak_live_bytes > 0);
        for class in stats.classes {
            assert_eq!(class.live, 0);
        }
        assert!(stats.slabs > 0, "release must keep slabs for reuse");
    }

    #[test]
    fn free_lists_recycle_lifo_within_a_class() {
        let mut pool = ObjectPool::new();
        let first = pool.allocate(KIND_STR, string_data("first"));
        let payload = unsafe { (*first).data };
        let header = first;
        // SAFETY: `first` is live and released exactly once.
        unsafe { pool.release(first) };

        assert_eq!(pool.stats().live, 0);
        let second = pool.allocate(KIND_STR, string_data("second"));
        assert_eq!(second, header, "header free list is not LIFO");
        // SAFETY: `second` is live.
        unsafe {
            assert_eq!((*second).data, payload, "payload free list is not LIFO");
            match (*second).data_ref() {
                crate::value::ObjDataRef::Str(text) => assert_eq!(text.as_str(), "second"),
                _ => panic!("recycled payload kept its old value"),
            }
        }
        let stats = pool.stats();
        assert_eq!(stats.live, 1);
        assert_eq!(stats.header.reused, 1);
        assert_eq!(
            class_stats(&pool, std::mem::size_of::<String>(), align_of::<String>()).reused,
            1
        );
        // SAFETY: `second` is live and released exactly once.
        unsafe { pool.release(second) };
    }

    #[test]
    fn different_payload_sizes_have_separate_classes_and_free_lists() {
        let mut pool = ObjectPool::new();
        let float = pool.allocate(KIND_FLOAT, ObjData::Float(1.0));
        let string = pool.allocate(KIND_STR, string_data("text"));
        let float_class = unsafe { (*float).class };
        let string_class = unsafe { (*string).class };
        assert_ne!(
            float_class, string_class,
            "f64 and String must not share a class"
        );

        let string_payload = unsafe { (*string).data };
        // Release only the larger payload: the next string must reuse it and
        // must not touch the f64 class free list.
        // SAFETY: `string` is live and released exactly once.
        unsafe { pool.release(string) };
        let float_stats = pool.stats().classes[usize::from(float_class - 1)];
        let string_stats = pool.stats().classes[usize::from(string_class - 1)];
        assert_eq!(float_stats.live, 1);
        assert_eq!(string_stats.live, 0);
        assert_eq!(string_stats.reused, 0);
        assert_eq!(float_stats.reused, 0);

        let recycled = pool.allocate(KIND_STR, string_data("again"));
        // SAFETY: `recycled` is live.
        unsafe {
            assert_eq!((*recycled).data, string_payload);
            assert_eq!((*recycled).class, string_class);
        }
        let stats = pool.stats();
        assert_eq!(stats.classes[usize::from(string_class - 1)].reused, 1);
        assert_eq!(stats.classes[usize::from(float_class - 1)].reused, 0);

        // SAFETY: both cells are live and released exactly once.
        unsafe {
            pool.release(float);
            pool.release(recycled);
        }
    }

    #[test]
    fn large_payloads_use_the_exact_system_fallback() {
        let mut pool = ObjectPool::new();
        let layout = Layout::from_size_align(MAX_CLASS_SIZE + 1, 16).unwrap();
        let (block, class) = pool.take_block(layout);
        assert_eq!(class, LARGE_CLASS);
        assert_eq!(block as usize % 16, 0);
        // The block is writable for its full size.
        // SAFETY: the block is a fresh large allocation of `layout`.
        unsafe { block.write_bytes(0xA5, MAX_CLASS_SIZE + 1) };

        let stats = pool.stats();
        assert_eq!(stats.large, 1);
        assert_eq!(stats.large_total, 1);
        assert_eq!(stats.large_bytes, MAX_CLASS_SIZE + 1);
        assert_eq!(stats.payload_bytes, MAX_CLASS_SIZE + 1);
        assert_eq!(stats.peak_live_bytes, MAX_CLASS_SIZE + 1);

        pool.give_block(class, block, layout);
        let stats = pool.stats();
        assert_eq!(stats.large, 0);
        assert_eq!(stats.large_total, 1);
        assert_eq!(stats.large_bytes, 0);
        assert_eq!(stats.payload_bytes, 0);
    }

    #[test]
    fn clear_resets_live_statistics_across_all_classes() {
        let mut pool = ObjectPool::new();
        let mut cells = Vec::new();
        for index in 0..64 {
            // Alternate tiny and medium payloads so several classes are live.
            if index % 2 == 0 {
                cells.push(pool.allocate(KIND_FLOAT, ObjData::Float(index as f64)));
            } else {
                cells.push(pool.allocate(KIND_STR, ObjData::Str(format!("value-{index}"))));
            }
        }
        let before = pool.stats();
        assert!(before.classes.iter().filter(|class| class.live > 0).count() >= 2);

        for &cell in &cells {
            // SAFETY: every cell is live and released exactly once.
            unsafe { pool.release(cell) };
        }
        pool.clear();
        let after = pool.stats();
        assert_eq!(after.slabs, 0);
        assert_eq!(after.capacity, 0);
        assert_eq!(after.live, 0);
        assert_eq!(after.payload_bytes, 0);
        assert_eq!(after.peak_live_bytes, 0);
        assert_eq!(after.large, 0);
        assert_eq!(after.large_total, 0);
        for class in after.classes {
            assert_eq!(class.slabs, 0);
            assert_eq!(class.capacity, 0);
            assert_eq!(class.live, 0);
            assert_eq!(class.bytes, 0);
        }

        // Reallocation grows fresh slabs again and does not report reuse from
        // the cleared free lists.
        let fresh = pool.allocate(KIND_STR, string_data("fresh"));
        assert_eq!(pool.stats().reused, 0);
        // SAFETY: `fresh` is live and released exactly once before `Drop`.
        unsafe { pool.release(fresh) };
    }

    #[test]
    fn absorb_transfers_every_class_and_free_cell() {
        let mut source = ObjectPool::new();
        let live_string = source.allocate(KIND_STR, string_data("live"));
        let freed_float = source.allocate(KIND_FLOAT, ObjData::Float(9.0));
        let freed_string = source.allocate(KIND_STR, string_data("freed"));
        // SAFETY: both cells are live and released exactly once.
        unsafe {
            source.release(freed_float);
            source.release(freed_string);
        }
        let source_stats = source.stats();

        let mut target = ObjectPool::new();
        let own = target.allocate(KIND_STR, string_data("own"));
        target.absorb(&mut source);

        // The source no longer owns anything and must not free the slabs.
        let source_after = source.stats();
        assert_eq!(source_after.slabs, 0);
        assert_eq!(source_after.capacity, 0);
        assert_eq!(source_after.live, 0);
        assert_eq!(source_after.payload_bytes, 0);
        assert_eq!(source_after.peak_live_bytes, 0);
        assert_eq!(source_after.large, 0);
        assert_eq!(source_after.large_total, 0);
        for class in source_after.classes {
            assert_eq!(class.slabs, 0);
            assert_eq!(class.capacity, 0);
            assert_eq!(class.live, 0);
            assert_eq!(class.bytes, 0);
            assert_eq!(class.reused, 0);
        }
        let stats = target.stats();
        assert_eq!(stats.live, 2, "own + live_string");
        assert_eq!(
            stats.slabs,
            source_stats.slabs + 2,
            "header + two payload classes"
        );
        assert!(stats.capacity >= source_stats.capacity + 2);
        assert_eq!(
            stats.payload_bytes,
            source_stats.payload_bytes + std::mem::size_of::<String>()
        );
        assert_eq!(stats.reused, 0);

        // The source's released string is still at the top of the merged
        // class free list, so the next string recycles its payload/header.
        let source_payload = unsafe { (*freed_string).data };
        let recycled = target.allocate(KIND_STR, string_data("recycled"));
        // SAFETY: `recycled` is live; `freed_string` is not (it was released).
        unsafe {
            assert_eq!((*recycled).data, source_payload);
        }
        assert!(target.stats().reused >= 1);

        // A cell allocated by the source can be released through the target.
        // SAFETY: `live_string` is still an outstanding allocation, now owned here.
        unsafe { target.release(live_string) };
        assert_eq!(target.stats().live, 2, "own + recycled");

        // SAFETY: `own` and `recycled` are live; release them so `clear` in
        // `Drop` does not leak payloads.
        unsafe {
            target.release(own);
            target.release(recycled);
        }
    }

    #[test]
    fn thousands_of_mixed_allocations_do_not_leak_cells() {
        let mut pool = ObjectPool::new();
        let mut headers = HashSet::new();
        let mut cells = Vec::new();
        for index in 0..12_000usize {
            let cell = match index % 3 {
                0 => pool.allocate(KIND_STR, ObjData::Str(format!("item-{index}"))),
                1 => pool.allocate(KIND_LIST, ObjData::List(vec![index as i64; index % 17])),
                _ => pool.allocate(KIND_FLOAT, ObjData::Float(index as f64)),
            };
            assert!(headers.insert(cell as usize), "header handed out twice");
            cells.push(cell);
        }
        let stats = pool.stats();
        assert_eq!(stats.live, 12_000);
        assert!(stats.header.capacity >= 12_000);
        assert_eq!(
            stats.capacity,
            stats.header.capacity + stats.classes.iter().map(|c| c.capacity).sum::<usize>()
        );

        for &cell in &cells {
            // SAFETY: every cell is live and released exactly once.
            unsafe { pool.release(cell) };
        }
        let stats = pool.stats();
        assert_eq!(stats.live, 0);
        assert_eq!(stats.payload_bytes, 0);
        for class in stats.classes {
            assert_eq!(class.live, 0);
        }
        assert!(stats.slabs > 0, "slabs must stay for reuse");
    }
}
