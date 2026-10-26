//! Conservative mark-&-sweep collector; the algorithm is specified in
//! ARCHITECTURE §7. Contract summary:
//!
//! * Every heap object is registered here. A word is treated as a tagged heap
//!   pointer only when its low three bits are `010` *and* the decoded address
//!   is in the registry, so a stale word can keep an object alive but never
//!   dereference an unknown address.
//! * Threads start with a thread-local registry; after
//!   `_thread.start_new_thread` the heap is process-wide ([`enable_shared`])
//!   and mutex-protected. Collections are incremental: a root snapshot and a
//!   final rescan stop the world, marking and sweeping run concurrently with
//!   mutators. Insertion barriers ([`write_barrier_edge`]) publish every new
//!   edge; objects allocated while marking are born grey.
//! * [`safepoint`] runs at function entry, loop backedges and before every
//!   allocation; blocking operations park via [`blocking_enter`]/
//!   [`blocking_leave`]. There is no GIL: shared containers still need user
//!   locks.
//! * Lock order: `GLOBAL` (registry) before the heap mutex, never after.
//!   `GLOBAL` is only held after all threads are parked or briefly during the
//!   sweep partition, never across a payload drop. `GRAPH_LOCK` serialises
//!   container mutation against marking; `WORKLIST` is never held while
//!   acquiring `GRAPH_LOCK`, a queue mutex or `GLOBAL`. `GC_LOCK` is only
//!   taken with `try_lock`, so a second collector skips. No guard is held
//!   across a safepoint or an allocation.
//!
//! Documented limits:
//!
//! * Container mutation is serialised against the marker by `GRAPH_LOCK`; a
//!   long mutation delays marking but cannot block the collector forever.
//! * An iterator's `index` is not traced, only its immutable `target`, so a
//!   partially consumed iterator keeps its whole target alive.
//! * A registered thread whose saved context cannot be bounded abandons the
//!   collection (objects are leaked) rather than scanning unsafely.
//!
//! `HydroObj` is a fixed 16-byte header and every object owns a separately
//! allocated payload, so storage is the size-class slab pool in [`alloc`]:
//! payloads are rounded to a class (8..4096 bytes, or an exact large
//! allocation), headers use their own slab class, and released cells return
//! to per-class free lists instead of the system allocator.
//!
//! # Weak references
//!
//! A weak reference (`_weakref.ReferenceType`) is an ordinary class instance
//! whose target lives in the process-wide `WeakState` side table, keyed by the
//! reference's cell address. The table stores no tagged strong edge, so
//! tracing a reference never reaches its target: a reference that is itself
//! reachable cannot keep its target alive. At the end of marking (with the
//! world stopped in shared mode, before the sweep in local mode) entries whose
//! reference object did not survive are dropped and entries whose target did
//! not survive are cleared. A resolving call during marking marks its target
//! grey, because the caller then holds a strong reference. The weak lock is
//! ordered before the `GLOBAL` heap mutex and never held across user code or a
//! payload drop.
//!
//! Armed callbacks are strong edges of the reference object (traced by the
//! instance trace and mirrored in `__callback__`). When a target is cleared,
//! the callback is taken out of the entry exactly once, its reference is
//! pinned and, if the reference would otherwise die, marked grey, and the pair
//! is queued. [`run_pending_weakref_callbacks`] invokes queued callbacks with
//! no lock held, from a generated-code safepoint, `gc.collect()`, or thread
//! exit; a raising callback is reported to stderr and its pending exception is
//! cleared. References stay pinned until their callback has run.
//!
//! `getweakrefs` pins the references it materialises into a list: the Rust
//! vector holding them is invisible to the conservative stack scan, so every
//! collection also marks the pinned references until the module releases the
//! pins (`weakrefs_unpin`).
//! # Precise shadow-stack roots
//!
//! Generated code registers its live locals with [`crate::gc::roots`]. At
//! every safepoint slow path and every [`blocking_enter`] the mutator
//! publishes its active frames/ranges, so the collector marks exactly those
//! slots (validated through the address index) instead of guessing. Two modes
//! are supported, selected by `HYDRO_GC_PRECISE`:
//!
//! * **mixed** (default): precise frames/ranges are marked exactly *and*
//!   their slot extents are excluded from the conservative stack/register
//!   scan, so a stale precise slot cannot retain garbage. Rust-side frames
//!   (runtime helpers, tests) are still scanned conservatively.
//! * **strict** (`HYDRO_GC_PRECISE=1`): every conservative stack and register
//!   scan is disabled. Only precise frames/ranges, pending/handled/thread
//!   roots, suspended fiber `SavedRoots`, C-API shim roots and persistent
//!   roots keep objects alive. Unregistered native stacks must root their
//!   tagged locals explicitly with [`crate::gc::roots::frame_push`] or a
//!   scope range.
//!
//! Persistent roots (`add_persistent_root`) belong to a thread registration
//! and survive ordinary collections until removed or cleared by [`reset`].
//! Stack words are read with inline assembly because they may be
//! uninitialized; those invariants are documented where they occur.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::exc;
use crate::gc::roots::{self, PublishedRoots};
use crate::value::{FunctionBody, HydroObj, ObjData, ObjDataRef, TAG_HEAP, TAG_MASK};

// The slab allocator is a private implementation detail of this collector. It
// lives in `gc/alloc.rs`; the explicit path keeps the module declaration here
// so `gc` stays a single-module subtree.
#[path = "alloc.rs"]
mod alloc;

use self::alloc::ObjectPool;

/// Default collection threshold (ARCHITECTURE §7).
const DEFAULT_THRESHOLD: usize = 10_000;

/// Point-in-time snapshot of the collector's cumulative statistics.
///
/// Counters are process-wide and cover both the local (single-threaded) and
/// the shared collection path. Durations are microseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct GcStats {
    /// Completed collections since the last [`reset_gc_stats`].
    pub collections: u64,
    /// Sum of the stop-the-world pauses of those collections.
    pub pause_total_us: u64,
    /// Longest single stop-the-world pause.
    pub pause_max_us: u64,
    /// Sum of the marking phases.
    pub mark_total_us: u64,
    /// Sum of the sweeping phases.
    pub sweep_total_us: u64,
    /// Objects freed by those collections.
    pub freed_total: u64,
    /// Objects registered at snapshot time.
    pub live_objects: u64,
    /// Effective `HYDRO_GC_THRESHOLD` (or the built-in default).
    pub threshold: u64,
    /// Whether `HYDRO_GC_STRESS=1` is active.
    pub stress: bool,
    /// Whether `HYDRO_GC_PRECISE=1` (strict precise-root mode) is active.
    ///
    /// Mixed mode (the default) also marks precise frames exactly, but keeps
    /// the conservative stack/register fallback for runtime and test frames.
    pub precise: bool,
    /// Precise frames marked by the most recent collection.
    pub precise_frames: u64,
}

// Relaxed counters: they only feed a diagnostic snapshot and never
// synchronise heap state, so a torn cross-thread view is acceptable and
// cheap. Increments happen exactly once per completed collection.
static STAT_COLLECTIONS: AtomicU64 = AtomicU64::new(0);
static STAT_PAUSE_TOTAL_US: AtomicU64 = AtomicU64::new(0);
static STAT_PAUSE_MAX_US: AtomicU64 = AtomicU64::new(0);
static STAT_MARK_TOTAL_US: AtomicU64 = AtomicU64::new(0);
static STAT_SWEEP_TOTAL_US: AtomicU64 = AtomicU64::new(0);
static STAT_FREED_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Precise frames marked by the most recent collection (diagnostic only).
static STAT_PRECISE_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Heap generation, bumped by every [`reset`].
///
/// Native modules cache class values (and other heap objects) per thread.
/// After a reset the old heap is gone, so a cache entry from a previous
/// generation must never be reused. The counter is the cheap invalidation
/// token those caches compare against.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Returns the current heap generation. See [`GENERATION`].
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Saturating conversion of a measured phase into whole microseconds.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// Records one completed collection. `pause` is the stop-the-world time
/// (local mode: the whole collection), `mark`/`sweep` are the phase times.
fn record_collection(pause: Duration, mark: Duration, sweep: Duration, freed: usize) {
    let pause_us = micros(pause);
    STAT_COLLECTIONS.fetch_add(1, Ordering::Relaxed);
    STAT_PAUSE_TOTAL_US.fetch_add(pause_us, Ordering::Relaxed);
    STAT_PAUSE_MAX_US.fetch_max(pause_us, Ordering::Relaxed);
    STAT_MARK_TOTAL_US.fetch_add(micros(mark), Ordering::Relaxed);
    STAT_SWEEP_TOTAL_US.fetch_add(micros(sweep), Ordering::Relaxed);
    STAT_FREED_TOTAL.fetch_add(u64::try_from(freed).unwrap_or(u64::MAX), Ordering::Relaxed);
}

/// Returns a snapshot of the collector statistics.
///
/// `live_objects` is read under the heap lock (shared mode) or from the
/// thread-local registry, exactly like `objects()`. `threshold` and `stress`
/// reflect the environment configuration in effect at snapshot time.
pub fn gc_stats() -> GcStats {
    let (threshold, stress) = read_config();
    GcStats {
        collections: STAT_COLLECTIONS.load(Ordering::Relaxed),
        pause_total_us: STAT_PAUSE_TOTAL_US.load(Ordering::Relaxed),
        pause_max_us: STAT_PAUSE_MAX_US.load(Ordering::Relaxed),
        mark_total_us: STAT_MARK_TOTAL_US.load(Ordering::Relaxed),
        sweep_total_us: STAT_SWEEP_TOTAL_US.load(Ordering::Relaxed),
        freed_total: STAT_FREED_TOTAL.load(Ordering::Relaxed),
        live_objects: objects() as u64,
        threshold: threshold as u64,
        stress,
        precise: precise_mode(),
        precise_frames: STAT_PRECISE_FRAMES.load(Ordering::Relaxed),
    }
}

/// Clears the cumulative counters without touching the heap itself.
pub fn reset_gc_stats() {
    STAT_COLLECTIONS.store(0, Ordering::Relaxed);
    STAT_PAUSE_TOTAL_US.store(0, Ordering::Relaxed);
    STAT_PAUSE_MAX_US.store(0, Ordering::Relaxed);
    STAT_MARK_TOTAL_US.store(0, Ordering::Relaxed);
    STAT_SWEEP_TOTAL_US.store(0, Ordering::Relaxed);
    STAT_FREED_TOTAL.store(0, Ordering::Relaxed);
}

/// Finalises a 64-bit value with the SplitMix64 mixers: two xor-shift
/// multiplies followed by a xor-shift, so every input bit affects every
/// output bit.
#[inline]
fn mix64(value: u64) -> u64 {
    let value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Fast non-cryptographic hasher for the GC's internal address index.
///
/// The index is keyed only by aligned heap addresses that the runtime handed
/// out itself (never by user-controlled bytes), so hash flooding and
/// collision attacks are not part of the threat model: a seeded SipHash buys
/// nothing here. Allocation-time `insert` and conservative stack-scan
/// `contains` are hot, so the index uses this Fx-style accumulator plus a
/// SplitMix64 finaliser instead of `RandomState`. This changes only the hash
/// function; every lookup still compares whole `usize` keys, so membership is
/// identical to a std `HashSet<usize>`.
#[derive(Default)]
struct AddressHasher(u64);

impl AddressHasher {
    #[inline]
    fn mix(&mut self, value: u64) {
        self.0 = mix64(self.0 ^ value);
    }
}

impl Hasher for AddressHasher {
    #[inline]
    fn finish(&self) -> u64 {
        mix64(self.0)
    }

    /// Byte-oriented fallback for types this index never stores. Each chunk
    /// is folded into the same accumulator, so equal byte strings still hash
    /// equally.
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.mix(u64::from_ne_bytes(word));
        }
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.mix(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.mix(value as u64);
    }
}

/// Address index of the registered heap objects.
type AddressSet = HashSet<usize, BuildHasherDefault<AddressHasher>>;

struct HeapState {
    /// Every live heap object, in allocation order.
    objects: Vec<*mut HydroObj>,
    /// Addresses of the objects in `objects`, for O(1) conservative lookups.
    addresses: AddressSet,
    /// Backing storage for `objects`. Every registered pointer was handed out
    /// by this pool and must be released through it exactly once.
    pool: ObjectPool,
    /// Collect when the registry has grown by more than `threshold` objects
    /// since the last collection. Using growth instead of an absolute size is
    /// essential for a large live set: otherwise every allocation after a
    /// collection would trigger another full mark of the same live graph.
    threshold: usize,
    /// Number of live objects observed at the end of the last collection.
    last_collection_objects: usize,
    /// Collect before every allocation (`HYDRO_GC_STRESS=1`).
    stress: bool,
}

impl HeapState {
    fn new() -> HeapState {
        let (threshold, stress) = read_config();
        HeapState {
            objects: Vec::new(),
            addresses: AddressSet::default(),
            pool: ObjectPool::new(),
            threshold,
            last_collection_objects: 0,
            stress,
        }
    }

    /// Re-applies the environment configuration after a reset.
    fn refresh_config(&mut self) {
        let (threshold, stress) = read_config();
        self.threshold = threshold;
        self.stress = stress;
    }

    /// Whether a collection is due. Growth since the last collection keeps
    /// the trigger meaningful when the live set itself is larger than the
    /// configured threshold; `HYDRO_GC_STRESS` still collects every time.
    fn collection_due(&self) -> bool {
        self.stress
            || self.objects.len() > self.last_collection_objects.saturating_add(self.threshold)
    }

    /// Releases every registered object and clears the registry.
    ///
    /// The cells go back to [`ObjectPool`]'s free list, so the slabs survive
    /// (`hydro_runtime_init` may be called again in the same process) and the
    /// next allocation reuses them. Only `ObjectPool`'s `Drop` returns the
    /// slabs to the system allocator.
    fn free_all(&mut self) {
        for &obj in &self.objects {
            // SAFETY: every registered pointer came from `ObjectPool::allocate`
            // in `register_box`, is owned by this registry and is released
            // exactly once here. `release` drops only Rust-owned payloads
            // (`String`, `Vec`, `Box<IterState>`); it never touches other
            // Hydro objects.
            unsafe { self.pool.release(obj) };
        }
        self.objects.clear();
        self.addresses.clear();
        self.last_collection_objects = 0;
        self.objects.shrink_to_fit();
        self.addresses.shrink_to_fit();
        self.refresh_config();
    }
}

thread_local! {
    static HEAP: RefCell<HeapState> = RefCell::new(HeapState::new());
    /// Python-visible identity of this OS thread. `0` means "not assigned
    /// yet"; the first thread that asks becomes the main thread (id 1).
    static THREAD_ID: Cell<i64> = const { Cell::new(0) };
}

/// Set once the process switches to the shared heap. From then on every
/// allocation, registration and collection goes through [`GLOBAL`].
static SHARED: AtomicBool = AtomicBool::new(false);

/// Number of live threads using the shared heap, including the main thread.
/// Starts at 1 (main). Incremented before a new thread starts and decremented
/// when it exits.
static ACTIVE_THREADS: AtomicUsize = AtomicUsize::new(1);

/// Next Python-visible thread id; id 1 is reserved for the main thread.
static NEXT_THREAD_ID: AtomicI64 = AtomicI64::new(2);

/// The process-wide heap used after [`enable_shared`].
///
/// # Invariants
///
/// * The registry owns every `*mut HydroObj` it contains; each pointer was
///   handed out by this heap's [`ObjectPool`] and is released through it
///   exactly once, either by [`HeapState::free_all`] or by a sweep.
/// * `enable_shared` moves the calling thread's pool ownership into this heap
///   ([`ObjectPool::absorb`]), so no thread-local pool ever frees a slab that
///   still holds a shared object.
/// * All access is serialized by the mutex: allocation, registration,
///   collection and migration never overlap, so the runtime itself never
///   holds two mutable aliases of an object.
/// * No user code runs while the mutex is held. Blocking operations (thread
///   join, lock acquisition, `time.sleep`) never take it.
/// * `HydroObj` payloads are `Send`; user code may still mutate a shared
///   container from two threads without a lock, which is a data race by
///   design (documented at the `_thread` module) and the programmer's
///   responsibility.
///
/// `HeapState` only needs `Send`; sharing is provided by the enclosing
/// `Mutex<HeapState>`, which is `Sync` and serialises every access.
unsafe impl Send for HeapState {}

static GLOBAL: LazyLock<Mutex<HeapState>> = LazyLock::new(|| Mutex::new(HeapState::new()));

// Colours, graph lock and write barriers

/// Not yet reached by the marker: swept when a collection ends.
pub(crate) const WHITE: u8 = 0;
/// Reached but not traced: queued on the worklist.
pub(crate) const GREY: u8 = 1;
/// Reached and traced: its children are marked. Edges inserted afterwards are
/// published by the insertion barrier, which marks only the new child.
pub(crate) const BLACK: u8 = 2;

/// Serialises container payload mutation against the marker.
///
/// Held by the [`crate::value`] container guards across the whole mutation
/// and by [`trace`] across the traversal and black transition of one object.
/// Never acquired while the registry or heap lock is held, and never held
/// across a blocking operation or a safepoint.
static GRAPH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Locks the graph, ignoring poisoning: a panic in user code must not leave
/// the collector or a container mutator permanently locked out.
pub(crate) fn graph_lock() -> MutexGuard<'static, ()> {
    GRAPH_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Set while a shared-mode collection is marking. Allocations are born grey
/// and container mutations publish insertion barriers only while this is set.
static MARKING: AtomicBool = AtomicBool::new(false);

// Weak references
//
// A weak reference is an ordinary Python object (`ReferenceType` is a native
// class, so the reference itself is a normal instance) whose target lives in
// this side table, never in a payload edge. The collector therefore never
// traces a weak target: a reference reachable on its own cannot keep its
// target alive.
//
// The table is authoritative for three questions:
//
// * did the target survive the last collection (`target != 0`),
// * does a live reference to a target exist (`getweakrefcount`/`getweakrefs`),
// * and may a resolving call return the target (`weakref_target`).
//
// Entries are pruned and targets cleared while the world is stopped at the
// end of marking (shared mode) or after the mark phase (local mode), before
// any white cell is released. Resolution after that point is safe: an entry
// whose target is still recorded refers to an object this cycle keeps.
//
// Lock order: `WEAK` before the `GLOBAL` heap mutex. No path takes `GLOBAL`
// first and then `WEAK`, and `WEAK` is never held while calling user code or
// while dropping a payload.
struct WeakEntry {
    /// Raw target cell address, or `0` after the target was collected.
    target: usize,
    /// Tagged callback value, or `NONE`. The callback is a strong edge held by
    /// the reference object (stored both here and in its `__callback__`
    /// attribute) and is taken out exactly once when the target is cleared.
    callback: i64,
    /// Whether the owning reference belongs to the process-wide (shared) heap.
    shared: bool,
}

/// One weakref callback waiting to run outside every GC lock.
struct PendingWeakCallback {
    /// Tagged reference object passed to the callback.
    reference: i64,
    /// Tagged callable.
    callback: i64,
    /// Token of the thread that queued it. Callbacks run on that thread, which
    /// owns any private heap the reference belongs to.
    owner: u64,
}

#[derive(Default)]
struct WeakState {
    /// Weak references by their own cell address.
    refs: HashMap<usize, WeakEntry>,
    /// Conservative "keep this reference object alive" pins, installed by
    /// `getweakrefs` while it materialises its result list and by queued
    /// callbacks until they have run. Pins are marked as roots by every
    /// collection and released by [`weakrefs_unpin`]/the callback runner.
    pins: HashMap<usize, usize>,
    /// Callbacks whose targets were cleared and that have not run yet.
    pending: Vec<PendingWeakCallback>,
}

static WEAK: LazyLock<Mutex<WeakState>> = LazyLock::new(|| Mutex::new(WeakState::default()));

/// Number of queued weakref callbacks, readable on the safepoint fast path.
static PENDING_WEAK_CALLBACKS: AtomicUsize = AtomicUsize::new(0);

/// Set once any weak reference with a callback exists. Instance tracing only
/// consults the weak table for the callback edge while this is set.
static WEAK_CALLBACKS_EXIST: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Re-entrancy guard: a callback may allocate and reach a safepoint, which
    /// must not start a nested callback drain.
    static RUNNING_WEAK_CALLBACKS: Cell<bool> = const { Cell::new(false) };
}

/// Set once the shared collector's final rescan has frozen the weak table for
/// the current cycle. Cleared when the cycle completes.
static WEAK_FROZEN: AtomicBool = AtomicBool::new(false);

/// Set when the shared sweep has partitioned the registry. From then on a
/// cell's colour is no longer a liveness signal (survivors are reset to white
/// immediately); registry membership is.
static WEAK_PARTITIONED: AtomicBool = AtomicBool::new(false);

fn lock_weak() -> MutexGuard<'static, WeakState> {
    WEAK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Addresses of grey objects whose children still have to be traced.
///
/// Pushed by root marking, by [`write_barrier_edge`] and by allocation.
/// Popped without holding any other lock, so a marker never holds the
/// worklist while it takes the graph lock.
///
/// Used by the process-wide collector. Single-threaded (local-heap)
/// collections use the thread-local `LOCAL_WORKLIST` instead: the runtime unit
/// tests run in parallel threads, and a shared worklist would let one test
/// trace (and then free) objects owned by another test's heap.
static WORKLIST: LazyLock<Mutex<Vec<usize>>> = LazyLock::new(|| Mutex::new(Vec::new()));

thread_local! {
    /// Worklist of a collection over this thread's private heap.
    static LOCAL_WORKLIST: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn lock_worklist() -> MutexGuard<'static, Vec<usize>> {
    WORKLIST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Queues one object for tracing on the worklist of the running collection.
///
/// While `MARKING` is set the collector owns the process-wide worklist;
/// otherwise only the calling thread can be collecting, so its private
/// worklist is used.
fn push_work(addr: usize) {
    if MARKING.load(Ordering::Acquire) {
        lock_worklist().push(addr);
    } else {
        LOCAL_WORKLIST.with(|worklist| worklist.borrow_mut().push(addr));
    }
}

/// Pops one queued object, or `None` when the active worklist is empty.
///
/// The worklist lock is released before this returns, so the caller traces
/// the object without holding it.
fn pop_work() -> Option<usize> {
    if MARKING.load(Ordering::Acquire) {
        let mut worklist = lock_worklist();
        worklist.pop()
    } else {
        LOCAL_WORKLIST.with(|worklist| worklist.borrow_mut().pop())
    }
}

/// Empties the process-wide worklist (start/abort of a shared collection).
fn clear_worklist() {
    lock_worklist().clear();
}

/// Empties this thread's private worklist.
fn clear_local_worklist() {
    LOCAL_WORKLIST.with(|worklist| worklist.borrow_mut().clear());
}

/// Moves a white object to grey and queues it. Idempotent: grey and black
/// objects are left alone.
fn mark_grey(addr: usize) {
    // SAFETY: callers only pass addresses of live registered objects (roots
    // validated against the address index, children read from live payloads,
    // fresh allocations). No object is freed while marking is in progress.
    unsafe {
        let obj = &*(addr as *const HydroObj);
        if obj
            .marked
            .compare_exchange(WHITE, GREY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            push_work(addr);
        }
    }
}

/// Records that an object's children have been traced.
fn mark_black(addr: usize) {
    // SAFETY: as `mark_grey`; the object is still live.
    unsafe {
        (*(addr as *const HydroObj))
            .marked
            .store(BLACK, Ordering::Release);
    }
}

/// Insertion barrier: publishes a newly stored graph edge `parent -> child`.
///
/// Called *after* `child` was stored into `parent`'s payload and, where the
/// mutation is serialised against the marker, while the container's graph
/// guard or the queue's item mutex is still held. If marking is running and
/// `parent` is already black, `child` is marked grey and queued. The parent
/// stays black, so a container that grows during a collection is traced once
/// instead of once per inserted edge.
///
/// If the parent is white or grey, no barrier is needed: its pending (or the
/// final rescan's) traversal observes the new child, because a mutation and
/// the marker's traversal/black transition are serialised by the graph lock
/// (queues: by the item mutex).
///
/// Takes no heap or registry lock.
pub(crate) fn write_barrier_edge(parent: i64, child: i64) {
    if !MARKING.load(Ordering::Acquire) {
        return;
    }
    if parent & crate::value::TAG_MASK != crate::value::TAG_HEAP
        || child & crate::value::TAG_MASK != crate::value::TAG_HEAP
    {
        return;
    }
    let addr = (parent & !crate::value::TAG_MASK) as usize;
    let child_addr = (child & !crate::value::TAG_MASK) as usize;
    if addr == 0 || child_addr == 0 {
        return;
    }
    // SAFETY: the parent is a live heap object owned by the mutator (the
    // collector only sweeps while every other thread is parked).
    unsafe {
        let obj = &*(addr as *const HydroObj);
        if obj.marked.load(Ordering::Acquire) == BLACK {
            mark_grey(child_addr);
        }
    }
}

/// Marks a tagged child value of a traced container.
///
/// Child values come from the program and are valid tagged heap pointers, so
/// no membership test against the address index is needed (unlike the
/// conservative stack scan).
fn mark_value(value: i64) {
    if value & crate::value::TAG_MASK != crate::value::TAG_HEAP {
        return;
    }
    let addr = (value & !crate::value::TAG_MASK) as usize;
    if addr == 0 {
        return;
    }
    mark_grey(addr);
}

/// Marks every pinned weak-reference object as a root.
///
/// `getweakrefs` pins the references it is about to place in a result list:
/// those references are only reachable from this side table while the list is
/// being built (the Rust `Vec` holding them is invisible to a conservative
/// scan), so the current collection must keep them. `addresses` restricts the
/// mark to the heap the collector is tracing, which matters for the parallel
/// unit tests that each own a private heap.
fn mark_weak_pins(addresses: &AddressSet) {
    let weak = lock_weak();
    for &addr in weak.pins.keys() {
        if addresses.contains(&addr) {
            mark_grey(addr);
        }
    }
    // A queued callback keeps its callable alive until it has run, even if the
    // reference object was dropped from the table in the same clear pass.
    for item in &weak.pending {
        if item.callback & crate::value::TAG_MASK == crate::value::TAG_HEAP {
            let callback_addr = (item.callback & !crate::value::TAG_MASK) as usize;
            if addresses.contains(&callback_addr) {
                mark_grey(callback_addr);
            }
        }
    }
}

/// Whether `target` supports weak references.
///
/// Mirrors CPython's weakref-able set for the value kinds this runtime has:
/// class instances, classes, functions and modules. Built-in containers,
/// strings and numbers are not weak-referenceable and are rejected by the
/// module with a `TypeError`.
pub(crate) fn weakref_supported(target: i64) -> bool {
    if target & crate::value::TAG_MASK != crate::value::TAG_HEAP {
        return false;
    }
    let addr = (target & !crate::value::TAG_MASK) as usize;
    if addr == 0 {
        return false;
    }
    // SAFETY: tagged heap address handed out by this allocator. Callers pass
    // values produced by the runtime, which the collector keeps alive while
    // they are reachable from the module argument buffer.
    let obj = unsafe { object_at(addr) };
    matches!(
        obj.data_ref(),
        ObjDataRef::Instance(_)
            | ObjDataRef::Class(_)
            | ObjDataRef::Function(_)
            | ObjDataRef::ExtFunction(_)
            | ObjDataRef::Module(_)
    )
}

/// Borrows a heap object from a raw address known to belong to this runtime.
///
/// # Safety
///
/// `addr` must be a live registered cell address.
unsafe fn object_at(addr: usize) -> &'static HydroObj {
    &*(addr as *const HydroObj)
}

/// The target to record for a reference created while the shared weak table is
/// frozen (the sweep window).
///
/// Production: a target that is no longer live is dead, so the reference is
/// recorded as already cleared. Unit tests may own a private heap even when
/// another test has enabled shared mode, so an address outside the shared
/// registry is left alone there.
fn frozen_registration_target(target_addr: usize) -> usize {
    let heap = lock_global();
    let registered = heap.addresses.contains(&target_addr);
    if !registered {
        // Production: the address was already removed by the partition, so
        // it is dead. Unit tests may own a private heap even when another
        // test has enabled shared mode, so leave such an address alone.
        #[cfg(test)]
        return target_addr;
        #[cfg(not(test))]
        return 0;
    }
    // After the partition, every remaining address belongs to a survivor or
    // an object allocated during the sweep; colours were reset to white.
    if WEAK_PARTITIONED.load(Ordering::Acquire) {
        return target_addr;
    }
    // Before the partition the rescan colours are authoritative.
    // SAFETY: registered objects are live until the sweep releases them; the
    // heap lock keeps the registry stable.
    if unsafe { object_at(target_addr).marked.load(Ordering::Acquire) != WHITE } {
        target_addr
    } else {
        0
    }
}

/// Whether a tagged value is a registered object of the active heap.
#[allow(dead_code)] // Production-only weak-resolution check; unit tests use private heaps.
fn registered_object(addr: usize) -> bool {
    if addr == 0 {
        return false;
    }
    if shared() {
        lock_global().addresses.contains(&addr)
    } else {
        HEAP.with(|cell| cell.borrow().addresses.contains(&addr))
    }
}

/// Records `reference` as a weak reference to `target` with no callback.
#[allow(dead_code)] // Convenience wrapper retained for tests/embedding.
pub(crate) fn weakref_register(reference: i64, target: i64) -> bool {
    weakref_register_with_callback(reference, target, crate::value::NONE)
}

/// Records `reference` as a weak reference to `target` with `callback`.
///
/// Both values must be tagged heap pointers; the caller (the `_weakref`
/// module) validates that `target` is weak-referenceable and reports the
/// Python-level `TypeError`. The callback is a strong edge owned by the
/// reference object; it is invoked exactly once if the target is collected.
/// `NONE` means no callback. Returns `false` for a malformed pair or a
/// reference that already died. During the sweep phase of a collection, a
/// target that lost the final rescan is recorded as already dead instead of
/// leaving a dangling address in the table.
pub(crate) fn weakref_register_with_callback(reference: i64, target: i64, callback: i64) -> bool {
    if reference & crate::value::TAG_MASK != crate::value::TAG_HEAP
        || target & crate::value::TAG_MASK != crate::value::TAG_HEAP
    {
        return false;
    }
    let ref_addr = (reference & !crate::value::TAG_MASK) as usize;
    let target_addr = (target & !crate::value::TAG_MASK) as usize;
    if ref_addr == 0 || target_addr == 0 || ref_addr == target_addr {
        return false;
    }
    let mut weak = lock_weak();
    // `MARKING && WEAK_FROZEN` is the sweep window: the final rescan chose the
    // survivors and a white target will be freed. Such a reference resolves to
    // `None` immediately rather than to a dangling address.
    let target_addr = if MARKING.load(Ordering::Acquire) && WEAK_FROZEN.load(Ordering::Acquire) {
        frozen_registration_target(target_addr)
    } else {
        target_addr
    };
    weak.refs.insert(
        ref_addr,
        WeakEntry {
            target: target_addr,
            callback,
            shared: shared(),
        },
    );
    if callback != crate::value::NONE {
        WEAK_CALLBACKS_EXIST.store(true, Ordering::Release);
        // Publication barrier for the strong callback edge: when the
        // reference is already black, a collection in progress has traced it
        // and would otherwise miss the newly armed callback. Marking is
        // idempotent; the final rescan/partition keeps the grey callback.
        if MARKING.load(Ordering::Acquire) {
            mark_value(callback);
        }
    }
    true
}

/// Takes the callback out of an entry, if one is still armed.
fn take_callback(entry: &mut WeakEntry) -> i64 {
    if entry.callback == crate::value::NONE {
        crate::value::NONE
    } else {
        std::mem::replace(&mut entry.callback, crate::value::NONE)
    }
}

/// Queues a cleared reference's callback and pins the reference until it runs.
///
/// The caller holds `WEAK`. The reference object may itself be otherwise
/// unreachable in this cycle (its target died), so the pin and the caller's
/// explicit `mark_grey` keep it (and the callback edge) alive.
fn queue_weakref_callback(weak: &mut WeakState, ref_addr: usize, callback: i64) {
    *weak.pins.entry(ref_addr).or_insert(0) += 1;
    weak.pending.push(PendingWeakCallback {
        reference: (ref_addr as i64) | crate::value::TAG_HEAP,
        callback,
        owner: current_gc_token(),
    });
    PENDING_WEAK_CALLBACKS.fetch_add(1, Ordering::AcqRel);
    WEAK_CALLBACKS_EXIST.store(true, Ordering::Release);
    // The callback is also a root through the pending queue (in addition to
    // the reference's side edge): this keeps it alive for a collection that
    // runs before the callback is drained.
    mark_value(callback);
}

/// Marks the callback stored for a weak-reference instance, if any.
///
/// The callback is a strong edge of the reference object. It is normally also
/// present in `__callback__`, but keeping the side table authoritative means a
/// user overwriting that attribute cannot make the collector free a callback
/// that is still armed.
fn mark_weakref_callback(addr: usize) {
    if !WEAK_CALLBACKS_EXIST.load(Ordering::Acquire) {
        return;
    }
    let weak = lock_weak();
    if let Some(entry) = weak.refs.get(&addr) {
        if entry.callback != crate::value::NONE {
            mark_value(entry.callback);
        }
    }
}

/// The target recorded for `reference`, without resolving it.
///
/// `None` means the value is not a registered weak reference; `Some(NONE)`
/// means the reference has been cleared (its target was collected).
pub(crate) fn weakref_target_raw(reference: i64) -> Option<i64> {
    if reference & crate::value::TAG_MASK != crate::value::TAG_HEAP {
        return None;
    }
    let ref_addr = (reference & !crate::value::TAG_MASK) as usize;
    if ref_addr == 0 {
        return None;
    }
    let weak = lock_weak();
    let entry = weak.refs.get(&ref_addr)?;
    Some(if entry.target == 0 {
        crate::value::NONE
    } else {
        (entry.target as i64) | crate::value::TAG_HEAP
    })
}

/// Resolves `reference` for a call, exactly like CPython's `weakref.__call__`.
///
/// Returns the target, or `NONE` once it has been collected. During marking
/// the target is marked grey before it is handed out: the caller now holds a
/// strong reference, so the current cycle must keep it (and its children)
/// alive. When no collection is running the target is returned only if it is
/// still registered; the caller's stack roots it before the next collection's
/// final rescan.
pub(crate) fn weakref_target(reference: i64) -> i64 {
    if reference & crate::value::TAG_MASK != crate::value::TAG_HEAP {
        return crate::value::NONE;
    }
    let ref_addr = (reference & !crate::value::TAG_MASK) as usize;
    if ref_addr == 0 {
        return crate::value::NONE;
    }
    if MARKING.load(Ordering::Acquire) {
        let weak = lock_weak();
        let Some(entry) = weak.refs.get(&ref_addr) else {
            return crate::value::NONE;
        };
        if entry.target == 0 {
            return crate::value::NONE;
        }
        // While we hold the weak lock the table cannot be frozen. Marking the
        // target here closes the window between "entry says alive" and the
        // sweep that could otherwise free it. Only shared entries belong to
        // the process-wide heap a shared collection is tracing.
        if entry.shared {
            mark_grey(entry.target);
        }
        return (entry.target as i64) | crate::value::TAG_HEAP;
    }
    let target = {
        let weak = lock_weak();
        match weak.refs.get(&ref_addr) {
            Some(entry) if entry.target != 0 => (entry.target as i64) | crate::value::TAG_HEAP,
            _ => return crate::value::NONE,
        }
    };
    let target_addr = (target & !crate::value::TAG_MASK) as usize;
    // Unit tests run every module against private heaps while another test
    // may have switched the process to shared mode; only the production
    // configuration can assume the active registry is authoritative.
    #[cfg(not(test))]
    if !registered_object(target_addr) {
        // The recorded target is gone (for example after an embedding reset
        // that bypassed the collector's weak pass); clear it so this and every
        // later call reports `None`.
        let mut weak = lock_weak();
        if let Some(entry) = weak.refs.get_mut(&ref_addr) {
            if entry.target == target_addr {
                entry.target = 0;
            }
        }
        return crate::value::NONE;
    }
    let _ = target_addr;
    target
}

/// Number of live weak references to `target`.
pub(crate) fn weakref_count(target: i64) -> usize {
    if target & crate::value::TAG_MASK != crate::value::TAG_HEAP {
        return 0;
    }
    let target_addr = (target & !crate::value::TAG_MASK) as usize;
    if target_addr == 0 {
        return 0;
    }
    let weak = lock_weak();
    weak.refs
        .values()
        .filter(|entry| entry.target == target_addr)
        .count()
}

/// Tagged weak references whose recorded target is `target`.
///
/// The returned references are pinned until [`weakrefs_unpin`] is called, so
/// the caller can allocate the result list without a collection pruning them.
/// The caller must release the pins on every path.
pub(crate) fn weakrefs_for(target: i64) -> Vec<i64> {
    if target & crate::value::TAG_MASK != crate::value::TAG_HEAP {
        return Vec::new();
    }
    let target_addr = (target & !crate::value::TAG_MASK) as usize;
    if target_addr == 0 {
        return Vec::new();
    }
    let mut weak = lock_weak();
    let candidates: Vec<usize> = weak
        .refs
        .iter()
        .filter_map(|(&ref_addr, entry)| (entry.target == target_addr).then_some(ref_addr))
        .collect();
    let mut refs = Vec::with_capacity(candidates.len());
    for ref_addr in candidates {
        // The table is authoritative for "this reference exists", but a stale
        // entry from a heap that was never swept (embedding reset) must not
        // produce a dangling tagged value. Unit tests may use a private heap
        // while another test has enabled shared mode, so the registry check is
        // production-only.
        #[cfg(not(test))]
        if !registered_object(ref_addr) {
            continue;
        }
        *weak.pins.entry(ref_addr).or_insert(0) += 1;
        refs.push((ref_addr as i64) | crate::value::TAG_HEAP);
    }
    refs
}

/// Releases the pins installed by [`weakrefs_for`] or queued callbacks.
pub(crate) fn weakrefs_unpin(refs: &[i64]) {
    if refs.is_empty() {
        return;
    }
    let mut weak = lock_weak();
    for &reference in refs {
        let addr = (reference & !crate::value::TAG_MASK) as usize;
        if addr == 0 {
            continue;
        }
        let remove = match weak.pins.get_mut(&addr) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if remove {
            weak.pins.remove(&addr);
        }
    }
}

/// Whether any weakref callback is queued.
pub(crate) fn weakref_callbacks_pending() -> bool {
    PENDING_WEAK_CALLBACKS.load(Ordering::Acquire) != 0
}

/// Runs queued weakref callbacks for the calling thread.
///
/// Called from safepoints and after an explicit collection: both are points
/// where no GC lock is held and the caller's live values are describable by
/// the collector. Each callback is popped under the weak lock, then invoked
/// with no lock held. The reference object stays pinned (and, for references
/// that would otherwise be collected in this cycle, grey) until the callback
/// returns. A raising callback is reported like CPython's unraisable output
/// and cleared, leaving no pending exception for the mutator.
pub(crate) fn run_pending_weakref_callbacks() {
    if !weakref_callbacks_pending() {
        return;
    }
    if RUNNING_WEAK_CALLBACKS.with(Cell::get) {
        // A callback allocated and reached a safepoint: the outer drain will
        // keep going once it returns.
        return;
    }
    RUNNING_WEAK_CALLBACKS.with(|running| running.set(true));
    struct ResetGuard;
    impl Drop for ResetGuard {
        fn drop(&mut self) {
            RUNNING_WEAK_CALLBACKS.with(|running| running.set(false));
        }
    }
    let _guard = ResetGuard;

    let owner = current_gc_token();
    loop {
        let item = {
            let mut weak = lock_weak();
            let Some(position) = weak.pending.iter().position(|item| item.owner == owner) else {
                break;
            };
            let item = weak.pending.remove(position);
            PENDING_WEAK_CALLBACKS.fetch_sub(1, Ordering::AcqRel);
            item
        };
        let argument = [item.reference];
        crate::builtins::call(item.callback, 1, argument.as_ptr());
        if exc::pending() {
            let text = crate::value::to_string(exc::pending_value());
            {
                use std::io::Write as _;
                let stderr = std::io::stderr();
                let mut stderr = stderr.lock();
                let _ = writeln!(stderr, "Exception ignored in weakref callback: {text}");
                let _ = stderr.flush();
            }
            exc::clear();
        }
        weakrefs_unpin(&[item.reference]);
    }
}

/// Clears weak references whose targets are white and drops entries whose
/// reference object is white, before a local sweep releases any cell.
///
/// Only entries of this thread's private heap are touched: a parallel test
/// thread owns a different address set, so its entries must be left alone.
/// References whose target died with an armed callback are returned so the
/// caller can resurrect them (mark them grey) before the sweep; the callback
/// itself is pinned and queued in the weak table.
fn clear_weakrefs_local(heap: &HeapState) -> Vec<usize> {
    let mut weak = lock_weak();
    let mut actions: Vec<(usize, i64)> = Vec::new();
    weak.refs.retain(|&ref_addr, entry| {
        if entry.shared || !heap.addresses.contains(&ref_addr) {
            return true;
        }
        // SAFETY: the address came from this heap's registry and the sweep has
        // not released it yet.
        let ref_live = unsafe { object_at(ref_addr).marked.load(Ordering::Acquire) != WHITE };
        let target_dead = entry.target != 0
            && heap.addresses.contains(&entry.target)
            // SAFETY: as above; white targets are still live cells here.
            && unsafe { object_at(entry.target).marked.load(Ordering::Acquire) == WHITE };
        if target_dead {
            entry.target = 0;
            let callback = take_callback(entry);
            if callback != crate::value::NONE {
                actions.push((ref_addr, callback));
            }
        }
        // A dead reference is dropped unless it still has a callback to run;
        // the callback keeps the object reachable through its queue pin.
        ref_live
    });
    let mut resurrect = Vec::with_capacity(actions.len());
    for (ref_addr, callback) in actions {
        queue_weakref_callback(&mut weak, ref_addr, callback);
        resurrect.push(ref_addr);
    }
    resurrect
}

/// Freezes the shared weak table at the end of the final rescan.
///
/// Runs with every other thread parked, before the sweep releases the white
/// cells. References whose object did not survive are removed; targets that
/// did not survive are cleared. Armed callbacks are queued and their reference
/// objects are marked grey (and pinned) so they survive the sweep until the
/// callback runs; the marking is drained after releasing the weak/heap locks.
/// From here until the end of the cycle every recorded target is live, so
/// `weakref.__call__` can return it without a registry lookup.
fn clear_weakrefs_shared() {
    let mut resurrect: Vec<usize> = Vec::new();
    {
        let mut weak = lock_weak();
        let heap = lock_global();
        let mut actions: Vec<(usize, i64)> = Vec::new();
        weak.refs.retain(|&ref_addr, entry| {
            if !entry.shared {
                return true;
            }
            let ref_live = shared_object_live_locked(&heap, ref_addr);
            let target_dead = entry.target != 0 && !shared_object_live_locked(&heap, entry.target);
            if target_dead {
                entry.target = 0;
                let callback = take_callback(entry);
                if callback != crate::value::NONE {
                    actions.push((ref_addr, callback));
                }
            }
            ref_live
        });
        for (ref_addr, callback) in actions {
            queue_weakref_callback(&mut weak, ref_addr, callback);
            resurrect.push(ref_addr);
        }
        WEAK_FROZEN.store(true, Ordering::SeqCst);
    }
    // The world is stopped, so no mutator can observe the pins or the queue
    // yet. Trace the resurrected references (and, through them, the callback
    // edges) without holding the weak or heap lock.
    for addr in resurrect {
        mark_grey(addr);
    }
    drain_worklist();
}

/// Liveness test for a caller already holding the heap lock.
fn shared_object_live_locked(heap: &HeapState, addr: usize) -> bool {
    if addr == 0 || !heap.addresses.contains(&addr) {
        return false;
    }
    // SAFETY: registered objects are live until the sweep releases them; the
    // caller holds the heap lock.
    unsafe { object_at(addr).marked.load(Ordering::Acquire) != WHITE }
}

/// Adopts the calling thread's weak references into the shared heap after
/// [`enable_shared`]. Entries of other (test) heaps that were never migrated
/// stay on their private address set.
fn weakrefs_enable_shared() {
    let mut weak = lock_weak();
    let addresses = lock_global().addresses.clone();
    for (&ref_addr, entry) in weak.refs.iter_mut() {
        if addresses.contains(&ref_addr) {
            entry.shared = true;
        }
    }
}

/// Drops the whole weak table (runtime reset). Process-wide by design: reset
/// is documented as valid only while no runtime thread is alive.
fn weakrefs_reset() {
    let mut weak = lock_weak();
    weak.refs.clear();
    weak.pins.clear();
    weak.pending.clear();
    PENDING_WEAK_CALLBACKS.store(0, Ordering::Release);
    WEAK_CALLBACKS_EXIST.store(false, Ordering::Release);
    WEAK_FROZEN.store(false, Ordering::SeqCst);
    WEAK_PARTITIONED.store(false, Ordering::SeqCst);
}

/// Test-only: forget every weak entry, pin and queued callback.
#[cfg(test)]
pub(crate) fn weakref_tests_clear() {
    let mut weak = lock_weak();
    weak.refs.clear();
    weak.pins.clear();
    weak.pending.clear();
    PENDING_WEAK_CALLBACKS.store(0, Ordering::Release);
    // `WEAK_CALLBACKS_EXIST` is deliberately left set: clearing it could
    // disable callback tracing for a concurrent non-weak test.
    WEAK_FROZEN.store(false, Ordering::SeqCst);
    WEAK_PARTITIONED.store(false, Ordering::SeqCst);
}

/// Test-only: mark a registered reference as cleared without running a
/// collection.
#[cfg(test)]
#[allow(dead_code)] // Used by the module's own test suite when it is declared.
pub(crate) fn weakref_test_kill(reference: i64) {
    let addr = (reference & !crate::value::TAG_MASK) as usize;
    let mut weak = lock_weak();
    let callback = {
        let Some(entry) = weak.refs.get_mut(&addr) else {
            return;
        };
        if entry.target == 0 {
            return;
        }
        entry.target = 0;
        take_callback(entry)
    };
    if callback != crate::value::NONE {
        queue_weakref_callback(&mut weak, addr, callback);
    }
}

/// Serialises unit tests that touch the process-wide weak table.
#[cfg(test)]
pub(crate) static WEAK_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Drains the worklist to a fixed point.
///
/// Each iteration pops one address without holding any lock and traces it
/// (taking the object's graph/queue lock for the duration of that step).
fn drain_worklist() {
    while let Some(addr) = pop_work() {
        // SAFETY: only live registered objects are ever queued.
        unsafe { trace(addr) };
    }
}

// Thread registry and stop-the-world coordination

/// Number of machine words captured per parked thread.
///
/// x86_64 has 16 general-purpose registers. AArch64 has 31 plus `sp`; the
/// 16 most relevant (callee-saved `x19..x29`, `x29`, `x30`, `sp` and the
/// argument/return registers `x0..x2`) are captured, while caller-saved
/// temporaries that are live across the safepoint call are spilled by the
/// caller onto the stack and covered by the stack scan.
const REG_WORDS: usize = 16;

/// Registration and parking state of one runtime thread.
///
/// # Invariants
///
/// * `stack_low`/`stack_high` bound the thread's native stack. They are `0`
///   only until a spawned thread has installed its bounds; the collector
///   refuses to run while a registered thread has unknown bounds.
/// * `saved_sp`/`saved_regs` are written before `parked` is set (release) and
///   describe the thread's live context while it is parked.
/// * `parked` is only changed under the registry mutex, so a collector that
///   observes it set under the same lock sees the saved context.
/// * `exited` means the thread no longer runs user code and is about to be
///   removed from the registry; it is never scanned afterwards.
/// * `pending` mirrors the thread's pending tagged exception so the collector
///   can mark exception objects owned by parked threads.
/// * `handled` mirrors the thread's active exception-handler stack so
///   exceptions reachable only through implicit chaining stay alive.
/// * `roots` holds tagged values that are only reachable from a Rust heap
///   closure (e.g. the arguments of `_thread.start_new_thread`). It is set
///   before the thread is inserted into the registry and is therefore visible
///   to a collector before the thread can run user code; it is cleared when
///   the thread exits.
pub(crate) struct ThreadGc {
    id: i64,
    /// Unique OS-thread token used to associate suspended fiber root ranges.
    token: u64,
    stack_low: AtomicUsize,
    stack_high: AtomicUsize,
    /// Native caller range registered by the outermost fiber resume, plus
    /// the resumed fiber's `caller_ctx` range; scanned as additional roots
    /// while the fiber runs.
    aux_low: AtomicUsize,
    aux_high: AtomicUsize,
    aux2_low: AtomicUsize,
    aux2_high: AtomicUsize,
    parked: AtomicBool,
    exited: AtomicBool,
    saved_sp: AtomicUsize,
    saved_regs: Mutex<[usize; REG_WORDS]>,
    pending: AtomicI64,
    handled: Mutex<Vec<i64>>,
    roots: Mutex<Vec<i64>>,
    /// Persistent roots (`add_persistent_root`): tagged values that stay
    /// reachable for the lifetime of the registration or until the explicit
    /// slot is removed. Async loop roots and similar long-lived Rust-side
    /// owners use them. Cleared by [`leave_thread`] and [`reset`].
    persistent: Mutex<Vec<i64>>,
}

/// Process-wide registration table, keyed by Python-visible thread id.
static THREAD_GC: LazyLock<Mutex<HashMap<i64, Arc<ThreadGc>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Signalled whenever a thread parks/unparks, registers or exits, and when a
/// collection starts or ends.
static GC_CONDVAR: LazyLock<Condvar> = LazyLock::new(Condvar::new);

/// Set while a collection is stopping the world. Checked by the safepoint
/// fast path (acquire) and by every waiting loop.
static GC_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Number of currently parked (non-exited) threads, including blocked ones.
static PARKED: AtomicUsize = AtomicUsize::new(0);

/// Serialises collectors; taken with `try_lock` so a second collector skips.
static GC_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

thread_local! {
    /// Registration of the calling thread; `None` outside the runtime (and in
    /// local-heap mode before `hydro_runtime_init` registered the main thread).
    static GC_SELF: RefCell<Option<Arc<ThreadGc>>> = const { RefCell::new(None) };
    /// Stack-bound overrides pushed while a fiber runs. The bottom entry is
    /// the native stack (or `(0, 0)` when it cannot be determined).
    static STACK_BOUNDS_OVERRIDE: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    /// Additional root ranges of the outermost fiber resume on this thread:
    /// the native caller stack and the resumed fiber's `caller_ctx`. A
    /// `(0, 0)` entry is unused.
    static AUX_STACK: Cell<[(usize, usize); 2]> = const { Cell::new([(0, 0); 2]) };
}

/// Conservative root ranges of live fiber objects, keyed by the owning
/// thread.
///
/// Every fiber registers its private stack and its two register contexts
/// once, at creation, and removes them in `Drop`. Scanning a running fiber's
/// stack is conservative but safe, so a resume performs no registry work.
/// Registrations live outside the `ThreadGc` record because a fiber object
/// may be collected by another thread.
///
/// In shared mode the ranges are scanned from the stop-the-world snapshot of
/// their owner; in local mode the owner scans them itself. A fiber is assumed
/// to stay on the thread that created it (the runtime's fiber contract).
static SUSPENDED_STACKS: LazyLock<Mutex<Vec<(u64, usize, usize)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Source of the per-thread tokens that own suspended fiber root ranges.
///
/// Unlike [`current_thread_id`] (a Python-visible identity that every
/// never-spawned thread claims as `1`, which makes parallel unit tests
/// indistinguishable), a token is unique for the lifetime of the OS thread.
static NEXT_GC_TOKEN: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static GC_TOKEN: Cell<u64> = const { Cell::new(0) };
}

fn current_gc_token() -> u64 {
    GC_TOKEN.with(|token| {
        let mut value = token.get();
        if value == 0 {
            value = NEXT_GC_TOKEN.fetch_add(1, Ordering::Relaxed);
            token.set(value);
        }
        value
    })
}

fn lock_suspended_stacks() -> MutexGuard<'static, Vec<(u64, usize, usize)>> {
    SUSPENDED_STACKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Locks the thread registry, ignoring poisoning: a panic in user code must
/// not prevent other threads from parking or registering.
fn lock_thread_registry() -> MutexGuard<'static, HashMap<i64, Arc<ThreadGc>>> {
    THREAD_GC
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Returns the calling thread's registration, if any.
fn current_gc() -> Option<Arc<ThreadGc>> {
    GC_SELF.with(|cell| cell.borrow().clone())
}

/// Captures all registers the collector needs for the calling thread.
fn capture_registers() -> [usize; REG_WORDS] {
    let dumped = spill_registers();
    let mut regs = [0usize; REG_WORDS];
    if dumped.len() <= REG_WORDS {
        regs[..dumped.len()].copy_from_slice(&dumped);
        return regs;
    }
    // AArch64: keep callee-saved registers plus argument/stack registers.
    const PICK: [usize; REG_WORDS] = [19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 0, 1, 2];
    for (slot, &index) in PICK.iter().enumerate() {
        regs[slot] = dumped[index];
    }
    regs
}

/// Saves the calling thread's stack pointer and registers into `gc`.
///
/// Called before the thread is marked parked, so a collector that observes
/// `parked` under the registry lock is guaranteed to see this context.
fn save_context(gc: &ThreadGc) {
    // `marker` lives in this frame, which is deeper than every caller frame;
    // scanning `marker..stack_high` therefore covers the whole live stack.
    let marker: usize = 0;
    gc.saved_sp
        .store((&marker as *const usize) as usize, Ordering::Relaxed);
    let mut slot = gc.saved_regs.lock().unwrap_or_else(|e| e.into_inner());
    *slot = capture_registers();
}

/// Cooperative safepoint (ABI `hydro_safepoint`).
///
/// The fast path is a single relaxed load (the slow path re-checks under the
/// registry lock, which supplies the ordering). If a collection is requested,
/// the calling thread saves its context, parks, and waits until the collector
/// has swept and cleared the request. Generated code calls this at every
/// function entry and loop backedge; every allocation and blocking operation
/// calls it too.
#[inline]
pub(crate) fn safepoint() {
    // Callbacks queued by an earlier collection run here: a safepoint is
    // outside every GC lock and generated code's live values are described by
    // the precise shadow stack. Never run them while a collection is trying
    // to stop this thread.
    if !GC_REQUESTED.load(Ordering::Relaxed) && weakref_callbacks_pending() {
        run_pending_weakref_callbacks();
    }
    if !GC_REQUESTED.load(Ordering::Relaxed) {
        return;
    }
    let Some(gc) = current_gc() else {
        return;
    };
    save_context(&gc);
    let mut registry = lock_thread_registry();
    // Re-check under the lock: the previous collector may have finished
    // while the context was being saved. Conversely, a collector that starts
    // after this check waits for the next safepoint, which is always reached
    // before the thread touches the heap again.
    if !GC_REQUESTED.load(Ordering::SeqCst) {
        return;
    }
    // Publish the exact roots of this stopped context before parking; the
    // collector dereferences them only while the thread stays parked.
    roots::publish_current();
    if !gc.parked.swap(true, Ordering::SeqCst) {
        PARKED.fetch_add(1, Ordering::SeqCst);
    }
    GC_CONDVAR.notify_all();
    while GC_REQUESTED.load(Ordering::SeqCst) {
        registry = GC_CONDVAR.wait(registry).unwrap_or_else(|e| e.into_inner());
    }
    if gc.parked.swap(false, Ordering::SeqCst) {
        PARKED.fetch_sub(1, Ordering::SeqCst);
    }
    GC_CONDVAR.notify_all();
}

/// Marks the calling thread as blocked/parked for the duration of a blocking
/// operation (lock acquisition, `_thread.join`, `time.sleep`).
///
/// An in-flight collection is awaited first, so the thread cannot block while
/// a collector is still waiting for it. The thread then parks: its saved
/// context stays scannable while it sleeps on a lock/condvar/OS wait.
///
/// Contract: from [`blocking_enter`] until [`blocking_leave`] the thread may
/// only wait. It must not allocate a Hydro object, mutate a container or publish
/// an insertion barrier, because a collection treats a parked thread as stopped and
/// will not re-scan the code it runs meanwhile.
///
/// Locking rules: the caller must not hold the heap mutex, the registry mutex,
/// the graph lock or the collector mutex.
pub(crate) fn blocking_enter() {
    if !shared() {
        // Local mode has no collector that could wait for this thread.
        return;
    }
    let Some(gc) = current_gc() else {
        return;
    };
    // Wait out a collection that is already stopping the world.
    safepoint();
    save_context(&gc);
    // Publish the exact roots of the context that is about to block: it may
    // stay parked for a long time and a collection must see its frames.
    roots::publish_current();
    let _registry = lock_thread_registry();
    // If a collection started between the safepoint and here, the collector
    // either already sees this thread parked (below) or is still waiting for
    // it; parking now satisfies it. The block itself never takes a runtime
    // lock, so this cannot deadlock.
    if !gc.parked.swap(true, Ordering::SeqCst) {
        PARKED.fetch_add(1, Ordering::SeqCst);
    }
    GC_CONDVAR.notify_all();
}

/// Ends a blocking operation. Waits for a running collection to finish before
/// returning to user code, then unparks.
pub(crate) fn blocking_leave() {
    if !shared() {
        return;
    }
    let Some(gc) = current_gc() else {
        return;
    };
    let mut registry = lock_thread_registry();
    while GC_REQUESTED.load(Ordering::SeqCst) {
        registry = GC_CONDVAR.wait(registry).unwrap_or_else(|e| e.into_inner());
    }
    if gc.parked.swap(false, Ordering::SeqCst) {
        PARKED.fetch_sub(1, Ordering::SeqCst);
    }
    GC_CONDVAR.notify_all();
}

/// Mirrors the calling thread's pending exception value (tagged `i64`) into
/// its registration so parked threads' exception objects stay reachable.
pub(crate) fn set_pending_mirror(value: i64) {
    if let Some(gc) = current_gc() {
        gc.pending.store(value, Ordering::Release);
    }
}

/// Mirrors the calling thread's handled-exception stack into its registration
/// so a collector keeps exceptions that only implicit chaining references.
pub(crate) fn set_handled_mirror(values: &[i64]) {
    if let Some(gc) = current_gc() {
        let mut handled = gc
            .handled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        handled.clear();
        handled.extend_from_slice(values);
    }
}

/// Adds `value` to the calling thread's out-of-stack roots so a collection
/// keeps it alive while it is only reachable from Rust-side state (the
/// unittest class registry). A no-op outside the runtime, where no collector
/// can run.
pub(crate) fn add_thread_root(value: i64) {
    if let Some(gc) = current_gc() {
        gc.roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(value);
    }
}

/// Pushes `values` onto the calling thread's out-of-stack roots and returns a
/// marker for [`release_roots`]. Used by container builders whose payloads
/// live in Rust heap memory that the conservative collector cannot scan.
pub(crate) fn root_values(values: &[i64]) -> usize {
    if let Some(gc) = current_gc() {
        let mut roots = gc
            .roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = roots.len();
        roots.extend_from_slice(values);
        start
    } else {
        0
    }
}

/// Drops the roots added since `start`.
pub(crate) fn release_roots(start: usize) {
    if let Some(gc) = current_gc() {
        let mut roots = gc
            .roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if start <= roots.len() {
            roots.truncate(start);
        }
    }
}

/// Registers a persistent root and returns its stable slot index.
///
/// A persistent root keeps `value` reachable from every subsequent
/// collection until the slot is overwritten, removed, the owning thread
/// exits or [`reset`] runs. It is the right tool for Rust-side owners that
/// outlive a single call (the asyncio event loop's scheduled tasks) and that
/// generated frames cannot describe.
///
/// Returns `usize::MAX` outside the runtime, where no collector can run.
/// The function never allocates a Hydro object and takes no heap lock.
pub(crate) fn add_persistent_root(value: i64) -> usize {
    let Some(gc) = current_gc() else {
        return usize::MAX;
    };
    let mut persistent = gc
        .persistent
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let slot = persistent.len();
    persistent.push(value);
    slot
}

/// Overwrites the persistent root in `slot`.
///
/// `value` may be tagged `None` to drop the root without releasing the slot.
/// Out-of-range slots (a malformed or stale handle) are ignored with a debug
/// assertion: a bad slot must never corrupt the registry.
pub(crate) fn set_persistent_root(slot: usize, value: i64) {
    let Some(gc) = current_gc() else {
        return;
    };
    let mut persistent = gc
        .persistent
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match persistent.get_mut(slot) {
        Some(entry) => *entry = value,
        None => debug_assert!(false, "set_persistent_root: slot {slot} out of range"),
    }
}

/// Removes the persistent root in `slot`.
///
/// The slot stops retaining its value immediately; trailing empty slots are
/// trimmed so a push after the last removal reuses their index. Like
/// [`set_persistent_root`], an out-of-range slot is ignored (debug
/// assertion). Never dereferences the tagged value.
pub(crate) fn remove_persistent_root(slot: usize) {
    let Some(gc) = current_gc() else {
        return;
    };
    let mut persistent = gc
        .persistent
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match persistent.get_mut(slot) {
        Some(entry) => *entry = crate::value::NONE,
        None => {
            debug_assert!(false, "remove_persistent_root: slot {slot} out of range");
            return;
        }
    }
    while persistent.last() == Some(&crate::value::NONE) {
        persistent.pop();
    }
}

/// Locks the shared heap, ignoring poisoning: a panic in user code must not
/// make the process unable to allocate. The registry is always consistent
/// because no user code runs while the lock is held.
fn lock_global() -> MutexGuard<'static, HeapState> {
    GLOBAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn shared() -> bool {
    SHARED.load(Ordering::Acquire)
}

/// Whether the process-wide heap (and therefore concurrent marking) is
/// active. Container mutations can skip the graph lock entirely before the
/// first thread is spawned: the local collector only runs on the same thread,
/// so no marker can observe a payload concurrently.
pub(crate) fn shared_mode() -> bool {
    shared()
}

/// Reads the collector configuration. Defensive: malformed values fall back
/// to the defaults and an unusable threshold is ignored.
fn read_config() -> (usize, bool) {
    let threshold = std::env::var("HYDRO_GC_THRESHOLD")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_THRESHOLD);
    let stress = std::env::var("HYDRO_GC_STRESS")
        .map(|value| value.trim() == "1")
        .unwrap_or(false);
    (threshold, stress)
}

/// Whether strict precise-root mode is active (`HYDRO_GC_PRECISE=1`).
///
/// Read on every collection (not cached) so a test or an embedding process
/// can switch modes without restarting the runtime; the lookup is off the
/// mutator fast path. See the module documentation for the exact root set
/// that survives in each mode.
pub(crate) fn precise_mode() -> bool {
    std::env::var("HYDRO_GC_PRECISE")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

/// Whether precise frames/ranges participate in collection (default) or the
/// collector ignores them and falls back to conservative scanning (`0`).
pub(crate) fn precise_enabled() -> bool {
    !std::env::var("HYDRO_GC_PRECISE")
        .map(|value| value.trim() == "0")
        .unwrap_or(false)
}

/// Allocates a heap object, registers it and returns its tagged value.
///
/// In shared mode the caller first reaches a safepoint (so it can be parked
/// by a concurrent collector), then checks the collection policy under a
/// short heap lock. If collection is due, the heap lock is released and the
/// collection runs *before* the new object exists, so the sweep can never
/// observe an unreferenced half-created object. The object is then created
/// and registered under the heap lock.
pub(crate) fn allocate(kind: u32, data: ObjData) -> i64 {
    if shared() {
        safepoint();
        let due = lock_global().collection_due();
        if due {
            // Another collector may be running; it then skips (returns 0),
            // which is fine: that collector will reclaim the garbage.
            let _ = collect_shared(None);
            // A collection may have started between the policy check and the
            // allocation; park before touching the heap again.
            safepoint();
        }
        let mut heap = lock_global();
        register_box(&mut heap, kind, data)
    } else {
        HEAP.with(|cell| {
            let mut heap = cell.borrow_mut();
            let tagged = register_box(&mut heap, kind, data);
            if heap.collection_due() {
                collect_state(&mut heap, Some(tagged));
            }
            tagged
        })
    }
}

/// Creates the object in `heap`'s slab pool and inserts it into the registry.
///
/// The object is registered before its tagged value is returned, and the
/// caller passes that value explicitly as a root to any collection triggered
/// below, so the object cannot be freed before the caller sees it.
///
/// While a concurrent mark is in progress the fresh object is born grey and
/// queued: it may already have children (e.g. a list filled right after
/// creation) and the marker must not sweep it before they are traced.
fn register_box(heap: &mut HeapState, kind: u32, data: ObjData) -> i64 {
    let raw = heap.pool.allocate(kind, data);
    let addr = raw as usize;
    debug_assert_eq!(
        addr & TAG_MASK as usize,
        0,
        "heap object not 8-byte aligned"
    );
    heap.objects.push(raw);
    heap.addresses.insert(addr);
    if MARKING.load(Ordering::Acquire) {
        mark_grey(addr);
    }
    (addr as i64) | TAG_HEAP
}

/// Number of currently registered (live) objects.
pub(crate) fn objects() -> usize {
    if shared() {
        lock_global().objects.len()
    } else {
        HEAP.with(|cell| cell.borrow().objects.len())
    }
}

/// Migrates the calling thread's heap into the process-wide shared heap and
/// switches every future allocation/collection to it. Idempotent.
///
/// Called by `_thread.start_new_thread` before the new thread exists, so the
/// migration cannot race with an allocation. After this call the local heap is
/// empty and stays unused for the rest of the process.
pub(crate) fn enable_shared() {
    if shared() {
        return;
    }
    let mut global_heap = lock_global();
    if shared() {
        return;
    }
    HEAP.with(|cell| {
        let mut local = cell.borrow_mut();
        global_heap.objects.append(&mut local.objects);
        global_heap.addresses.extend(local.addresses.drain());
        // Slab ownership follows the objects: without this the thread-local
        // pool would deallocate slabs that still hold objects registered in
        // the shared heap.
        global_heap.pool.absorb(&mut local.pool);
        // The local heap keeps its threshold/stress configuration for a later
        // `hydro_runtime_init`; the objects now belong to the shared registry.
        local.objects.shrink_to_fit();
        local.addresses.shrink_to_fit();
    });
    SHARED.store(true, Ordering::Release);
    // The weak table moves with the objects it describes. Take the weak lock
    // only after releasing the heap lock (documented order: `WEAK` first).
    drop(global_heap);
    weakrefs_enable_shared();
    // `_thread` may be started from inside a fiber. The bound/aux mirroring
    // was a no-op while the heap was local; publish the current state now so
    // a shared safepoint can validate this thread's saved context.
    if let Some((low, high)) = stack_bounds() {
        update_thread_bounds(low, high);
    }
    let aux = AUX_STACK.with(Cell::get);
    if aux != [(0, 0); 2] {
        set_aux_stack(aux[0].0, aux[0].1, aux[1].0, aux[1].1);
    }
}

/// Frees every registered object and clears the registry. Called by
/// `hydro_runtime_init` after the exception state has been reset, so no tagged
/// pending value can survive the free.
///
/// Only valid while no runtime thread is alive: `hydro_runtime_init` must run
/// once at program start, never from a spawned thread.
pub(crate) fn reset() {
    // Unit tests run concurrently in one process. A test that inspects the
    // spawn registry must not lose it to another test's `hydro_runtime_init`.
    #[cfg(test)]
    let _registry_guard = crate::spawn::TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Invalidate per-thread class caches (they compare against the
    // generation) and drop every weak reference: the heap they describe is
    // about to be freed.
    GENERATION.fetch_add(1, Ordering::Relaxed);
    weakrefs_reset();
    // The async loop owns persistent roots; drop it first, then clear every
    // registration's persistent slots. A fresh run must not inherit them.
    crate::modules::asyncio::reset();
    {
        let registry = lock_thread_registry();
        for gc in registry.values() {
            gc.persistent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }
    }
    HEAP.with(|cell| cell.borrow_mut().free_all());
    // A fresh program run must not resolve functions from the previous one.
    crate::spawn::reset();
    // The registered unittest classes lived in the freed heap; drop the
    // stale tagged values before user code runs.
    crate::modules::unittest::reset();
    // Defensive cleanup of a previous run in the same process (the runtime
    // tests reuse their process). Legal because the precondition above says
    // no thread is alive.
    if shared() {
        lock_global().free_all();
        SHARED.store(false, Ordering::Release);
    }
    MARKING.store(false, Ordering::SeqCst);
    clear_worklist();
    clear_local_worklist();
    // No fiber is running or suspended across a runtime reset; this thread's
    // conservative root ranges of the previous run are stale. Ranges of other
    // threads (parallel unit tests) are left untouched.
    let token = current_gc_token();
    lock_suspended_stacks().retain(|&(owner, _, _)| owner != token);
    STACK_BOUNDS_OVERRIDE.with(|bounds| bounds.borrow_mut().clear());
    AUX_STACK.with(|slot| slot.set([(0, 0); 2]));
    ACTIVE_THREADS.store(1, Ordering::Release);
    THREAD_ID.with(|id| id.set(1));
    // Drop stale registrations from a previous run and register the main
    // thread, whose stack the local-mode collector and every shared-mode
    // collection scan.
    {
        let mut registry = lock_thread_registry();
        registry.clear();
        GC_CONDVAR.notify_all();
    }
    GC_REQUESTED.store(false, Ordering::SeqCst);
    PARKED.store(0, Ordering::SeqCst);
    GC_SELF.with(|cell| cell.borrow_mut().take());
    // The previous run's shadow frames and their publication are stale: the
    // slots they describe may already be gone from this thread's stack.
    roots::clear_current();
    register_current_thread(1, Vec::new());
}

/// Forces a collection (ABI `hydro_gc_collect`). Returns the number of freed
/// objects; returns `0` when another collector is already running.
pub(crate) fn collect() -> i64 {
    let freed = if shared() {
        collect_shared(None)
    } else {
        collect_with_root(None) as i64
    };
    // `gc.collect()` is a safe point: no GC lock is held and no unrooted
    // allocation is in flight. Run callbacks queued by this (or an earlier)
    // collection here, the same way CPython runs them from the collector.
    run_pending_weakref_callbacks();
    freed
}

/// Root set of one registered thread, captured while it is parked.
struct RootSnapshot {
    /// Saved stack pointer (`0` for the collector's own registration, whose
    /// live stack is scanned directly).
    sp: usize,
    high: usize,
    regs: [usize; REG_WORDS],
    pending: i64,
    handled: Vec<i64>,
    roots: Vec<i64>,
    /// Persistent roots of the thread registration.
    persistent: Vec<i64>,
    /// Precise frames/ranges active in this thread's context (published by
    /// the mutator at its safepoint, or read directly for the collector).
    precise: PublishedRoots,
    /// Conservative root ranges of suspended fibers owned by this thread.
    suspended: Vec<(usize, usize)>,
}

/// True when every registered thread other than the caller is parked or has
/// exited. The caller must hold the registry lock.
fn all_others_parked(registry: &HashMap<i64, Arc<ThreadGc>>) -> bool {
    let self_id = current_thread_id();
    registry.iter().all(|(&id, gc)| {
        id == self_id || gc.exited.load(Ordering::Acquire) || gc.parked.load(Ordering::Acquire)
    })
}

/// True when every live thread has a registration.
///
/// A thread that was spawned but has not reached [`register_thread`] yet is
/// invisible to the registry while it already holds tagged values in its Rust
/// closure; its stack must not be scanned, so collections are abandoned until
/// it registered (the spawner waits for that before returning, so this is a
/// very short window). `ACTIVE_THREADS` is decremented after a thread removes
/// its registration, so a stale over-count only costs one skipped collection.
fn all_threads_registered(registry: &HashMap<i64, Arc<ThreadGc>>) -> bool {
    ACTIVE_THREADS.load(Ordering::Acquire) <= registry.len()
}

/// Collects the root sets of every registered thread and validates their
/// saved contexts.
///
/// The caller must hold the registry lock and have verified
/// [`all_others_parked`]. Returns `None` when a thread's saved context cannot
/// be scanned safely; the caller then abandons the collection instead of
/// freeing possibly reachable objects.
fn snapshot_roots(registry: &HashMap<i64, Arc<ThreadGc>>) -> Option<Vec<RootSnapshot>> {
    let self_id = current_thread_id();
    // The world is stopped for the snapshot, so the global registry is
    // stable while it is copied. Published precise roots are keyed by the
    // per-OS-thread token used by the registration.
    let all_suspended: Vec<(u64, usize, usize)> = lock_suspended_stacks().clone();
    let mut published: HashMap<u64, PublishedRoots> = roots::snapshot_all().into_iter().collect();
    // The collector's own context never parked, so its publication may be
    // older than the frames it holds right now; read the live set instead.
    let self_precise = roots::snapshot_current();
    let mut snapshots = Vec::with_capacity(registry.len());
    for (&id, gc) in registry.iter() {
        let pending = gc.pending.load(Ordering::Acquire);
        // Roots are installed before the thread is inserted into the registry
        // and only cleared after it marked itself exited, so copying them
        // under the registry lock is safe.
        let roots = gc
            .roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let handled = gc
            .handled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let persistent = gc
            .persistent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let precise = if id == self_id {
            self_precise.clone()
        } else {
            // A parked thread published just before parking; a missing entry
            // means it never used precise roots.
            published.remove(&gc.token).unwrap_or_default()
        };
        let mut suspended: Vec<(usize, usize)> = all_suspended
            .iter()
            .filter(|&&(owner, _, _)| owner == gc.token)
            .map(|&(_, low, high)| (low, high))
            .collect();
        let aux = [
            (
                gc.aux_low.load(Ordering::Acquire),
                gc.aux_high.load(Ordering::Acquire),
            ),
            (
                gc.aux2_low.load(Ordering::Acquire),
                gc.aux2_high.load(Ordering::Acquire),
            ),
        ];
        for (low, high) in aux {
            if high > low {
                suspended.push((low, high));
            }
        }
        if id == self_id {
            // The caller's live stack/registers are scanned directly; only
            // its pending mirror, roots and suspended fiber ranges contribute
            // separately.
            snapshots.push(RootSnapshot {
                sp: 0,
                high: 0,
                regs: [0; REG_WORDS],
                pending,
                handled,
                roots,
                persistent,
                precise,
                suspended,
            });
            continue;
        }
        if gc.exited.load(Ordering::Acquire) {
            continue;
        }
        if !gc.parked.load(Ordering::Acquire) {
            return None;
        }
        let sp = gc.saved_sp.load(Ordering::Acquire);
        let high = gc.stack_high.load(Ordering::Acquire);
        let low = gc.stack_low.load(Ordering::Acquire);
        if sp == 0 || high == 0 || low == 0 || sp < low || sp > high {
            return None;
        }
        let regs = *gc.saved_regs.lock().unwrap_or_else(|e| e.into_inner());
        snapshots.push(RootSnapshot {
            sp,
            high,
            regs,
            pending,
            handled,
            roots,
            persistent,
            precise,
            suspended,
        });
    }
    Some(snapshots)
}

/// Byte extents of the precise frames/ranges in `snapshots`, sorted and
/// merged.
///
/// Conservative stack scans skip these bytes: the precise slots are marked
/// exactly, so scanning their raw words again would only resurrect stale
/// values that the precise shadow stack has already forgotten (a slot reused
/// for a dead temporary). Merging makes the exclusion check a single linear
/// pass alongside the scan cursor.
fn precise_exclusions(snapshots: &[RootSnapshot]) -> Vec<(usize, usize)> {
    let mut extents: Vec<(usize, usize)> = Vec::new();
    let mut push = |precise: &PublishedRoots| {
        for frame in precise.frames() {
            if frame.high > frame.low {
                extents.push((frame.low, frame.high));
            }
        }
        for range in precise.ranges() {
            let low = range.base as usize;
            let high = low.saturating_add(range.count.saturating_mul(8));
            if high > low {
                extents.push((low, high));
            }
        }
    };
    for snapshot in snapshots {
        push(&snapshot.precise);
    }
    extents.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(extents.len());
    for (low, high) in extents {
        match merged.last_mut() {
            Some(last) if low <= last.1 => last.1 = last.1.max(high),
            _ => merged.push((low, high)),
        }
    }
    merged
}

/// Marks every root of a snapshot set plus the caller's live stack,
/// registers and pending exception.
///
/// Precise frames/ranges are always marked slot by slot through
/// [`mark_word`], so a stale tagged value can only be ignored (the address
/// index validates it), never dereferenced. In strict mode (`strict`) no
/// conservative stack/register scan runs at all; in mixed mode those scans
/// skip the byte extents of the precise slots.
fn mark_roots(
    snapshots: &[RootSnapshot],
    extra_root: Option<i64>,
    addresses: &AddressSet,
    self_sp: usize,
    self_high: usize,
    strict: bool,
) {
    let precise_on = precise_enabled();
    let exclusions = if precise_on {
        precise_exclusions(snapshots)
    } else {
        Vec::new()
    };
    if let Some(root) = extra_root {
        mark_word(root as usize, addresses);
    }
    // The caller's own pending exception, handler stack and live context.
    mark_word(exc::pending_value() as usize, addresses);
    for value in exc::handled_values() {
        mark_word(value as usize, addresses);
    }
    if !strict {
        scan_stack_excluding(self_sp, self_high, addresses, &exclusions);
        mark_captured_registers(addresses);
    }

    // Parked threads' precise roots, pending mirrors, handler stacks,
    // out-of-stack roots (spawn arguments held only by Rust closures),
    // persistent roots, saved stacks/registers and suspended fiber ranges.
    let mut precise_frames = 0u64;
    for thread in snapshots {
        precise_frames += thread.precise.frames().len() as u64;
        if precise_on {
            thread
                .precise
                .mark(|value| mark_word(value as usize, addresses));
        }
        mark_word(thread.pending as usize, addresses);
        for &value in &thread.handled {
            mark_word(value as usize, addresses);
        }
        for &root in &thread.roots {
            mark_word(root as usize, addresses);
        }
        for &root in &thread.persistent {
            mark_word(root as usize, addresses);
        }
        if strict {
            continue;
        }
        if thread.high > thread.sp {
            scan_stack_excluding(thread.sp, thread.high, addresses, &exclusions);
        }
        for &register in &thread.regs {
            mark_word(register, addresses);
        }
        for &(low, high) in &thread.suspended {
            if high > low {
                scan_stack_excluding(low, high, addresses, &exclusions);
            }
        }
    }
    // References pinned by an in-progress `getweakrefs` are keeping their
    // result objects reachable from outside the scanned stacks.
    mark_weak_pins(addresses);
    STAT_PRECISE_FRAMES.store(precise_frames, Ordering::Relaxed);
}

/// Abandons a collection: resets every colour, drops the worklist and
/// releases the world. The caller must hold the registry lock.
///
/// Leaking is always preferable to freeing a reachable object; the next
/// collection retries from a clean, all-white heap.
fn abort_shared_collection() -> i64 {
    {
        let heap = lock_global();
        for &obj in &heap.objects {
            // SAFETY: every registered pointer is live.
            unsafe { (*(obj)).marked.store(WHITE, Ordering::Release) };
        }
        // Cleared under the heap lock for the same reason as in the sweep:
        // no allocation may observe `MARKING` after the heap lock is
        // released without also being seen by the next collection.
        MARKING.store(false, Ordering::SeqCst);
        WEAK_FROZEN.store(false, Ordering::SeqCst);
        WEAK_PARTITIONED.store(false, Ordering::SeqCst);
    }
    clear_worklist();
    GC_REQUESTED.store(false, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    0
}

/// Reads `HYDRO_GC_STATS` once per collection; stats are off by default.
fn gc_stats_enabled() -> bool {
    std::env::var("HYDRO_GC_STATS")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

/// Runs an incremental concurrent mark-&-sweep in shared mode.
///
/// Only two short windows stop the world (root snapshot and final rescan);
/// marking and the dropping of white payloads run while the other threads
/// keep mutating. Returns `0` without collecting when another collector is
/// already running, or when a thread context cannot be scanned safely. The
/// caller must not hold the heap mutex.
fn collect_shared(extra_root: Option<i64>) -> i64 {
    let Ok(_collector) = GC_LOCK.try_lock() else {
        return 0;
    };

    // The caller's own stack must be bounded before anything is marked;
    // leaking is always preferable to freeing a reachable object.
    let marker: usize = 0;
    let self_sp = &marker as *const usize as usize;
    let Some((self_low, self_high)) = stack_bounds() else {
        return 0;
    };
    if self_sp < self_low || self_sp >= self_high {
        return 0;
    }

    let stats = gc_stats_enabled();
    // Strict precise mode disables every conservative stack/register scan;
    // mixed mode keeps them but excludes the precise slot extents.
    let strict = precise_mode();
    let started = Instant::now();

    // A successful collection leaves the worklist empty and an aborted one
    // clears it; start from a known state anyway.
    clear_worklist();
    // From now on allocations are born grey and inserted edges mark their
    // child when the parent is already black, so the mutators may run while
    // the worklist drains.
    MARKING.store(true, Ordering::SeqCst);
    WEAK_PARTITIONED.store(false, Ordering::SeqCst);

    // ---- Stage A: root snapshot (stop the world) ----
    let mut registry = lock_thread_registry();
    GC_REQUESTED.store(true, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    while !all_others_parked(&registry) {
        registry = GC_CONDVAR.wait(registry).unwrap_or_else(|e| e.into_inner());
    }
    // A thread that has not registered yet is invisible to the scan and may
    // hold the only reference to an object; abandon rather than risk it.
    if !all_threads_registered(&registry) {
        return abort_shared_collection();
    }
    let addresses = {
        // All other threads are parked: the registry and address index are
        // stable while they are copied.
        let heap = lock_global();
        heap.addresses.clone()
    };
    let Some(snapshots) = snapshot_roots(&registry) else {
        return abort_shared_collection();
    };
    mark_roots(
        &snapshots, extra_root, &addresses, self_sp, self_high, strict,
    );
    drop(snapshots);
    drop(addresses);
    // Release the world: the mutators run while the worklist drains.
    GC_REQUESTED.store(false, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    drop(registry);
    let pause_a = started.elapsed();

    // ---- Stage B: concurrent marking ----
    let mark_start = Instant::now();
    drain_worklist();
    let mark_time = mark_start.elapsed();

    // ---- Stage C: final rescan (stop the world) ----
    let pause_start = Instant::now();
    let mut registry = lock_thread_registry();
    GC_REQUESTED.store(true, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    while !all_others_parked(&registry) {
        registry = GC_CONDVAR.wait(registry).unwrap_or_else(|e| e.into_inner());
    }
    if !all_threads_registered(&registry) {
        return abort_shared_collection();
    }
    // The registry lock stays held across rescan and drain: no thread can
    // register, allocate or publish a barrier once every other thread is
    // parked, so the final drain is complete.
    let addresses = {
        let heap = lock_global();
        heap.addresses.clone()
    };
    let Some(snapshots) = snapshot_roots(&registry) else {
        return abort_shared_collection();
    };
    mark_roots(
        &snapshots, extra_root, &addresses, self_sp, self_high, strict,
    );
    drop(snapshots);
    drop(addresses);
    drain_worklist();
    // Defensive: no grey object should survive the final drain. If one does
    // (a barrier/allocation that raced a phase transition), re-queue it now
    // and trace it; the world is stopped, so this both terminates and avoids
    // sweeping a parent whose children are still white.
    loop {
        let mut found = false;
        {
            let heap = lock_global();
            for &obj in &heap.objects {
                // SAFETY: every registered pointer is live.
                if unsafe { (*obj).marked.load(Ordering::Acquire) } == GREY {
                    push_work(obj as usize);
                    found = true;
                }
            }
        }
        if !found {
            break;
        }
        drain_worklist();
    }
    // The world is still stopped and no white cell has been released. Freeze
    // the weak table now: references whose object lost the rescan disappear,
    // targets that lost it are cleared, and every target that remains is
    // guaranteed to survive this cycle.
    clear_weakrefs_shared();
    // Release the world for the sweep. `MARKING` stays set, so allocations
    // made now are born grey and queued; they are live by construction and
    // the partition below keeps them.
    GC_REQUESTED.store(false, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    drop(registry);
    let pause = pause_a + pause_start.elapsed();

    // ---- Stage D: concurrent sweep ----
    let sweep_start = Instant::now();
    // The only heap-lock section of the sweep: partition the registry. The
    // white objects leave it, survivors are reset to white and the address
    // index is rebuilt. Nothing is dropped under the lock.
    let whites = {
        let mut heap = lock_global();
        let whites = partition_whites(&mut heap);
        heap.last_collection_objects = heap.objects.len();
        // Colours are no longer a liveness signal from here on.
        WEAK_PARTITIONED.store(true, Ordering::SeqCst);
        whites
    };
    let freed = whites.len();
    // Drop payloads outside every lock, with the mutators running: no Hydro
    // object can reference these addresses any more (the final rescan and
    // insertion barriers published every live edge before the world was
    // released), so the payloads can be dropped concurrently with allocation
    // and mutation. The cells themselves are returned to the pool in stage E,
    // which already takes the heap lock briefly for the colour reset.
    for &obj in &whites {
        // SAFETY: every pointer came from `ObjectPool::allocate` in
        // `register_box`, was removed from the registry while the heap lock
        // was held and is owned by this collector, so its payload is dropped
        // exactly once here.
        unsafe { ObjectPool::drop_object(obj) };
    }

    // ---- Stage E: reset colours and finish ----
    let pool_stats = {
        // A mutator that acquires the heap lock after this must observe
        // `MARKING == false`, so no grey object can be created for a finished
        // cycle. The reset also whitens the grey objects allocated during the
        // sweep (they are live and registered).
        let mut heap = lock_global();
        for &obj in &heap.objects {
            // SAFETY: every registered pointer is live.
            unsafe { (*obj).marked.store(WHITE, Ordering::Release) };
        }
        for &obj in &whites {
            // SAFETY: the payload was dropped above; the cell becomes
            // available for reuse and must not be dereferenced afterwards.
            heap.pool.reclaim(obj);
        }
        MARKING.store(false, Ordering::SeqCst);
        WEAK_FROZEN.store(false, Ordering::SeqCst);
        WEAK_PARTITIONED.store(false, Ordering::SeqCst);
        heap.pool.stats()
    };
    drop(whites);
    // Outside the heap lock: a barrier that pushed between the reset and this
    // clear leaves a harmless stale entry (the next collection starts by
    // clearing the worklist and the object is live and white).
    clear_worklist();
    drop(_collector);

    let sweep_time = sweep_start.elapsed();
    record_collection(pause, mark_time, sweep_time, freed);

    if stats {
        eprintln!(
            "hydro-gc: pause_ms={:.3} mark_ms={:.3} sweep_ms={:.3} freed={} \
             slabs={} capacity={} live={} reused={} precise={} precise_frames={}",
            pause.as_secs_f64() * 1000.0,
            mark_time.as_secs_f64() * 1000.0,
            sweep_time.as_secs_f64() * 1000.0,
            freed,
            pool_stats.slabs,
            pool_stats.capacity,
            pool_stats.live,
            pool_stats.reused,
            if strict { "strict" } else { "mixed" },
            STAT_PRECISE_FRAMES.load(Ordering::Relaxed)
        );
    }
    freed as i64
}

/// Assigns and returns this OS thread's Python-visible id. The first thread
/// to ask is the main thread (id 1); spawned threads get their id from
/// [`new_thread_id`] and install it with [`register_thread`] before running
/// user code.
pub(crate) fn current_thread_id() -> i64 {
    THREAD_ID.with(|cell| {
        let mut id = cell.get();
        if id == 0 {
            id = 1;
            cell.set(id);
        }
        id
    })
}

/// Allocates an id for a new thread. Id 1 is reserved for the main thread.
pub(crate) fn new_thread_id() -> i64 {
    NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed)
}

/// Installs the calling thread's id at the start of a spawned thread and
/// registers it for stop-the-world collection (stack bounds, context,
/// pending mirror, thread roots) before any user code runs.
///
/// `roots` are tagged values the newly spawned thread owns outside its stack
/// (the `_thread.start_new_thread` callable and arguments, which live in the
/// Rust closure until the generated code receives them). They are installed
/// together with the registration, so a collector that can see the thread in
/// the registry already sees its roots and may free neither.
pub(crate) fn register_thread(id: i64, roots: Vec<i64>) {
    THREAD_ID.with(|cell| cell.set(id));
    register_current_thread(id, roots);
}

/// Creates the registration of the calling thread and inserts it into the
/// process-wide table. Called by [`reset`] for the main thread and by
/// [`register_thread`] for spawned threads.
fn register_current_thread(id: i64, roots: Vec<i64>) {
    let (low, high) = stack_bounds().unwrap_or((0, 0));
    let gc = Arc::new(ThreadGc {
        id,
        token: current_gc_token(),
        stack_low: AtomicUsize::new(low),
        stack_high: AtomicUsize::new(high),
        aux_low: AtomicUsize::new(0),
        aux_high: AtomicUsize::new(0),
        aux2_low: AtomicUsize::new(0),
        aux2_high: AtomicUsize::new(0),
        parked: AtomicBool::new(false),
        exited: AtomicBool::new(false),
        saved_sp: AtomicUsize::new(0),
        saved_regs: Mutex::new([0; REG_WORDS]),
        pending: AtomicI64::new(crate::value::NONE),
        handled: Mutex::new(Vec::new()),
        roots: Mutex::new(roots),
        persistent: Mutex::new(Vec::new()),
    });
    GC_SELF.with(|cell| *cell.borrow_mut() = Some(gc.clone()));
    let mut registry = lock_thread_registry();
    registry.insert(id, gc);
    GC_CONDVAR.notify_all();
}

/// Records a new live thread. Called before the OS thread is spawned so the
/// collector already sees the upcoming thread.
pub(crate) fn enter_thread() {
    ACTIVE_THREADS.fetch_add(1, Ordering::AcqRel);
}

/// Undoes [`enter_thread`] when the OS thread could not be spawned. The
/// calling thread keeps its own registration; only the live-thread count is
/// rolled back.
pub(crate) fn leave_unstarted_thread() {
    ACTIVE_THREADS.fetch_sub(1, Ordering::AcqRel);
}

/// Records that a spawned thread has exited: marks it exited, removes it from
/// the collector registry and wakes a waiting collector.
pub(crate) fn leave_thread() {
    // Run callbacks queued for this thread while its registration and stack
    // are still intact. Skip while a collector is stopping the world: the
    // pins keep the references alive until the next drain or reset.
    if !GC_REQUESTED.load(Ordering::SeqCst) && !MARKING.load(Ordering::Acquire) {
        run_pending_weakref_callbacks();
    }
    if let Some(gc) = GC_SELF.with(|cell| cell.borrow_mut().take()) {
        gc.exited.store(true, Ordering::SeqCst);
        let mut registry = lock_thread_registry();
        registry.remove(&gc.id);
        GC_CONDVAR.notify_all();
        // Drop the registry lock before the roots lock, matching the
        // collector's registry-then-roots order. The spawn arguments (tagged
        // values owned by the thread's Rust closure) and the persistent
        // roots are released here.
        drop(registry);
        gc.roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        gc.persistent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
    // No shadow frame survives the thread that owns its slots: drop the
    // active set and the published snapshot so the collector can never see
    // (and dereference) a dangling slot array.
    roots::clear_current();
    ACTIVE_THREADS.fetch_sub(1, Ordering::AcqRel);
}

/// Number of live runtime threads, including the main thread.
pub(crate) fn active_threads() -> usize {
    ACTIVE_THREADS.load(Ordering::Acquire)
}

/// Resets the collector and thread registry in a freshly forked child.
///
/// `fork` only duplicates the calling thread, so every registration of a
/// vanished thread is stale. The child becomes the main thread (id 1), the
/// live-thread count restarts at 1, and any in-flight stop-the-world request
/// is cleared. The heap contents survive the fork and stay shared; only the
/// bookkeeping is rebuilt.
///
/// Fork boundary: a lock (or any other mutex) held by a thread that vanished
/// in the fork stays locked in the child, and a queue whose waiting thread
/// vanished never wakes. Callers must fork from a stable state.
pub(crate) fn after_fork_child() {
    THREAD_ID.with(|id| id.set(1));
    GC_SELF.with(|cell| cell.borrow_mut().take());
    {
        let mut registry = lock_thread_registry();
        // Every other registration belonged to a thread that does not exist
        // in the child. Its published precise roots and persistent roots must
        // not be scanned against the child's address space.
        for gc in registry.values() {
            roots::clear_thread(gc.token);
        }
        registry.clear();
    }
    ACTIVE_THREADS.store(1, Ordering::Release);
    NEXT_THREAD_ID.store(2, Ordering::Release);
    // A child inherits neither the collector nor its mark state: drop any
    // in-flight marking request, the worklist and every colour.
    MARKING.store(false, Ordering::SeqCst);
    WEAK_FROZEN.store(false, Ordering::SeqCst);
    WEAK_PARTITIONED.store(false, Ordering::SeqCst);
    clear_worklist();
    clear_local_worklist();
    // The child has exactly one thread, so every inherited fiber range now
    // belongs to it; re-tagging keeps roots of fibers created on vanished
    // threads scannable.
    let token = current_gc_token();
    for entry in lock_suspended_stacks().iter_mut() {
        entry.0 = token;
    }
    AUX_STACK.with(|slot| slot.set([(0, 0); 2]));
    reset_colors();
    GC_REQUESTED.store(false, Ordering::SeqCst);
    PARKED.store(0, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    register_current_thread(1, Vec::new());
}

/// Turns every registered object white (shared and thread-local registry).
///
/// Used by [`after_fork_child`], where a vanished collector may have left
/// grey/black objects behind that would otherwise never be swept.
fn reset_colors() {
    {
        let heap = lock_global();
        for &obj in &heap.objects {
            // SAFETY: every registered pointer is live.
            unsafe { (*(obj)).marked.store(WHITE, Ordering::Release) };
        }
    }
    HEAP.with(|cell| {
        let heap = cell.borrow();
        for &obj in &heap.objects {
            // SAFETY: every registered pointer is live.
            unsafe { (*(obj)).marked.store(WHITE, Ordering::Release) };
        }
    });
}

/// Marks from all roots and sweeps unmarked objects of the thread-local heap.
fn collect_with_root(extra_root: Option<i64>) -> usize {
    HEAP.with(|cell| {
        let mut heap = cell.borrow_mut();
        collect_state(&mut heap, extra_root)
    })
}

/// Marks from all roots and sweeps unmarked objects of the thread-local
/// heap. Returns the freed count.
///
/// Local mode has exactly one thread, so no barrier, graph-lock contention or
/// stop-the-world coordination is involved: roots are marked grey, the
/// worklist is drained and white objects are swept.
///
/// If the native stack bounds cannot be determined, or the current stack
/// pointer is outside them, the collection is skipped: leaking is always
/// preferable to freeing a reachable object.
fn collect_state(heap: &mut HeapState, extra_root: Option<i64>) -> usize {
    let marker: usize = 0;
    let sp = &marker as *const usize as usize;

    let Some((stack_low, stack_high)) = stack_bounds() else {
        return 0;
    };
    if sp < stack_low || sp >= stack_high {
        return 0;
    }

    let started = Instant::now();
    clear_local_worklist();
    let mark_start = Instant::now();
    // This collector runs on the mutator thread, so its precise roots are
    // read directly (no publication needed).
    let precise = roots::snapshot_current();
    let precise_on = precise_enabled();
    let exclusions = if !precise_on {
        Vec::new()
    } else {
        let mut extents: Vec<(usize, usize)> = Vec::new();
        for frame in precise.frames() {
            if frame.high > frame.low {
                extents.push((frame.low, frame.high));
            }
        }
        for range in precise.ranges() {
            let low = range.base as usize;
            let high = low.saturating_add(range.count.saturating_mul(8));
            if high > low {
                extents.push((low, high));
            }
        }
        extents.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(extents.len());
        for (low, high) in extents {
            match merged.last_mut() {
                Some(last) if low <= last.1 => last.1 = last.1.max(high),
                _ => merged.push((low, high)),
            }
        }
        merged
    };
    let strict = precise_mode();
    {
        let addresses = &heap.addresses;

        if let Some(root) = extra_root {
            mark_word(root as usize, addresses);
        }
        // The pending exception is a thread-local root.
        mark_word(exc::pending_value() as usize, addresses);
        // Precise frames/ranges of the frames currently on this stack.
        if precise_on {
            precise.mark(|value| mark_word(value as usize, addresses));
            STAT_PRECISE_FRAMES.store(precise.frames().len() as u64, Ordering::Relaxed);
        }
        // Rust-side owners registered through `add_thread_root` (the unittest
        // class registry) and persistent roots are invisible to the stack
        // scan in local mode too.
        if let Some(gc) = current_gc() {
            let roots = gc
                .roots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for &root in roots.iter() {
                mark_word(root as usize, addresses);
            }
            let handled = gc
                .handled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for &value in handled.iter() {
                mark_word(value as usize, addresses);
            }
            let persistent = gc
                .persistent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for &root in persistent.iter() {
                mark_word(root as usize, addresses);
            }
        }

        if !strict {
            scan_stack_excluding(sp, stack_high, addresses, &exclusions);
            mark_captured_registers(addresses);
            scan_aux_stack(addresses, &exclusions);
            scan_suspended_stacks(addresses, &exclusions);
        }
    }
    // `getweakrefs` pins the references it is about to return; keep them and
    // their (native-only) children while the result list is being built.
    mark_weak_pins(&heap.addresses);
    drain_worklist();

    // ---- sweep -----------------------------------------------------
    // Clear weak targets that did not survive and drop entries whose
    // reference object did not survive, before any white cell is released.
    // References with an armed callback are resurrected so the callback can
    // receive them after the collection.
    for addr in clear_weakrefs_local(heap) {
        mark_grey(addr);
    }
    drain_worklist();
    let mark_time = mark_start.elapsed();
    let sweep_start = Instant::now();
    let freed = sweep(heap);
    let sweep_time = sweep_start.elapsed();
    heap.last_collection_objects = heap.objects.len();
    // Local mode has no separate stop-the-world window: the whole collection
    // runs with the only thread stopped, so `pause` is the total time.
    record_collection(started.elapsed(), mark_time, sweep_time, freed);
    freed
}

/// Whether a registry allocation is grossly oversized and worth giving back.
///
/// Collections are normally followed by roughly the same number of
/// allocations, so the surviving allocation is reused and returning it would
/// only cost a reallocation later. Only a collapsed live set (or a full
/// `free_all`) leaves a capacity this far above the current length.
fn capacity_is_oversized(capacity: usize, len: usize) -> bool {
    capacity > len.saturating_mul(4).max(1024)
}

/// Frees all white registered objects and keeps the address index in sync.
///
/// Only used by a single-threaded (local-heap) collection, where the sweep can
/// release each cell into the pool in place. The shared collector partitions
/// first and drops payloads outside the heap lock (see [`partition_whites`]).
///
/// Grey objects are kept as well (defensively: with the worklist drained they
/// should not exist) and turned white together with the black ones, so the
/// next collection starts from a clean heap.
///
/// The address index is rebuilt in place after the sweep: its allocation is
/// cleared and refilled from the surviving registry, so the cost is linear in
/// the live set instead of a hash removal per freed cell. Both registries keep
/// their allocation unless it is grossly oversized relative to the live set.
fn sweep(heap: &mut HeapState) -> usize {
    let mut freed = 0usize;
    let HeapState {
        objects,
        addresses,
        pool,
        ..
    } = heap;
    objects.retain(|&obj| {
        // SAFETY: `obj` is a live registered object. White objects are
        // unreachable and released exactly once; every surviving colour
        // is reset to white.
        let color = unsafe { (*obj).marked.load(Ordering::Acquire) };
        if color == WHITE {
            // SAFETY: as above; the cell goes back to the free list.
            unsafe { pool.release(obj) };
            freed += 1;
            false
        } else {
            unsafe { (*obj).marked.store(WHITE, Ordering::Release) };
            true
        }
    });

    // Rebuild the index from the survivors. The existing allocation is reused
    // (`reserve` is a no-op when the old capacity already covers the smaller
    // live set), so no collection path allocates a fresh set.
    addresses.clear();
    addresses.reserve(objects.len());
    addresses.extend(objects.iter().map(|&obj| obj as usize));

    if capacity_is_oversized(objects.capacity(), objects.len()) {
        objects.shrink_to_fit();
    }
    if capacity_is_oversized(addresses.capacity(), addresses.len()) {
        addresses.shrink_to_fit();
    }

    freed
}

/// Removes every white object from the shared registry and hands it back for
/// the caller to drop.
///
/// Runs under the heap lock but with the mutators running. Only the collector
/// removes registry entries, and the returned objects are unreachable: the
/// final rescan ran with every other thread parked, and any edge stored
/// afterwards marked its child grey through [`write_barrier_edge`], so no live
/// object can point at a white one. Survivors are reset to white so the next
/// cycle starts from a clean heap; objects allocated during the sweep are
/// grey and survive as well.
///
/// No cell is released here: the caller drops the payloads outside every
/// lock, so releasing a large payload never blocks an allocator or a mutator,
/// and returns the cells to the pool in the brief final heap-lock window.
fn partition_whites(heap: &mut HeapState) -> Vec<*mut HydroObj> {
    let objects = std::mem::take(&mut heap.objects);
    let mut whites = Vec::new();
    let mut live = Vec::with_capacity(objects.len());
    for obj in objects {
        // SAFETY: every registered pointer came from `ObjectPool::allocate`
        // in `register_box` and is live. Colours are changed at runtime only
        // by this collector and by the barrier colouring a newly allocated
        // object grey; the load is atomic, so the partition sees one of them.
        let color = unsafe { (*obj).marked.load(Ordering::Acquire) };
        if color == WHITE {
            whites.push(obj);
        } else {
            // SAFETY: as above; the object survives this collection.
            unsafe { (*obj).marked.store(WHITE, Ordering::Release) };
            live.push(obj);
        }
    }
    heap.objects = live;
    heap.objects.shrink_to_fit();

    let mut addresses =
        AddressSet::with_capacity_and_hasher(heap.objects.len(), BuildHasherDefault::default());
    addresses.extend(heap.objects.iter().map(|&obj| obj as usize));
    heap.addresses = addresses;
    heap.addresses.shrink_to_fit();

    whites
}

/// Marks `word` as a root if it looks like one of our tagged heap pointers.
fn mark_word(word: usize, addresses: &AddressSet) {
    if word & TAG_MASK as usize != TAG_HEAP as usize {
        return;
    }
    let addr = word & !(TAG_MASK as usize);
    if addr == 0 || !addresses.contains(&addr) {
        // Not a registered object: a conservative false positive at worst.
        return;
    }
    mark_grey(addr);
}

/// Traces the children of a grey object and turns it black.
///
/// Mutable containers (list/dict/set) are read under the graph lock and
/// queues under their item mutex; the black transition happens *inside* that
/// critical section so a concurrent mutation cannot slip between "children
/// read" and "black" and lose its insertion barrier. Tuples, iterator targets,
/// thread/process targets and arguments are immutable and need no lock.
///
/// # Safety
///
/// `addr` must denote a live `HydroObj`.
unsafe fn trace(addr: usize) {
    let obj = &*(addr as *const HydroObj);
    match obj.kind {
        crate::value::KIND_LIST
        | crate::value::KIND_DICT
        | crate::value::KIND_SET
        | crate::value::KIND_DEFAULTDICT
        | crate::value::KIND_ARG_PARSER
        | crate::value::KIND_NAMESPACE
        | crate::value::KIND_STR_BUILDER
        | crate::value::KIND_CELL => {
            let _graph = graph_lock();
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            match obj.data_ref() {
                ObjDataRef::List(items) => {
                    for &item in items {
                        mark_value(item);
                    }
                }
                ObjDataRef::Dict(dict) => {
                    for &(key, value) in dict.entries() {
                        mark_value(key);
                        mark_value(value);
                    }
                }
                ObjDataRef::Set(items) => {
                    for &item in items {
                        mark_value(item);
                    }
                }
                ObjDataRef::DefaultDict(state) => {
                    if state.factory != crate::value::NONE {
                        mark_value(state.factory);
                    }
                    for &(key, item) in &state.pairs {
                        mark_value(key);
                        mark_value(item);
                    }
                }
                ObjDataRef::ArgParser(parser) => {
                    if parser.formatter_class != crate::value::NONE {
                        mark_value(parser.formatter_class);
                    }
                    if parser.argument_default != crate::value::NONE {
                        mark_value(parser.argument_default);
                    }
                    for spec in &parser.options {
                        if spec.type_fn != crate::value::NONE {
                            mark_value(spec.type_fn);
                        }
                        if spec.default != crate::value::NONE {
                            mark_value(spec.default);
                        }
                        if spec.const_value != crate::value::NONE {
                            mark_value(spec.const_value);
                        }
                        if let Some(choices) = &spec.choices {
                            for &choice in choices {
                                mark_value(choice);
                            }
                        }
                    }
                    for &(_, item) in &parser.defaults {
                        mark_value(item);
                    }
                    if let Some(subparsers) = &parser.subparsers {
                        for &(_, child, _) in &subparsers.parsers {
                            mark_value(child);
                        }
                    }
                }
                ObjDataRef::Namespace(namespace) => {
                    for &(_, item) in &namespace.pairs {
                        mark_value(item);
                    }
                }
                ObjDataRef::Cell(value) => mark_value(value),
                // A builder only holds a tagged value after the generic
                // concatenation path; the growing text is native state.
                ObjDataRef::StrBuilder(crate::value::StrBuilderState::Materialized(value)) => {
                    mark_value(*value);
                }
                ObjDataRef::StrBuilder(crate::value::StrBuilderState::Building(_)) => {}
                _ => {}
            }
            mark_black(addr);
        }
        crate::value::KIND_QUEUE => {
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            let ObjDataRef::Queue(queue) = obj.data_ref() else {
                mark_black(addr);
                return;
            };
            // Blocking queue operations release this mutex before parking and
            // never allocate while holding it, so it is available here. The
            // black transition stays inside the critical section.
            let items = queue.trace_items();
            for &item in items.iter() {
                mark_value(item);
            }
            mark_black(addr);
        }
        crate::value::KIND_CLASS => {
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            let ObjDataRef::Class(state) = obj.data_ref() else {
                mark_black(addr);
                return;
            };
            // The frozen namespace is immutable, so only the overlay needs the
            // lock: `class_set_attr` publishes its insertion barrier under the
            // same mutex, so an edge added during marking cannot be lost.
            let overlay = state.lock_overlay();
            mark_value(state.base);
            for &base in &state.bases {
                mark_value(base);
            }
            for &ancestor in &state.mro {
                mark_value(ancestor);
            }
            if state.metaclass != crate::value::NONE {
                mark_value(state.metaclass);
            }
            for &(_, function, _) in &state.methods {
                mark_value(function);
            }
            for &(_, value) in &state.attrs {
                mark_value(value);
            }
            for &(_, value) in overlay.iter() {
                mark_value(value);
            }
            drop(overlay);
            mark_black(addr);
        }
        crate::value::KIND_INSTANCE => {
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            let ObjDataRef::Instance(state) = obj.data_ref() else {
                mark_black(addr);
                return;
            };
            // Hold the attribute mutex until the instance is black:
            // `Attributes::set` publishes its insertion barrier under the same
            // lock, so an edge added during marking cannot be lost between
            // trace and black.
            let attrs = state.lock_attrs();
            mark_value(state.class);
            for &(_, value) in attrs.iter() {
                mark_value(value);
            }
            // A weak reference holds its callback as a strong edge even if a
            // user overwrote `__callback__`; the side table is authoritative.
            mark_weakref_callback(addr);
            mark_black(addr);
        }
        _ => {
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            match obj.data_ref() {
                // An iterator's target is immutable; its index belongs to the
                // owning loop and is not a GC edge.
                ObjDataRef::Iter(state) => mark_value(state.target),
                ObjDataRef::Tuple(items) => {
                    for &item in items {
                        mark_value(item);
                    }
                }
                // Template payloads are immutable after construction, so their
                // literal strings and interpolations are stable GC edges; an
                // interpolation traces its value and format-spec string.
                ObjDataRef::Template(state) => {
                    for &string in &state.strings {
                        mark_value(string);
                    }
                    for &interpolation in &state.interpolations {
                        mark_value(interpolation);
                    }
                }
                ObjDataRef::Interpolation(state) => {
                    mark_value(state.value);
                    mark_value(state.format_spec);
                }
                // A view observes its exporter live, so the exporter is a
                // stable edge: `release()` only drops it.
                ObjDataRef::MemoryView(state) => mark_value(state.buffer),
                // A dict view is live over its dict.
                ObjDataRef::DictView(state) => mark_value(state.dict),
                ObjDataRef::Super(state) => {
                    mark_value(state.class);
                    mark_value(state.object);
                }
                // A user-defined exception traces its class identity, the
                // exception chaining edges of its native payload and its
                // instance attribute table (held under the mutex until black,
                // like a normal instance).
                ObjDataRef::ExcInstance(state) => {
                    let attrs = state.lock_attrs();
                    mark_value(state.class);
                    if let Some(cause) = state.data.cause {
                        mark_value(cause);
                    }
                    if let Some(context) = state.data.context {
                        mark_value(context);
                    }
                    for &(_, value) in attrs.iter() {
                        mark_value(value);
                    }
                    drop(attrs);
                }
                // A bytearray holds native bytes only.
                ObjDataRef::ByteArray(_) => {}
                ObjDataRef::Thread(thread) => {
                    mark_value(thread.target);
                    for &arg in &thread.args {
                        mark_value(arg);
                    }
                }
                ObjDataRef::Process(process) => {
                    mark_value(process.target);
                    for &arg in &process.args {
                        mark_value(arg);
                    }
                }
                ObjDataRef::Pool(pool) => {
                    // The queue vectors are immutable after construction.
                    for &queue in &pool.task_queues {
                        mark_value(queue);
                    }
                    for &queue in &pool.result_queues {
                        mark_value(queue);
                    }
                }
                ObjDataRef::CsvWriter(writer) => mark_value(writer.file),
                // A fiber owns its call arguments and result; its suspended
                // stack is registered as a separate root range. Join requests
                // reference the gathered tasks.
                ObjDataRef::Fiber(fiber) => {
                    fiber.trace(mark_value);
                }
                // Wrapper functions own their captured values; item getters
                // own their keys and partials own the callable and bound
                // arguments. Names of attribute getters are plain strings.
                ObjDataRef::Function(function) => {
                    for param in &function.signature {
                        if let Some(default) = param.default {
                            mark_value(default);
                        }
                    }
                    match &function.body {
                        FunctionBody::Wrapper { captures, .. } => {
                            for &capture in captures {
                                mark_value(capture);
                            }
                        }
                        FunctionBody::ItemGetter { keys } => {
                            for &key in keys {
                                mark_value(key);
                            }
                        }
                        FunctionBody::Partial {
                            function: inner,
                            args,
                            keywords,
                        } => {
                            mark_value(*inner);
                            for &arg in args {
                                mark_value(arg);
                            }
                            for (_, value) in keywords {
                                mark_value(*value);
                            }
                        }
                        FunctionBody::BoundMethod { receiver, .. } => {
                            mark_value(*receiver);
                        }
                        // Interpreter functions own their closure captures.
                        FunctionBody::Host { captures, .. } => {
                            for &capture in captures {
                                mark_value(capture);
                            }
                        }
                        FunctionBody::AttributeGetter { .. }
                        | FunctionBody::Identity
                        | FunctionBody::Native { .. }
                        | FunctionBody::Builtin(_) => {}
                    }
                }
                // The explicit cause (`raise X from Y`) and the implicit
                // context are GC edges; `from None` stores no cause but may
                // keep the handled exception as a context.
                ObjDataRef::Exception(exception) => {
                    if let Some(cause) = exception.cause {
                        mark_value(cause);
                    }
                    if let Some(context) = exception.context {
                        mark_value(context);
                    }
                }
                // Scalars, ranges, locks, events, cross-process
                // queues, files, csv readers, paths, compiled patterns and
                // matches hold no Hydro values.
                ObjDataRef::Str(_)
                | ObjDataRef::Float(_)
                | ObjDataRef::Range { .. }
                | ObjDataRef::Lock(_)
                | ObjDataRef::Event(_)
                | ObjDataRef::MpQueue(_)
                | ObjDataRef::File(_)
                | ObjDataRef::Bytes(_)
                | ObjDataRef::Random(_)
                | ObjDataRef::Path(_)
                | ObjDataRef::Pattern(_)
                | ObjDataRef::Match(_)
                | ObjDataRef::Socket(_)
                | ObjDataRef::CsvReader(_)
                | ObjDataRef::ExtFunction(_)
                | ObjDataRef::Capsule { .. }
                | ObjDataRef::Module(_)
                | ObjDataRef::StdStream(_)
                | ObjDataRef::BigInt(_) => {}
                // A string accumulator is traced with its own kind above,
                // where the graph lock protects its materialised edge.
                ObjDataRef::StrBuilder(_) => {}
                // The mutable containers were handled above; they cannot
                // appear here.
                ObjDataRef::List(_)
                | ObjDataRef::Dict(_)
                | ObjDataRef::Set(_)
                | ObjDataRef::DefaultDict(_)
                | ObjDataRef::ArgParser(_)
                | ObjDataRef::Namespace(_)
                | ObjDataRef::Queue(_)
                | ObjDataRef::Class(_)
                | ObjDataRef::Instance(_)
                | ObjDataRef::Cell(_) => {}
            }
            mark_black(addr);
        }
    }
}

/// Conservatively scans the native caller range of a running fiber, if any.
///
/// `exclude` holds the merged byte extents of the precise shadow frames and
/// ranges of the collection, so their slots are marked exactly instead of
/// being scanned again here.
fn scan_aux_stack(addresses: &AddressSet, exclude: &[(usize, usize)]) {
    for (low, high) in AUX_STACK.with(Cell::get) {
        if high > low {
            scan_stack_excluding(low, high, addresses, exclude);
        }
    }
}

/// Conservatively scans every lifetime-registered fiber root range of this
/// thread (private stack and both register contexts, including the running
/// fiber's own stack).
fn scan_suspended_stacks(addresses: &AddressSet, exclude: &[(usize, usize)]) {
    let owner = current_gc_token();
    let ranges: Vec<(usize, usize)> = lock_suspended_stacks()
        .iter()
        .filter(|&&(id, _, _)| id == owner)
        .map(|&(_, low, high)| (low, high))
        .collect();
    for (low, high) in ranges {
        if high > low {
            scan_stack_excluding(low, high, addresses, exclude);
        }
    }
}

/// Conservatively scans the native stack from `sp` (inclusive) up to `high`
/// (exclusive). Every aligned word is inspected; only tagged values whose
/// object is registered are marked.
///
/// `exclude` is a sorted, merged list of half-open byte ranges that belong to
/// precise frames/ranges. Those bytes are skipped: the precise marker already
/// visited every live slot there, so a scan would only see dead temporaries
/// the shadow frame has stopped reporting. The exclusion list is consumed
/// with a monotonic index that advances alongside `cursor`, keeping the scan
/// linear.
fn scan_stack_excluding(
    sp: usize,
    high: usize,
    addresses: &AddressSet,
    exclude: &[(usize, usize)],
) {
    let word_size = std::mem::size_of::<usize>();
    let mut cursor = sp & !(std::mem::align_of::<usize>() - 1);
    let mut excluded = 0usize;
    while cursor + word_size <= high {
        while excluded < exclude.len() && exclude[excluded].1 <= cursor {
            excluded += 1;
        }
        if let Some(&(low, end)) = exclude.get(excluded) {
            if low <= cursor && cursor < end {
                // Jump past the whole precise extent; `end` is aligned by
                // construction (slot arrays are `i64`-aligned).
                cursor = end;
                continue;
            }
        }
        // SAFETY: `cursor` lies inside the current thread's stack mapping
        // (bounds obtained from `pthread_attr_getstack`); the load is a plain
        // machine word read that is opaque to the compiler, so it cannot
        // assume the stack slot holds an initialized `usize`.
        let word = unsafe { load_stack_word(cursor) };
        mark_word(word, addresses);
        cursor += word_size;
    }
}

/// Reads one machine word from `addr` without materializing a Rust reference.
///
/// Stack slots above the current stack pointer may be uninitialized. Reading
/// them via `ptr::read_unaligned::<usize>` would be undefined behavior, so the
/// load is expressed as inline assembly: a machine load whose result is
/// opaque to the compiler. The instruction does not write memory and preserves
/// flags. The compiler may allocate `value` and `addr` to the same register;
/// the resulting self-load is harmless.
///
/// # Safety
///
/// `addr` must point to readable memory for one word, which the caller
/// guarantees by staying inside the thread's stack mapping.
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn load_stack_word(addr: usize) -> usize {
    let value: usize;
    // SAFETY: `addr` is readable (caller contract); the asm only loads one
    // word, touches no stack and preserves flags.
    unsafe {
        std::arch::asm!(
            "mov {}, [{}]",
            out(reg) value,
            in(reg) addr,
            options(readonly, nostack, preserves_flags)
        );
    }
    value
}

/// AArch64 variant of [`load_stack_word`].
///
/// # Safety
///
/// Same contract as the x86_64 variant: `addr` must be readable for one word.
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn load_stack_word(addr: usize) -> usize {
    let value: usize;
    // SAFETY: `addr` is readable (caller contract); the asm only loads one
    // word, touches no stack and preserves flags.
    unsafe {
        std::arch::asm!(
            "ldr {}, [{}]",
            out(reg) value,
            in(reg) addr,
            options(readonly, nostack, preserves_flags)
        );
    }
    value
}

/// Fallback for architectures without an inline-asm word load.
///
/// # Safety
///
/// Same contract as the x86_64 variant. These targets are unsupported run
/// targets and accept the theoretical uninitialized-memory caveat.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline]
unsafe fn load_stack_word(addr: usize) -> usize {
    unsafe { std::ptr::read_unaligned(addr as *const usize) }
}

// Stack bounds

/// Redirects the collector's stack bounds to `low..high` while a fiber runs.
///
/// The override is a stack: a nested resume pushes the inner fiber's range on
/// top of its caller's. In shared mode the thread registration's bounds are
/// updated as well, so a safepoint reached on a fiber stack saves a context
/// the shared collector can validate.
pub(crate) fn push_stack_bounds(low: usize, high: usize) {
    STACK_BOUNDS_OVERRIDE.with(|bounds| {
        let mut bounds = bounds.borrow_mut();
        if bounds.is_empty() {
            let base = native_stack_bounds().unwrap_or((0, 0));
            bounds.push(base);
        }
        bounds.push((low, high));
    });
    update_thread_bounds(low, high);
}

/// Undoes one [`push_stack_bounds`], restoring the enclosing context's bounds.
pub(crate) fn pop_stack_bounds() {
    let restored = STACK_BOUNDS_OVERRIDE.with(|bounds| {
        let mut bounds = bounds.borrow_mut();
        if bounds.len() > 1 {
            bounds.pop();
        }
        bounds.last().copied().unwrap_or((0, 0))
    });
    let restored = if restored == (0, 0) {
        native_stack_bounds().unwrap_or((0, 0))
    } else {
        restored
    };
    update_thread_bounds(restored.0, restored.1);
}

/// Runs `f` with the calling thread's collector registration, without the
/// `Arc` clone of [`current_gc`] (this is on the per-resume path).
fn with_current_gc(f: impl FnOnce(&ThreadGc)) {
    GC_SELF.with(|cell| {
        if let Some(gc) = cell.borrow().as_ref() {
            f(gc);
        }
    });
}

fn update_thread_bounds(low: usize, high: usize) {
    // Only the shared collector validates a parked thread against its saved
    // bounds; local mode reads the thread-local override directly.
    if !shared() {
        return;
    }
    with_current_gc(|gc| {
        gc.stack_low.store(low, Ordering::Release);
        gc.stack_high.store(high, Ordering::Release);
    });
}

/// Publishes the two extra root ranges of the outermost fiber resume.
///
/// While the fiber runs, the resumer's native stack frames (`low..high`) and
/// the resumed fiber's `caller_ctx` (which the context switch fills with the
/// resumer's callee-saved registers) are live but are not on the current
/// stack. Both are scanned conservatively. Nested resumes do not publish
/// anything: their caller is a fiber whose lifetime stack and context ranges
/// are already registered.
pub(crate) fn set_aux_stack(low: usize, high: usize, ctx_low: usize, ctx_high: usize) {
    let ranges = [(low, high), (ctx_low, ctx_high)];
    AUX_STACK.with(|slot| slot.set(ranges));
    if !shared() {
        return;
    }
    with_current_gc(|gc| {
        gc.aux_low.store(ranges[0].0, Ordering::Release);
        gc.aux_high.store(ranges[0].1, Ordering::Release);
        gc.aux2_low.store(ranges[1].0, Ordering::Release);
        gc.aux2_high.store(ranges[1].1, Ordering::Release);
    });
}

/// Removes the aux ranges published by [`set_aux_stack`].
pub(crate) fn clear_aux_stack() {
    AUX_STACK.with(|slot| slot.set([(0, 0); 2]));
    if !shared() {
        return;
    }
    with_current_gc(|gc| {
        gc.aux_low.store(0, Ordering::Release);
        gc.aux_high.store(0, Ordering::Release);
        gc.aux2_low.store(0, Ordering::Release);
        gc.aux2_high.store(0, Ordering::Release);
    });
}

/// Publishes `low..high` as a conservative GC root range for the lifetime of
/// a fiber object (its private stack and its two register contexts).
/// Registered once at creation and removed in `Drop`; the per-resume path
/// does not touch this registry.
pub(crate) fn register_suspended_stack(low: usize, high: usize) {
    if low == 0 || high <= low {
        return;
    }
    let owner = current_gc_token();
    lock_suspended_stacks().push((owner, low, high));
}

/// Removes a range registered by [`register_suspended_stack`].
pub(crate) fn unregister_suspended_stack(low: usize, high: usize) {
    if low == 0 || high <= low {
        return;
    }
    let mut stacks = lock_suspended_stacks();
    if let Some(index) = stacks
        .iter()
        .rposition(|&(_, lo, hi)| lo == low && hi == high)
    {
        stacks.remove(index);
    }
}

/// The calling thread's effective stack bounds: the innermost fiber override
/// when one is active, the native stack otherwise.
pub(crate) fn stack_bounds() -> Option<(usize, usize)> {
    match STACK_BOUNDS_OVERRIDE.with(|bounds| bounds.borrow().last().copied()) {
        Some(bounds) if bounds != (0, 0) => Some(bounds),
        _ => native_stack_bounds(),
    }
}

/// The OS thread's native stack bounds, ignoring any fiber override.
#[cfg(unix)]
fn native_stack_bounds() -> Option<(usize, usize)> {
    // SAFETY: `attr` is initialized by `pthread_getattr_np`, queried with a
    // valid stackt pointer pair and destroyed exactly once. Failures are
    // reported as `None`; no pointers escape.
    unsafe {
        let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
        if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
            return None;
        }
        // SAFETY: on success `pthread_getattr_np` initialized the attribute.
        let mut attr = attr.assume_init();
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let result = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        if result != 0 || addr.is_null() || size == 0 {
            return None;
        }
        let low = addr as usize;
        Some((low, low.saturating_add(size)))
    }
}

#[cfg(not(unix))]
fn native_stack_bounds() -> Option<(usize, usize)> {
    // Without a portable stack-bounds API the collector conservatively
    // declines to run (objects are simply leaked).
    None
}

// Register dump

/// Marks every word of the register dump returned by [`spill_registers`].
fn mark_captured_registers(addresses: &AddressSet) {
    for register in spill_registers() {
        mark_word(register, addresses);
    }
}

/// Dumps all general-purpose registers of the calling convention into a
/// stack-local buffer using a single `asm!` block.
///
/// Invariant: every value that only lives in a register at collection time is
/// preserved. Either one of the stores captures it verbatim, or the compiler
/// chose that register to hold `regs.as_mut_ptr()`. In the latter case the
/// register's previous live value must have been spilled to the stack before
/// the asm block (the pointer is an input operand, so the value is live up to
/// that point), and [`scan_stack_excluding`] covers that spill slot. All
/// other GPRs are captured verbatim. Because the whole buffer is marked, no
/// register needs special casing.
///
/// No clobbers are declared: the block only reads registers and writes the
/// buffer through the pointer. Default asm options are kept so the memory
/// writes are modeled for the optimizer.
#[cfg(target_arch = "x86_64")]
fn spill_registers() -> [usize; 16] {
    let mut regs = [0usize; 16];
    // SAFETY: the block stores 16 word-sized registers into the 16-word local
    // buffer; its address is valid, aligned and writable. No other memory is
    // accessed. The registers read are the live ones at this point, which is
    // exactly the intent.
    unsafe {
        std::arch::asm!(
            "mov [{out} + 0], rax",
            "mov [{out} + 8], rbx",
            "mov [{out} + 16], rcx",
            "mov [{out} + 24], rdx",
            "mov [{out} + 32], rsi",
            "mov [{out} + 40], rdi",
            "mov [{out} + 48], rbp",
            "mov [{out} + 56], rsp",
            "mov [{out} + 64], r8",
            "mov [{out} + 72], r9",
            "mov [{out} + 80], r10",
            "mov [{out} + 88], r11",
            "mov [{out} + 96], r12",
            "mov [{out} + 104], r13",
            "mov [{out} + 112], r14",
            "mov [{out} + 120], r15",
            out = in(reg) regs.as_mut_ptr(),
        );
    }
    regs
}

/// AArch64 variant of [`spill_registers`]: `x0..x30` and `sp` into 32 words.
///
/// The same invariant applies: a register selected to hold the buffer pointer
/// has its live value spilled to the stack by the compiler.
#[cfg(target_arch = "aarch64")]
fn spill_registers() -> [usize; 32] {
    let mut regs = [0usize; 32];
    // SAFETY: the block stores 32 word-sized registers into the 32-word local
    // buffer; its address is valid, aligned and writable. No other memory is
    // accessed. The registers read are the live ones at this point, which is
    // exactly the intent.
    unsafe {
        std::arch::asm!(
            "str x0, [{out}, #0]",
            "str x1, [{out}, #8]",
            "str x2, [{out}, #16]",
            "str x3, [{out}, #24]",
            "str x4, [{out}, #32]",
            "str x5, [{out}, #40]",
            "str x6, [{out}, #48]",
            "str x7, [{out}, #56]",
            "str x8, [{out}, #64]",
            "str x9, [{out}, #72]",
            "str x10, [{out}, #80]",
            "str x11, [{out}, #88]",
            "str x12, [{out}, #96]",
            "str x13, [{out}, #104]",
            "str x14, [{out}, #112]",
            "str x15, [{out}, #120]",
            "str x16, [{out}, #128]",
            "str x17, [{out}, #136]",
            "str x18, [{out}, #144]",
            "str x19, [{out}, #152]",
            "str x20, [{out}, #160]",
            "str x21, [{out}, #168]",
            "str x22, [{out}, #176]",
            "str x23, [{out}, #184]",
            "str x24, [{out}, #192]",
            "str x25, [{out}, #200]",
            "str x26, [{out}, #208]",
            "str x27, [{out}, #216]",
            "str x28, [{out}, #224]",
            "str x29, [{out}, #232]",
            "str x30, [{out}, #240]",
            "str sp, [{out}, #248]",
            out = in(reg) regs.as_mut_ptr(),
        );
    }
    regs
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn spill_registers() -> [usize; 0] {
    // Unknown calling convention: rely on the stack scan only.
    []
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exc;
    use crate::value::KIND_STR;

    /// Serialises the stats tests: they inspect process-wide counters, and
    /// `reset_clears_counters` must not zero them while another stats test is
    /// measuring a delta. Other unit tests may still collect concurrently;
    /// the counters only ever grow, which the assertions tolerate.
    static STATS_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn stats_test_guard() -> MutexGuard<'static, ()> {
        STATS_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn collect_increments_counters() {
        let _guard = stats_test_guard();
        let before = gc_stats();
        let _ = collect_with_root(None);
        let after = gc_stats();
        assert!(after.collections > before.collections);
        assert!(after.pause_total_us >= before.pause_total_us);
        assert!(after.mark_total_us >= before.mark_total_us);
        assert!(after.sweep_total_us >= before.sweep_total_us);
        // The maximum can never exceed the accumulated total.
        assert!(after.pause_max_us <= after.pause_total_us);
    }

    #[test]
    fn snapshot_reports_live_objects() {
        let _guard = stats_test_guard();
        let tagged = allocate(KIND_STR, ObjData::Str(String::from("live")));
        let stats = gc_stats();
        // The object was registered on this thread's heap and stays reachable
        // through `tagged` for the rest of the test.
        assert!(stats.live_objects >= 1);
        assert_eq!(crate::value::as_str(tagged), Some("live"));
    }

    #[test]
    fn module_values_have_no_children_and_survive_collection() {
        let _guard = stats_test_guard();
        let module = crate::value::module_value(String::from("unittest"));
        // The module holds no Hydro edges; a collection with it as an explicit
        // root must neither trace a child nor free the object itself.
        let _ = collect_with_root(Some(module));
        assert_eq!(crate::value::type_name(module), "module");
        assert_eq!(
            crate::value::as_module(module).map(|state| state.name()),
            Some("unittest")
        );
    }

    /// Untags a heap value into the raw cell address used by the registries.
    fn cell_address(value: i64) -> usize {
        (value & !TAG_MASK) as usize
    }

    /// Marks a registered cell black so the sweep keeps it.
    fn mark_survivor(value: i64) {
        mark_cell(value, BLACK);
    }

    /// Forces a cell's colour (tests that inspect the weak pass directly must
    /// not depend on a parallel collector's `MARKING` state).
    fn mark_cell(value: i64, color: u8) {
        // SAFETY: the caller registered the object in its own heap.
        unsafe {
            object_at(cell_address(value))
                .marked
                .store(color, Ordering::Release)
        };
    }

    /// Registers `label`'s string object in `heap` and returns its address.
    fn register_test_object(heap: &mut HeapState, label: &str) -> usize {
        let tagged = register_box(heap, KIND_STR, ObjData::Str(String::from(label)));
        cell_address(tagged)
    }

    /// Serialises and clears the process-wide weak table for one test.
    fn weak_guard() -> MutexGuard<'static, ()> {
        let guard = WEAK_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        weakref_tests_clear();
        CALLBACK_CALLS.store(0, Ordering::SeqCst);
        CALLBACK_LAST.store(0, Ordering::SeqCst);
        guard
    }

    static CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CALLBACK_LAST: AtomicI64 = AtomicI64::new(0);

    unsafe extern "C" fn counting_callback(argv: *const i64) -> i64 {
        CALLBACK_CALLS.fetch_add(1, Ordering::SeqCst);
        if !argv.is_null() {
            // SAFETY: the caller passes one readable tagged argument.
            CALLBACK_LAST.store(unsafe { *argv }, Ordering::SeqCst);
        }
        crate::value::NONE
    }

    fn counting_callback_value() -> i64 {
        let entry: unsafe extern "C" fn(*const i64) -> i64 = counting_callback;
        crate::value::function_new_with_flags(entry as usize, 1, "weakref_callback".to_string(), 0)
    }

    /// The colour of a registered cell.
    fn color(value: i64) -> u8 {
        // SAFETY: the caller registered the object in a live heap.
        unsafe {
            object_at(cell_address(value))
                .marked
                .load(Ordering::Acquire)
        }
    }

    /// Registers a weakref directly in the side table as a private-heap entry,
    /// independent of whether another test has switched the process to shared
    /// mode or has a collection in flight.
    fn register_local_weak(reference: i64, target: i64) -> bool {
        let ref_addr = cell_address(reference);
        let target_addr = cell_address(target);
        if ref_addr == 0 || target_addr == 0 {
            return false;
        }
        lock_weak().refs.insert(
            ref_addr,
            WeakEntry {
                target: target_addr,
                callback: crate::value::NONE,
                shared: false,
            },
        );
        true
    }

    #[test]
    fn weak_reference_does_not_trace_its_target() {
        let _guard = weak_guard();
        let mut heap = HeapState::new();
        let target = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let reference = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        assert!(register_local_weak(reference, target));
        assert_eq!(weakref_target_raw(reference), Some(target));

        // Tracing the reference must not reach the target through the side
        // table: the weak edge is not part of the object graph.
        mark_cell(target, WHITE);
        mark_cell(reference, GREY);
        // SAFETY: both cells were just registered in `heap` and the reference
        // is grey; trace it directly (no worklist shared with a possibly
        // concurrent collector).
        unsafe { trace(cell_address(reference)) };
        assert_eq!(color(reference), BLACK);
        assert_eq!(color(target), WHITE);
    }

    #[test]
    fn collecting_the_target_clears_the_reference() {
        let _guard = weak_guard();
        let mut heap = HeapState::new();
        let target = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let reference = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        assert!(register_local_weak(reference, target));
        // Only the reference survives the mark phase.
        mark_cell(target, WHITE);
        mark_survivor(reference);

        let _ = clear_weakrefs_local(&heap);
        assert_eq!(weakref_target_raw(reference), Some(crate::value::NONE));
        assert_eq!(sweep(&mut heap), 1);
        // The reference object itself is still alive and resolves to None.
        assert_eq!(weakref_target_raw(reference), Some(crate::value::NONE));
        assert!(heap.addresses.contains(&cell_address(reference)));
        assert!(!heap.addresses.contains(&cell_address(target)));
    }

    #[test]
    fn weak_reference_to_a_live_target_resolves() {
        let _guard = weak_guard();
        let mut heap = HeapState::new();
        let target = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let reference = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        assert!(register_local_weak(reference, target));
        mark_survivor(reference);
        mark_survivor(target);

        let _ = clear_weakrefs_local(&heap);
        assert_eq!(sweep(&mut heap), 0);
        assert_eq!(weakref_target_raw(reference), Some(target));
        assert_eq!(weakref_count(target), 1);
    }

    #[test]
    fn collected_reference_is_dropped_from_the_table() {
        let _guard = weak_guard();
        let mut heap = HeapState::new();
        let target = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let reference = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        assert!(register_local_weak(reference, target));
        // The reference is white; the target survives.
        mark_cell(reference, WHITE);
        mark_survivor(target);

        let _ = clear_weakrefs_local(&heap);
        assert_eq!(weakref_target_raw(reference), None);
        assert_eq!(sweep(&mut heap), 1);
        assert_eq!(weakref_count(target), 0);
    }

    #[test]
    fn weak_reference_in_a_cycle_does_not_pin_it() {
        let _guard = weak_guard();
        let mut heap = HeapState::new();
        // Two lists pointing at each other form an unreachable cycle.
        let first = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let second = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(vec![first]),
        );
        if let Some(mut items) = crate::value::list_items_mut(first) {
            items.push(second);
        }
        let reference = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        assert!(register_local_weak(reference, first));
        mark_cell(first, WHITE);
        mark_cell(second, WHITE);
        mark_survivor(reference);

        let _ = clear_weakrefs_local(&heap);
        assert_eq!(weakref_target_raw(reference), Some(crate::value::NONE));
        // Both members of the cycle are collected; the weak reference and its
        // non-owning edge do not retain either of them.
        assert_eq!(sweep(&mut heap), 2);
    }

    #[test]
    fn weakref_callback_runs_once_when_the_target_dies() {
        let _guard = weak_guard();
        let mut heap = HeapState::new();
        let target = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let reference = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let callback = counting_callback_value();
        assert!(weakref_register_with_callback(reference, target, callback));
        assert!(!weakref_callbacks_pending());

        // The target dies; the reference survives this cycle.
        mark_cell(target, WHITE);
        mark_survivor(reference);
        let resurrect = clear_weakrefs_local(&heap);
        assert_eq!(resurrect, vec![cell_address(reference)]);
        assert!(weakref_callbacks_pending());
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 0);

        run_pending_weakref_callbacks();
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(CALLBACK_LAST.load(Ordering::SeqCst), reference);

        // Nothing is armed any more: clearing and draining again is a no-op.
        let _ = clear_weakrefs_local(&heap);
        run_pending_weakref_callbacks();
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 1);
        assert!(!weakref_callbacks_pending());

        assert_eq!(sweep(&mut heap), 1);
        assert!(heap.addresses.contains(&cell_address(reference)));
    }

    #[test]
    fn weakref_callback_receives_an_unreachable_reference() {
        let _guard = weak_guard();
        let mut heap = HeapState::new();
        let target = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let reference = register_box(
            &mut heap,
            crate::value::KIND_LIST,
            ObjData::List(Vec::new()),
        );
        let callback = counting_callback_value();
        assert!(weakref_register_with_callback(reference, target, callback));

        // Both the target and the reference are white this cycle. The callback
        // still runs (with the reference) before either cell is released.
        mark_cell(target, WHITE);
        mark_cell(reference, WHITE);
        let resurrect = clear_weakrefs_local(&heap);
        assert_eq!(resurrect, vec![cell_address(reference)]);
        assert_eq!(weakref_target_raw(reference), None);
        run_pending_weakref_callbacks();
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(CALLBACK_LAST.load(Ordering::SeqCst), reference);
        assert_eq!(sweep(&mut heap), 2);
    }

    #[test]
    fn only_weakrefable_values_are_accepted() {
        let _guard = weak_guard();
        exc::init();
        let class = crate::value::class_new(String::from("C"), crate::value::NONE, Vec::new());
        let instance = crate::value::instance_new(class);
        assert!(weakref_supported(instance));
        assert!(!weakref_supported(crate::value::encode_int(1).unwrap()));
        assert!(!weakref_supported(crate::value::list_new(Vec::new())));
        assert!(!exc::pending());
    }

    #[test]
    fn capacity_shrink_policy_is_conservative() {
        // Small registries never shrink: the next collection refills them.
        assert!(!capacity_is_oversized(0, 0));
        assert!(!capacity_is_oversized(1024, 0));
        assert!(!capacity_is_oversized(1024, 100));
        assert!(!capacity_is_oversized(4092, 1023));
        // Four times the live length (or the 1024 floor) is still tolerable.
        assert!(!capacity_is_oversized(4096, 1024));
        assert!(!capacity_is_oversized(8192, 2048));
        // Grossly oversized allocations are returned.
        assert!(capacity_is_oversized(4097, 1024));
        assert!(capacity_is_oversized(8193, 2048));
        assert!(capacity_is_oversized(4096, 0));
    }

    #[test]
    fn sweep_keeps_address_index_exact() {
        let mut heap = HeapState::new();
        // Two white (unreachable garbage) and two black (survivors).
        let garbage_a = register_test_object(&mut heap, "garbage a");
        let garbage_b = register_test_object(&mut heap, "garbage b");
        let live_a_tagged = register_box(&mut heap, KIND_STR, ObjData::Str(String::from("live a")));
        let live_b_tagged = register_box(&mut heap, KIND_STR, ObjData::Str(String::from("live b")));
        let live_a = cell_address(live_a_tagged);
        let live_b = cell_address(live_b_tagged);
        mark_survivor(live_a_tagged);
        mark_survivor(live_b_tagged);

        let objects_capacity = heap.objects.capacity();
        let addresses_capacity = heap.addresses.capacity();

        let freed = sweep(&mut heap);
        assert_eq!(freed, 2);
        assert_eq!(heap.objects.len(), 2);
        // The rebuilt index matches the live registry exactly: no freed
        // address survives and no survivor lost its entry.
        let registered: AddressSet = heap.objects.iter().map(|&obj| obj as usize).collect();
        assert_eq!(
            registered,
            [live_a, live_b].into_iter().collect::<AddressSet>()
        );
        assert_eq!(heap.addresses, registered);
        assert_eq!(heap.addresses.len(), heap.objects.len());
        assert!(!heap.addresses.contains(&garbage_a));
        assert!(!heap.addresses.contains(&garbage_b));
        // Survivors were reset to white for the next cycle.
        for &obj in &heap.objects {
            // SAFETY: every registered pointer is live.
            assert_eq!(unsafe { (*obj).marked.load(Ordering::Acquire) }, WHITE);
        }
        // The rebuild reuses both allocations; the normal path neither
        // reallocates the index nor shrinks the registries.
        assert_eq!(heap.objects.capacity(), objects_capacity);
        assert_eq!(heap.addresses.capacity(), addresses_capacity);

        // A collection where every object is garbage empties both registries;
        // the index rebuild must not leave stale addresses behind.
        let freed = sweep(&mut heap);
        assert_eq!(freed, 2);
        assert!(heap.objects.is_empty());
        assert!(heap.addresses.is_empty());
        assert_eq!(heap.addresses.capacity(), addresses_capacity);
    }

    #[test]
    fn reset_clears_counters() {
        let _guard = stats_test_guard();
        // Force collections on this thread's private heap so every counter
        // has something to clear.
        for _ in 0..3 {
            let _ = collect_with_root(None);
        }
        let before = gc_stats();
        assert!(before.collections >= 3);

        reset_gc_stats();
        let after = gc_stats();
        // Other unit tests share the process-wide counters and run in
        // parallel, so another test may complete a collection between the
        // reset and this snapshot. The counters only ever grow; a reset must
        // still erase the collections forced above.
        assert!(
            after.collections < before.collections,
            "reset did not clear the collection counter: before={}, after={}",
            before.collections,
            after.collections
        );
    }
}
