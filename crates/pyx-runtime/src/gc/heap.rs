//! Interim conservative mark-&-sweep collector (ARCHITECTURE §7).
//!
//! Every heap object created by [`crate::value`] is registered here. The scan
//! is conservative: a word is treated as a tagged heap pointer only when its
//! low three bits are `010` *and* the decoded address is present in the
//! registry, so a false positive can only keep an object alive, never
//! dereference an unknown address.
//!
//! # Single-threaded and shared heaps
//!
//! By default every thread owns a thread-local registry, so independent
//! runtime unit tests stay isolated and the collector can scan the current
//! stack without coordination. Once `_thread.start_new_thread` is used the
//! process switches permanently to a process-wide *shared heap*
//! ([`enable_shared`]): all objects are migrated into one mutex-protected
//! registry and every later allocation is registered there, so objects
//! created by the main thread are visible to child threads and vice versa.
//!
//! # Incremental concurrent collection
//!
//! Shared-mode collections are incremental mark-&-sweep with only two short
//! stop-the-world windows:
//!
//! 1. **Root snapshot.** `MARKING` is published, the collector requests a
//!    stop-the-world, waits until every other registered thread is parked or
//!    exited, snapshots their saved contexts, pending mirrors and explicit
//!    roots (plus its own live stack/registers), copies the registry's address
//!    index, marks the roots grey and releases the world.
//! 2. **Concurrent marking.** Mutators keep running while the collector drains
//!    the worklist. Every mutation that stores a new child value publishes an
//!    insertion barrier for that edge ([`write_barrier_edge`]): if the
//!    container is already black, the *child* is marked grey and queued, so
//!    the new edge is traced without re-tracing the (possibly huge) parent.
//!    Allocations made while `MARKING` is set are born grey (floating garbage
//!    is acceptable).
//! 3. **Final rescan.** The collector stops the world again and holds the
//!    thread registry across the rescan and the final drain, so no
//!    registration, allocation or barrier can slip in. Afterwards it releases
//!    the world while `MARKING` stays set, so allocations made during the
//!    sweep are still born grey and can never be collected.
//! 4. **Concurrent sweep.** Under the heap lock only, the collector partitions
//!    the registry: white objects leave it, every survivor turns white again
//!    and the address index is rebuilt. The payloads are then dropped
//!    *outside* the heap lock while the mutators run; no live PyX object can
//!    reference them (the final rescan and the insertion barriers published
//!    every live edge before the world was released). The cells themselves go
//!    back to the pool's free list under the brief final heap lock, which
//!    also clears `MARKING` and whitens every registered object again.
//!
//! Threads park at [`safepoint`]s, which generated code executes at function
//! entry and loop backedges and which every allocation runs before touching
//! the heap. A thread that blocks (lock acquisition, `_thread.join`,
//! `time.sleep`) parks itself for the duration of the block via
//! [`blocking_enter`]/[`blocking_leave`]; while blocked its saved context is
//! scanned as usual, and it does not resume user code until a running
//! collection has finished. This is *not* a GIL: between safepoints threads
//! execute in parallel, and shared mutable containers still need user locks.
//!
//! # Locking and deadlock avoidance
//!
//! * `GLOBAL` (heap registry) is only held by the collector after every
//!   other thread is parked, except during the registry partition and final
//!   colour reset of the concurrent sweep, where it is held briefly and
//!   never across a payload drop (`ptr::drop_in_place`). A thread never waits
//!   for another thread while holding it.
//! * `GRAPH_LOCK` serialises container payload mutation (via the guards in
//!   [`crate::value`]) against the marker's [`trace`]. The marker takes it per
//!   object and holds it across the black transition; a guard holds it across
//!   the mutation *and* its edge barriers, so a barrier cannot race with the
//!   marker.
//! * `WORKLIST` is never held while acquiring `GRAPH_LOCK`, a queue's item
//!   mutex or `GLOBAL`; a mutator's barrier may therefore take `GRAPH_LOCK`
//!   (or the queue mutex) and then `WORKLIST`.
//! * The marker takes a queue's item mutex across the traversal *and* the
//!   black transition; a queue push publishes its edge barrier under the same
//!   mutex, so a push can never be lost to a completed trace.
//! * `GC_LOCK` is only ever taken with `try_lock`, so a second collector
//!   skips instead of blocking.
//! * [`blocking_enter`] must never run while the heap, registry, graph or
//!   collector lock is held, and no guard is held across a safepoint or a PyX
//!   allocation (a collection could otherwise wait for a lock the thread
//!   itself holds).
//! * A thread counted as parked only *waits*: it never inserts into or removes
//!   from a container between [`blocking_enter`] and [`blocking_leave`]
//!   (blocking queue operations park around their condvar wait and mutate
//!   after unparking), so a root snapshot cannot miss an edge created by a
//!   thread the collector believes is stopped.
//! * The registry mutex is taken before the heap mutex, never after.
//!
//! # Documented limits
//!
//! * The stack/register scan is conservative: a stale word keeps an object
//!   alive but can never free a reachable one.
//! * `GRAPH_LOCK` serialises every list/dict/set mutation against the marker.
//!   A mutation that takes long delays marking but never blocks the collector
//!   forever (mutators hold the lock only for the payload update and barrier).
//! * An iterator's `index` is not part of the traced graph; only its immutable
//!   `target` is. The owner advances the index, and a partially consumed
//!   iterator keeps its whole target alive.
//! * If a registered thread's saved context cannot be bounded, the collection
//!   is abandoned (objects are leaked) rather than scanned unsafely.
//!
//! # Allocation
//!
//! `PyxObj` has a single fixed layout, so the registry's storage is the slab
//! pool in [`alloc`]: cells are allocated in fixed-size slabs, released into a
//! free list and reused without touching the system allocator. That covers
//! the one object size class; variable *payload* size classes (and precise
//! shadow-stack roots) remain future work (P2-05/P2-06).
//!
//! Stack words are read with inline assembly because they may be
//! uninitialized; the relevant invariants are documented where they occur.
//!
//! This is deliberately an interim solution: P2-05/P2-06 (size-class
//! allocator, precise shadow-stack roots) replace it later.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use crate::exc;
use crate::value::{FunctionBody, ObjData, PyxObj, TAG_HEAP, TAG_MASK};

// The slab allocator is a private implementation detail of this collector. It
// lives in `gc/alloc.rs`; the explicit path keeps the module declaration here
// so `gc` stays a single-module subtree.
#[path = "alloc.rs"]
mod alloc;

use self::alloc::ObjectPool;

/// Default collection threshold (ARCHITECTURE §7).
const DEFAULT_THRESHOLD: usize = 10_000;

struct HeapState {
    /// Every live heap object, in allocation order.
    objects: Vec<*mut PyxObj>,
    /// Addresses of the objects in `objects`, for O(1) conservative lookups.
    addresses: HashSet<usize>,
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
    /// Collect before every allocation (`PYX_GC_STRESS=1`).
    stress: bool,
}

impl HeapState {
    fn new() -> HeapState {
        let (threshold, stress) = read_config();
        HeapState {
            objects: Vec::new(),
            addresses: HashSet::new(),
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
    /// configured threshold; `PYX_GC_STRESS` still collects every time.
    fn collection_due(&self) -> bool {
        self.stress
            || self.objects.len() > self.last_collection_objects.saturating_add(self.threshold)
    }

    /// Releases every registered object and clears the registry.
    ///
    /// The cells go back to [`ObjectPool`]'s free list, so the slabs survive
    /// (`pyx_runtime_init` may be called again in the same process) and the
    /// next allocation reuses them. Only `ObjectPool`'s `Drop` returns the
    /// slabs to the system allocator.
    fn free_all(&mut self) {
        for &obj in &self.objects {
            // SAFETY: every registered pointer came from `ObjectPool::allocate`
            // in `register_box`, is owned by this registry and is released
            // exactly once here. `release` drops only Rust-owned payloads
            // (`String`, `Vec`, `Box<IterState>`); it never touches other
            // PyX objects.
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
/// * The registry owns every `*mut PyxObj` it contains; each pointer was
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
/// * `PyxObj` payloads are `Send`; user code may still mutate a shared
///   container from two threads without a lock, which is a data race by
///   design (documented at the `_thread` module) and the programmer's
///   responsibility.
///
/// `HeapState` only needs `Send`; sharing is provided by the enclosing
/// `Mutex<HeapState>`, which is `Sync` and serialises every access.
unsafe impl Send for HeapState {}

static GLOBAL: LazyLock<Mutex<HeapState>> = LazyLock::new(|| Mutex::new(HeapState::new()));

// -------------------------------------------------------------------------
// Colours, graph lock and write barriers
// -------------------------------------------------------------------------

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
        let obj = &*(addr as *const PyxObj);
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
        (*(addr as *const PyxObj))
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
        let obj = &*(addr as *const PyxObj);
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

// -------------------------------------------------------------------------
// Thread registry and stop-the-world coordination
// -------------------------------------------------------------------------

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
/// * `roots` holds tagged values that are only reachable from a Rust heap
///   closure (e.g. the arguments of `_thread.start_new_thread`). It is set
///   before the thread is inserted into the registry and is therefore visible
///   to a collector before the thread can run user code; it is cleared when
///   the thread exits.
pub(crate) struct ThreadGc {
    id: i64,
    stack_low: AtomicUsize,
    stack_high: AtomicUsize,
    parked: AtomicBool,
    exited: AtomicBool,
    saved_sp: AtomicUsize,
    saved_regs: Mutex<[usize; REG_WORDS]>,
    pending: AtomicI64,
    roots: Mutex<Vec<i64>>,
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
    /// local-heap mode before `pyx_runtime_init` registered the main thread).
    static GC_SELF: RefCell<Option<Arc<ThreadGc>>> = const { RefCell::new(None) };
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

/// Cooperative safepoint (ABI `pyx_safepoint`).
///
/// The fast path is a single relaxed load (the slow path re-checks under the
/// registry lock, which supplies the ordering). If a collection is requested,
/// the calling thread saves its context, parks, and waits until the collector
/// has swept and cleared the request. Generated code calls this at every
/// function entry and loop backedge; every allocation and blocking operation
/// calls it too.
#[inline]
pub(crate) fn safepoint() {
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
/// only wait. It must not allocate a PyX object, mutate a container or publish
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
    let threshold = std::env::var("PYX_GC_THRESHOLD")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_THRESHOLD);
    let stress = std::env::var("PYX_GC_STRESS")
        .map(|value| value.trim() == "1")
        .unwrap_or(false);
    (threshold, stress)
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
        // `pyx_runtime_init`; the objects now belong to the shared registry.
        local.objects.shrink_to_fit();
        local.addresses.shrink_to_fit();
    });
    SHARED.store(true, Ordering::Release);
}

/// Frees every registered object and clears the registry. Called by
/// `pyx_runtime_init` after the exception state has been reset, so no tagged
/// pending value can survive the free.
///
/// Only valid while no runtime thread is alive: `pyx_runtime_init` must run
/// once at program start, never from a spawned thread.
pub(crate) fn reset() {
    // Unit tests run concurrently in one process. A test that inspects the
    // spawn registry must not lose it to another test's `pyx_runtime_init`.
    #[cfg(test)]
    let _registry_guard = crate::spawn::TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    HEAP.with(|cell| cell.borrow_mut().free_all());
    // A fresh program run must not resolve functions from the previous one.
    crate::spawn::reset();
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
    register_current_thread(1, Vec::new());
}

/// Forces a collection (ABI `pyx_gc_collect`). Returns the number of freed
/// objects; returns `0` when another collector is already running.
pub(crate) fn collect() -> i64 {
    if shared() {
        collect_shared(None)
    } else {
        collect_with_root(None) as i64
    }
}

/// Root set of one registered thread, captured while it is parked.
struct RootSnapshot {
    /// Saved stack pointer (`0` for the collector's own registration, whose
    /// live stack is scanned directly).
    sp: usize,
    high: usize,
    regs: [usize; REG_WORDS],
    pending: i64,
    roots: Vec<i64>,
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
        if id == self_id {
            // The caller's live stack/registers are scanned directly; only
            // its pending mirror and roots contribute separately.
            snapshots.push(RootSnapshot {
                sp: 0,
                high: 0,
                regs: [0; REG_WORDS],
                pending,
                roots,
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
            roots,
        });
    }
    Some(snapshots)
}

/// Marks every root of a snapshot set plus the caller's live stack,
/// registers and pending exception.
fn mark_roots(
    snapshots: &[RootSnapshot],
    extra_root: Option<i64>,
    addresses: &HashSet<usize>,
    self_sp: usize,
    self_high: usize,
) {
    if let Some(root) = extra_root {
        mark_word(root as usize, addresses);
    }
    // The caller's own pending exception and live context.
    mark_word(exc::pending_value() as usize, addresses);
    scan_stack(self_sp, self_high, addresses);
    mark_captured_registers(addresses);

    // Parked threads' saved stacks/registers, pending mirrors and
    // out-of-stack roots (spawn arguments held only by Rust closures).
    for thread in snapshots {
        mark_word(thread.pending as usize, addresses);
        for &root in &thread.roots {
            mark_word(root as usize, addresses);
        }
        if thread.high > thread.sp {
            scan_stack(thread.sp, thread.high, addresses);
        }
        for &register in &thread.regs {
            mark_word(register, addresses);
        }
    }
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
    }
    clear_worklist();
    GC_REQUESTED.store(false, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    0
}

/// Reads `PYX_GC_STATS` once per collection; stats are off by default.
fn gc_stats_enabled() -> bool {
    std::env::var("PYX_GC_STATS")
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
    let started = Instant::now();

    // A successful collection leaves the worklist empty and an aborted one
    // clears it; start from a known state anyway.
    clear_worklist();
    // From now on allocations are born grey and inserted edges mark their
    // child when the parent is already black, so the mutators may run while
    // the worklist drains.
    MARKING.store(true, Ordering::SeqCst);

    // ---- Phase A: root snapshot (stop the world) ----
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
    mark_roots(&snapshots, extra_root, &addresses, self_sp, self_high);
    drop(snapshots);
    drop(addresses);
    // Release the world: the mutators run while the worklist drains.
    GC_REQUESTED.store(false, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    drop(registry);
    let pause_a = started.elapsed();

    // ---- Phase B: concurrent marking ----
    let mark_start = Instant::now();
    drain_worklist();
    let mark_time = mark_start.elapsed();

    // ---- Phase C: final rescan (stop the world) ----
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
    mark_roots(&snapshots, extra_root, &addresses, self_sp, self_high);
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
    // Release the world for the sweep. `MARKING` stays set, so allocations
    // made now are born grey and queued; they are live by construction and
    // the partition below keeps them.
    GC_REQUESTED.store(false, Ordering::SeqCst);
    GC_CONDVAR.notify_all();
    drop(registry);
    let pause = pause_a + pause_start.elapsed();

    // ---- Phase D: concurrent sweep ----
    let sweep_start = Instant::now();
    // The only heap-lock section of the sweep: partition the registry. The
    // white objects leave it, survivors are reset to white and the address
    // index is rebuilt. Nothing is dropped under the lock.
    let whites = {
        let mut heap = lock_global();
        let whites = partition_whites(&mut heap);
        heap.last_collection_objects = heap.objects.len();
        whites
    };
    let freed = whites.len();
    // Drop payloads outside every lock, with the mutators running: no PyX
    // object can reference these addresses any more (the final rescan and
    // insertion barriers published every live edge before the world was
    // released), so the payloads can be dropped concurrently with allocation
    // and mutation. The cells themselves are returned to the pool in Phase E,
    // which already takes the heap lock briefly for the colour reset.
    for &obj in &whites {
        // SAFETY: every pointer came from `ObjectPool::allocate` in
        // `register_box`, was removed from the registry while the heap lock
        // was held and is owned by this collector, so its payload is dropped
        // exactly once here.
        unsafe { ObjectPool::drop_object(obj) };
    }

    // ---- Phase E: reset colours and finish ----
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
        heap.pool.stats()
    };
    drop(whites);
    // Outside the heap lock: a barrier that pushed between the reset and this
    // clear leaves a harmless stale entry (the next collection starts by
    // clearing the worklist and the object is live and white).
    clear_worklist();
    drop(_collector);

    if stats {
        eprintln!(
            "pyx-gc: pause_ms={:.3} mark_ms={:.3} sweep_ms={:.3} freed={} \
             slabs={} capacity={} live={} reused={}",
            pause.as_secs_f64() * 1000.0,
            mark_time.as_secs_f64() * 1000.0,
            sweep_start.elapsed().as_secs_f64() * 1000.0,
            freed,
            pool_stats.slabs,
            pool_stats.capacity,
            pool_stats.live,
            pool_stats.reused
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
        stack_low: AtomicUsize::new(low),
        stack_high: AtomicUsize::new(high),
        parked: AtomicBool::new(false),
        exited: AtomicBool::new(false),
        saved_sp: AtomicUsize::new(0),
        saved_regs: Mutex::new([0; REG_WORDS]),
        pending: AtomicI64::new(crate::value::NONE),
        roots: Mutex::new(roots),
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
    if let Some(gc) = GC_SELF.with(|cell| cell.borrow_mut().take()) {
        gc.exited.store(true, Ordering::SeqCst);
        let mut registry = lock_thread_registry();
        registry.remove(&gc.id);
        GC_CONDVAR.notify_all();
        // Drop the registry lock before the roots lock, matching the
        // collector's registry-then-roots order. The spawn arguments (tagged
        // values owned by the thread's Rust closure) are released here.
        drop(registry);
        gc.roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
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
        registry.clear();
    }
    ACTIVE_THREADS.store(1, Ordering::Release);
    NEXT_THREAD_ID.store(2, Ordering::Release);
    // A child inherits neither the collector nor its mark state: drop any
    // in-flight marking request, the worklist and every colour.
    MARKING.store(false, Ordering::SeqCst);
    clear_worklist();
    clear_local_worklist();
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

    clear_local_worklist();
    {
        let addresses = &heap.addresses;

        if let Some(root) = extra_root {
            mark_word(root as usize, addresses);
        }
        // The pending exception is a thread-local root.
        mark_word(exc::pending_value() as usize, addresses);

        scan_stack(sp, stack_high, addresses);
        mark_captured_registers(addresses);
    }
    drain_worklist();

    // ---- sweep -----------------------------------------------------
    let freed = sweep(heap);
    heap.last_collection_objects = heap.objects.len();
    freed
}

/// Frees all white registered objects and rebuilds the address index.
///
/// Only used by a single-threaded (local-heap) collection, where the sweep can
/// release each cell into the pool in place. The shared collector partitions
/// first and drops payloads outside the heap lock (see [`partition_whites`]).
///
/// Grey objects are kept as well (defensively: with the worklist drained they
/// should not exist) and turned white together with the black ones, so the
/// next collection starts from a clean heap.
fn sweep(heap: &mut HeapState) -> usize {
    let mut freed = 0usize;
    {
        let pool = &mut heap.pool;
        heap.objects.retain(|&obj| {
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
    }
    heap.objects.shrink_to_fit();

    let mut addresses = HashSet::with_capacity(heap.objects.len());
    addresses.extend(heap.objects.iter().map(|&obj| obj as usize));
    heap.addresses = addresses;
    heap.addresses.shrink_to_fit();

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
fn partition_whites(heap: &mut HeapState) -> Vec<*mut PyxObj> {
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

    let mut addresses = HashSet::with_capacity(heap.objects.len());
    addresses.extend(heap.objects.iter().map(|&obj| obj as usize));
    heap.addresses = addresses;
    heap.addresses.shrink_to_fit();

    whites
}

/// Marks `word` as a root if it looks like one of our tagged heap pointers.
fn mark_word(word: usize, addresses: &HashSet<usize>) {
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
/// `addr` must denote a live `PyxObj`.
unsafe fn trace(addr: usize) {
    let obj = &*(addr as *const PyxObj);
    match obj.kind {
        crate::value::KIND_LIST
        | crate::value::KIND_DICT
        | crate::value::KIND_SET
        | crate::value::KIND_DEFAULTDICT
        | crate::value::KIND_ARG_PARSER
        | crate::value::KIND_NAMESPACE => {
            let _graph = graph_lock();
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            match &obj.data {
                ObjData::List(items) => {
                    for &item in items {
                        mark_value(item);
                    }
                }
                ObjData::Dict(pairs) => {
                    for &(key, value) in pairs {
                        mark_value(key);
                        mark_value(value);
                    }
                }
                ObjData::Set(items) => {
                    for &item in items {
                        mark_value(item);
                    }
                }
                ObjData::DefaultDict(state) => {
                    if state.factory != crate::value::NONE {
                        mark_value(state.factory);
                    }
                    for &(key, item) in &state.pairs {
                        mark_value(key);
                        mark_value(item);
                    }
                }
                ObjData::ArgParser(parser) => {
                    for spec in &parser.options {
                        if spec.type_fn != crate::value::NONE {
                            mark_value(spec.type_fn);
                        }
                        if spec.default != crate::value::NONE {
                            mark_value(spec.default);
                        }
                        if let Some(choices) = &spec.choices {
                            for &choice in choices {
                                mark_value(choice);
                            }
                        }
                    }
                }
                ObjData::Namespace(namespace) => {
                    for &(_, item) in &namespace.pairs {
                        mark_value(item);
                    }
                }
                _ => {}
            }
            mark_black(addr);
        }
        crate::value::KIND_QUEUE => {
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            let ObjData::Queue(queue) = &obj.data else {
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
            let ObjData::Class(state) = &obj.data else {
                mark_black(addr);
                return;
            };
            // The class namespace is immutable, so no lock is needed: the
            // base and every method are stable GC edges.
            if state.base != crate::value::NONE {
                mark_value(state.base);
            }
            for &(_, function) in &state.methods {
                mark_value(function);
            }
            mark_black(addr);
        }
        crate::value::KIND_INSTANCE => {
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            let ObjData::Instance(state) = &obj.data else {
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
            mark_black(addr);
        }
        _ => {
            if obj.marked.load(Ordering::Acquire) != GREY {
                return;
            }
            match &obj.data {
                // An iterator's target is immutable; its index belongs to the
                // owning loop and is not a GC edge.
                ObjData::Iter(state) => mark_value(state.target),
                ObjData::Tuple(items) => {
                    for &item in items {
                        mark_value(item);
                    }
                }
                ObjData::Thread(thread) => {
                    mark_value(thread.target);
                    for &arg in &thread.args {
                        mark_value(arg);
                    }
                }
                ObjData::Process(process) => {
                    mark_value(process.target);
                    for &arg in &process.args {
                        mark_value(arg);
                    }
                }
                ObjData::Pool(pool) => {
                    // The queue vectors are immutable after construction.
                    for &queue in &pool.task_queues {
                        mark_value(queue);
                    }
                    for &queue in &pool.result_queues {
                        mark_value(queue);
                    }
                }
                ObjData::CsvWriter(writer) => mark_value(writer.file),
                // Wrapper functions own their captured values; native and
                // builtin function bodies own no PyX values.
                ObjData::Function(function) => {
                    if let FunctionBody::Wrapper { captures, .. } = &function.body {
                        for &capture in captures {
                            mark_value(capture);
                        }
                    }
                }
                // Scalars, ranges, locks, events, cross-process
                // queues, files, csv readers, paths, compiled patterns and
                // matches hold no PyX values.
                ObjData::Str(_)
                | ObjData::Float(_)
                | ObjData::Exception(_)
                | ObjData::Range { .. }
                | ObjData::Lock(_)
                | ObjData::Event(_)
                | ObjData::MpQueue(_)
                | ObjData::File(_)
                | ObjData::Bytes(_)
                | ObjData::Random(_)
                | ObjData::Path(_)
                | ObjData::Pattern(_)
                | ObjData::Match(_)
                | ObjData::Socket(_)
                | ObjData::CsvReader(_) => {}
                // The mutable containers were handled above; they cannot
                // appear here.
                ObjData::List(_)
                | ObjData::Dict(_)
                | ObjData::Set(_)
                | ObjData::DefaultDict(_)
                | ObjData::ArgParser(_)
                | ObjData::Namespace(_)
                | ObjData::Queue(_)
                | ObjData::Class(_)
                | ObjData::Instance(_) => {}
            }
            mark_black(addr);
        }
    }
}

/// Conservatively scans the native stack from `sp` (inclusive) up to `high`
/// (exclusive). Every aligned word is inspected; only tagged values whose
/// object is registered are marked.
fn scan_stack(sp: usize, high: usize, addresses: &HashSet<usize>) {
    let mut cursor = sp & !(std::mem::align_of::<usize>() - 1);
    while cursor + std::mem::size_of::<usize>() <= high {
        // SAFETY: `cursor` lies inside the current thread's stack mapping
        // (bounds obtained from `pthread_attr_getstack`); the load is a plain
        // machine word read that is opaque to the compiler, so it cannot
        // assume the stack slot holds an initialized `usize`.
        let word = unsafe { load_stack_word(cursor) };
        mark_word(word, addresses);
        cursor += std::mem::size_of::<usize>();
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
/// Same contract as the x86_64 variant. The interim collector accepts the
/// theoretical uninitialized-memory caveat on these targets; they are not
/// supported run targets.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline]
unsafe fn load_stack_word(addr: usize) -> usize {
    unsafe { std::ptr::read_unaligned(addr as *const usize) }
}

// -------------------------------------------------------------------------
// Stack bounds
// -------------------------------------------------------------------------

#[cfg(unix)]
fn stack_bounds() -> Option<(usize, usize)> {
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
fn stack_bounds() -> Option<(usize, usize)> {
    // Without a portable stack-bounds API the collector conservatively
    // declines to run (objects are simply leaked).
    None
}

// -------------------------------------------------------------------------
// Register dump
// -------------------------------------------------------------------------

/// Marks every word of the register dump returned by [`spill_registers`].
fn mark_captured_registers(addresses: &HashSet<usize>) {
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
/// that point), and [`scan_stack`] covers that spill slot. All other GPRs are
/// captured verbatim. Because the whole buffer is marked, no register needs
/// special casing.
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
