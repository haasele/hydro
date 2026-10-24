//! Tagged value representation and heap object layout (ARCHITECTURE §3.1).
//!
//! All runtime values are `i64`. The low three bits select the representation:
//!
//! | bits  | meaning                                                |
//! |-------|--------------------------------------------------------|
//! | `000` | immediate `int`, value is `v >> 3` (arithmetic shift)   |
//! | `001` | immediate special: `None = 1`, `False = 9`, `True = 17` |
//! | `010` | heap pointer: `ptr = v & !7`, points at [`PyxObj`]      |
//!
//! Heap objects carry a `kind: u32` header and are allocated with
//! `Box::into_raw`. They are owned by the interim conservative mark-&-sweep
//! collector in [`crate::heap`] (ARCHITECTURE §7): a reference returned by
//! this module stays valid as long as the object is reachable from the
//! runtime roots. Scalars are immutable; lists and dicts are mutated in place
//! by item assignment, iterators advance in place and tuples are immutable.
//! After `_thread.start_new_thread` the heap is process-wide and values are
//! visible to every thread; there is no GIL, so concurrent mutation of the
//! same list/dict requires a user lock. The registry and collector access is
//! mutex-protected (see [`crate::heap`]).

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::container::DictData;
use crate::exc::ExcData;

/// Mask selecting the representation bits of a tagged value.
pub(crate) const TAG_MASK: i64 = 0b111;
pub(crate) const TAG_INT: i64 = 0b000;
pub(crate) const TAG_SPECIAL: i64 = 0b001;
pub(crate) const TAG_HEAP: i64 = 0b010;

/// Immediate singletons (payload `0/1/2`).
pub(crate) const NONE: i64 = 1;
pub(crate) const FALSE: i64 = 9;
pub(crate) const TRUE: i64 = 17;

/// Heap object kinds.
pub(crate) const KIND_STR: u32 = 0;
pub(crate) const KIND_FLOAT: u32 = 1;
pub(crate) const KIND_EXCEPTION: u32 = 2;
pub(crate) const KIND_LIST: u32 = 3;
pub(crate) const KIND_DICT: u32 = 4;
pub(crate) const KIND_ITER: u32 = 5;
pub(crate) const KIND_TUPLE: u32 = 6;
pub(crate) const KIND_SET: u32 = 7;
pub(crate) const KIND_RANGE: u32 = 8;
pub(crate) const KIND_FUNCTION: u32 = 9;
/// `_thread.allocate_lock()` / `threading.Lock` / `threading.RLock` object.
/// Holds no PyX values.
pub(crate) const KIND_LOCK: u32 = 10;
/// `threading.Thread` object. Traces its `target` and `args`.
pub(crate) const KIND_THREAD: u32 = 11;
/// `threading.Event` object. Holds no PyX values.
pub(crate) const KIND_EVENT: u32 = 12;
/// `queue.Queue` object. Traces every queued item.
pub(crate) const KIND_QUEUE: u32 = 13;
/// `multiprocessing.Process` object. Traces its `target` and `args`.
pub(crate) const KIND_PROCESS: u32 = 14;
/// `multiprocessing.Queue` object. Items travel as pickled bytes, so the
/// native object holds no PyX children.
pub(crate) const KIND_MPQUEUE: u32 = 15;
/// `multiprocessing.Pool` object. Traces its per-worker task and result
/// queues; the worker pids and flags are plain native state.
pub(crate) const KIND_POOL: u32 = 16;
/// Text file object returned by `io.open`. Holds no PyX values; the native
/// file handle is closed when the object is dropped.
pub(crate) const KIND_FILE: u32 = 17;
/// Parsed-snapshot `csv.reader`/`csv.DictReader`. Holds Python strings, not
/// tagged values.
pub(crate) const KIND_CSVREADER: u32 = 18;
/// `csv.writer`/`csv.DictWriter`; holds the tagged target file as a GC edge.
pub(crate) const KIND_CSVWRITER: u32 = 19;
/// Immutable `bytes` object. Holds no PyX values.
pub(crate) const KIND_BYTES: u32 = 20;
/// `random.Random` generator object. Holds no PyX values; its PRNG state is
/// native and mutex-protected.
pub(crate) const KIND_RANDOM: u32 = 21;
/// `pathlib.Path` object. Holds a single native path string, no PyX values.
pub(crate) const KIND_PATH: u32 = 22;
/// `argparse.ArgumentParser`. Traces its option conversion functions,
/// defaults and choices.
pub(crate) const KIND_ARG_PARSER: u32 = 23;
/// `argparse.Namespace`. Traces every attribute value.
pub(crate) const KIND_NAMESPACE: u32 = 24;
/// `collections.defaultdict`. Traces its factory and every pair.
pub(crate) const KIND_DEFAULTDICT: u32 = 25;
/// Compiled `re.Pattern`; holds native regex state, no PyX values.
pub(crate) const KIND_PATTERN: u32 = 26;
/// `re.Match`; holds the searched text and capture positions, no PyX values.
pub(crate) const KIND_MATCH: u32 = 27;
/// Blocking TCP `socket` object. Holds native state only, no PyX values; the
/// descriptor is closed when the object is dropped.
pub(crate) const KIND_SOCKET: u32 = 28;
/// Class object created by `pyx_class_new`. Traces its base class and every
/// method value. The class namespace is immutable after creation.
pub(crate) const KIND_CLASS: u32 = 29;
/// Instance of a class. Traces its class and every instance attribute.
pub(crate) const KIND_INSTANCE: u32 = 30;
/// Stackful generator/coroutine fiber. Traces its arguments and result; the
/// suspended stack is registered as a separate GC root.
pub(crate) const KIND_FIBER: u32 = 31;
/// Function obtained from a CPython limited-API extension module. Stores the
/// shim's module handle and method id plus the Python attribute name; it has
/// no PyX children.
pub(crate) const KIND_EXT_FUNCTION: u32 = 32;
/// Opaque pointer capsule obtained through the CPython C API. Holds a native
/// pointer and an optional name; it has no PyX children, and its destructor
/// is never called in this slice.
pub(crate) const KIND_CAPSULE: u32 = 33;

/// Function flag: created by `async def`, a call produces a coroutine.
pub(crate) const FUNCTION_FLAG_ASYNC: u32 = 1;
/// Function flag: contains `yield`, a call produces a generator.
pub(crate) const FUNCTION_FLAG_GENERATOR: u32 = 2;

/// Largest integer representable in a tagged immediate (`2^60 - 1`).
pub(crate) const MAX_INT: i64 = i64::MAX >> 3;
/// Smallest integer representable in a tagged immediate (`-2^60`).
pub(crate) const MIN_INT: i64 = i64::MIN >> 3;

/// Heap object header plus payload.
///
/// `#[repr(C)]` guarantees `kind` sits at offset 0 and that the alignment of
/// the payload (8, because of `String`/`f64`) makes every allocation
/// 8-byte aligned, which keeps the low tag bits free.
#[repr(C)]
pub(crate) struct PyxObj {
    pub(crate) kind: u32,
    /// Collector colour, owned by [`crate::heap`]: `WHITE` (0, not yet
    /// reached), `GREY` (1, queued for tracing) or `BLACK` (2, traced).
    ///
    /// The field is one byte wide, so `kind` stays at offset 0 and the
    /// 8-byte alignment of `data` (forced by `String`/`f64`) keeps every
    /// allocation 8-byte aligned, which keeps the low tag bits free.
    pub(crate) marked: AtomicU8,
    pub(crate) data: ObjData,
}

/// Mutable state of a runtime iterator. `target` is the tagged iterable and
/// `index` is the next position to yield; it belongs to one `for` loop.
pub(crate) struct IterState {
    pub(crate) target: i64,
    pub(crate) index: usize,
}

/// What a function value refers to.
pub(crate) enum FunctionBody {
    /// Address of a generated `extern "C"` entry point plus the values
    /// captured when a nested function was defined. The wrapper receives
    /// `captures + arguments`, in that order.
    Wrapper { entry: usize, captures: Vec<i64> },
    /// Native implementation from one of the `pyx_ir::stdlib` modules.
    Native {
        module: &'static str,
        name: &'static str,
    },
    /// A Python builtin referenced as a value (`f = int`).
    Builtin(pyx_ir::Builtin),
}

/// A function value. For [`FunctionBody::Wrapper`] values `arity` is the exact
/// number of user-visible positional parameters (captured values are not
/// counted); native functions validate their arguments against the central
/// spec (`min_args..=max_args`) and store the upper bound here. Builtin values
/// store `usize::MAX` because they validate their own arity. `name` is the
/// Python-visible name (`<lambda>` for anonymous functions). Wrapper function
/// objects own their captured PyX values; other function bodies own none.
pub(crate) struct FunctionObj {
    pub(crate) body: FunctionBody,
    pub(crate) arity: usize,
    pub(crate) name: String,
    /// [`FUNCTION_FLAG_ASYNC`] / [`FUNCTION_FLAG_GENERATOR`] bits for a
    /// generated wrapper. Native and builtin bodies keep `0`.
    pub(crate) flags: u32,
}

/// Ownership state of a runtime lock, guarded by one mutex.
///
/// `held` records whether the lock is taken; `owner`/`depth` implement
/// reentrancy for `threading.RLock`. Plain locks keep `depth == 1` and only
/// use `held`, so their behaviour is unchanged (release by any thread).
struct LockInner {
    held: bool,
    owner: i64,
    depth: usize,
}

impl LockInner {
    fn unlocked() -> LockInner {
        LockInner {
            held: false,
            owner: 0,
            depth: 0,
        }
    }
}

/// Mutable state of a runtime lock (`_thread` locks, `threading.Lock` and
/// `threading.RLock`).
///
/// `released` wakes one waiter per release. All fields are `Sync`, so a lock
/// object can be shared by every thread. A blocking acquisition parks the
/// calling thread for the stop-the-world collector before it waits, so a lock
/// holder stopped by a collection can never keep the collector waiting on an
/// unaccounted thread.
///
/// `RLock.acquire` by the owning thread increments `depth`; `release` is only
/// accepted from the owner (the runtime raises `RuntimeError` otherwise).
/// Plain locks stay binary and non-reentrant.
pub(crate) struct LockState {
    inner: Mutex<LockInner>,
    released: Condvar,
    reentrant: bool,
}

impl LockState {
    fn new_inner(reentrant: bool) -> LockState {
        LockState {
            inner: Mutex::new(LockInner::unlocked()),
            released: Condvar::new(),
            reentrant,
        }
    }

    fn new() -> LockState {
        LockState::new_inner(false)
    }

    fn new_reentrant() -> LockState {
        LockState::new_inner(true)
    }

    /// Locks the ownership state, ignoring poisoning: a panic in user code
    /// must not make the lock unusable.
    fn inner(&self) -> std::sync::MutexGuard<'_, LockInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Acquires the lock. With `blocking == false` a single attempt is made.
    /// Returns whether the lock is now held (always `true` when blocking).
    pub(crate) fn acquire(&self, blocking: bool) -> bool {
        let me = crate::heap::current_thread_id();
        if !blocking {
            let mut inner = self.inner();
            if !inner.held {
                inner.held = true;
                inner.owner = me;
                inner.depth = 1;
                return true;
            }
            if self.reentrant && inner.owner == me {
                inner.depth += 1;
                return true;
            }
            return false;
        }
        // Fast path: an uncontended acquisition takes no park round-trip.
        {
            let mut inner = self.inner();
            if !inner.held {
                inner.held = true;
                inner.owner = me;
                inner.depth = 1;
                return true;
            }
            if self.reentrant && inner.owner == me {
                inner.depth += 1;
                return true;
            }
        }
        // Park for the stop-the-world collector while waiting: a lock holder
        // that is itself stopped by the collector must never be waited on
        // unaccounted. `blocking_enter`/`blocking_leave` take no heap,
        // registry or lock-internal mutex across the block.
        crate::heap::blocking_enter();
        let mut inner = self.inner();
        while inner.held {
            if self.reentrant && inner.owner == me {
                // Defensive: ownership could only change through this thread.
                inner.depth += 1;
                drop(inner);
                crate::heap::blocking_leave();
                return true;
            }
            inner = self
                .released
                .wait(inner)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        inner.held = true;
        inner.owner = me;
        inner.depth = 1;
        drop(inner);
        crate::heap::blocking_leave();
        true
    }

    /// Releases the lock; `false` means it was not held (or, for an `RLock`,
    /// that the caller is not the owner).
    pub(crate) fn release(&self) -> bool {
        let mut inner = self.inner();
        if !inner.held {
            return false;
        }
        if self.reentrant && inner.owner != crate::heap::current_thread_id() {
            return false;
        }
        inner.depth = inner.depth.saturating_sub(1);
        if inner.depth > 0 {
            return true;
        }
        inner.held = false;
        inner.owner = 0;
        inner.depth = 0;
        drop(inner);
        self.released.notify_one();
        true
    }

    pub(crate) fn locked(&self) -> bool {
        self.inner().held
    }

    /// Whether this is a `threading.RLock` (release is owner-checked).
    pub(crate) fn is_reentrant(&self) -> bool {
        self.reentrant
    }
}

/// SplitMix64 step; the same constant as the reference implementation.
const SPLITMIX_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// Mixes one 64-bit word (SplitMix64's finaliser).
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Entropy source for unseeded generators: wall clock, process id and a
/// per-process counter. Only used when no seed is supplied.
fn entropy_seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    mix64(nanos ^ pid.rotate_left(32) ^ count.wrapping_mul(SPLITMIX_GAMMA))
}

/// Native payload of a `pathlib.Path` object.
///
/// The path is stored exactly as the constructor received it (no
/// normalisation); derived values such as `name`/`parent` are computed from
/// it on demand. A path holds no PyX values.
pub(crate) struct PathState {
    path: String,
}

impl PathState {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
}

/// Mutable PRNG state of a `random.Random` object (or of the module's global
/// generator).
///
/// The algorithm is SplitMix64, which is deterministic for a given seed but
/// deliberately *not* reproducible against CPython's Mersenne Twister.
pub(crate) struct RandomState {
    state: Mutex<u64>,
}

impl RandomState {
    /// Creates a generator seeded from [`entropy_seed`].
    pub(crate) fn new() -> RandomState {
        RandomState::seeded(entropy_seed())
    }

    /// Creates a generator for an explicit 64-bit seed.
    pub(crate) fn seeded(seed: u64) -> RandomState {
        RandomState {
            state: Mutex::new(mix64(seed)),
        }
    }

    /// Creates a generator seeded from the process environment.
    pub(crate) fn from_entropy() -> RandomState {
        RandomState::seeded(entropy_seed())
    }

    fn state(&self) -> MutexGuard<'_, u64> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reseeds the generator in place (`random.seed` / `Random.seed`).
    pub(crate) fn reset(&self, seed: u64) {
        *self.state() = mix64(seed);
    }

    /// Reseeds from the environment (`seed()` / `seed(None)`).
    pub(crate) fn reset_entropy(&self) {
        self.reset(entropy_seed());
    }

    /// Next 64 bits (SplitMix64).
    pub(crate) fn next_u64(&self) -> u64 {
        let mut state = self.state();
        *state = state.wrapping_add(SPLITMIX_GAMMA);
        mix64(*state)
    }

    /// Uniform float in `[0, 1)` with 53 random bits.
    pub(crate) fn next_f64(&self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform integer in `0..width` (`width > 0`), rejection-sampled to
    /// avoid modulo bias.
    pub(crate) fn below(&self, width: u64) -> u64 {
        debug_assert!(width > 0);
        if width == 1 {
            return 0;
        }
        let zone = (u64::MAX / width) * width;
        loop {
            let draw = self.next_u64();
            if draw < zone {
                return draw % width;
            }
        }
    }
}

/// Shared state of a `threading.Thread` object.
///
/// `target` and `args` are immutable after construction, so the collector may
/// trace them without synchronisation. `started`, `id`, `joined` and the
/// shared `finished` flag are atomics: the spawning, joining and child threads
/// all observe them.
pub(crate) struct ThreadState {
    pub(crate) target: i64,
    pub(crate) args: Vec<i64>,
    pub(crate) name: String,
    pub(crate) started: AtomicBool,
    /// Python-visible id assigned when the OS thread is spawned (`0` before).
    pub(crate) id: AtomicI64,
    pub(crate) joined: AtomicBool,
    /// Set by the child just before it leaves the runtime.
    pub(crate) finished: Arc<AtomicBool>,
}

impl ThreadState {
    fn new(target: i64, args: Vec<i64>, name: String) -> ThreadState {
        ThreadState {
            target,
            args,
            name,
            started: AtomicBool::new(false),
            id: AtomicI64::new(0),
            joined: AtomicBool::new(false),
            finished: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Shared state of a `threading.Event`.
///
/// The flag and condition variable are independent of the heap, so a waiting
/// thread parks for the collector while it sleeps on the condition variable.
pub(crate) struct EventState {
    flag: Mutex<bool>,
    changed: Condvar,
}

impl EventState {
    fn new() -> EventState {
        EventState {
            flag: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn flag(&self) -> std::sync::MutexGuard<'_, bool> {
        self.flag
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn set(&self) {
        let mut flag = self.flag();
        *flag = true;
        drop(flag);
        self.changed.notify_all();
    }

    pub(crate) fn clear(&self) {
        *self.flag() = false;
    }

    pub(crate) fn is_set(&self) -> bool {
        *self.flag()
    }

    /// Waits until the event is set or `timeout` expires. Returns the flag.
    pub(crate) fn wait(&self, timeout: Option<Duration>) -> bool {
        if self.is_set() {
            return true;
        }
        // Park before the first wait; the flag mutex is only taken after the
        // park, so no runtime mutex is held while blocking.
        crate::heap::blocking_enter();
        let result = {
            let mut flag = self.flag();
            match timeout {
                None => {
                    while !*flag {
                        flag = self
                            .changed
                            .wait(flag)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    true
                }
                Some(limit) => {
                    let deadline = Instant::now() + limit;
                    loop {
                        if *flag {
                            break true;
                        }
                        let now = Instant::now();
                        if now >= deadline {
                            break false;
                        }
                        let (next, _) = self
                            .changed
                            .wait_timeout(flag, deadline - now)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        flag = next;
                    }
                }
            }
        };
        crate::heap::blocking_leave();
        result
    }
}

/// Failure modes of a `queue.Queue` operation; the module layer maps them to
/// `RuntimeError` with the CPython message (`queue.Full` / `queue.Empty`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueError {
    Full,
    Empty,
}

/// Shared state of a `queue.Queue`: FIFO storage plus the two condition
/// variables used by blocking producers and consumers.
///
/// The mutex is held only for short bookkeeping; a waiting operation releases
/// it inside `Condvar::wait` and parks for the collector beforehand.
pub(crate) struct QueueState {
    maxsize: usize,
    items: Mutex<VecDeque<i64>>,
    not_empty: Condvar,
    not_full: Condvar,
    /// Tagged value of the owning queue object, installed right after the
    /// allocation. A queue push publishes an insertion barrier for the new
    /// item so a black queue's item is marked without re-tracing the queue.
    owner: AtomicI64,
}

impl QueueState {
    fn new(maxsize: usize) -> QueueState {
        QueueState {
            maxsize,
            items: Mutex::new(VecDeque::new()),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            owner: AtomicI64::new(0),
        }
    }

    /// Installs the tagged value of the owning heap object (the `owner` tag
    /// used by the insertion barrier).
    pub(crate) fn set_owner(&self, owner: i64) {
        self.owner
            .store(owner, std::sync::atomic::Ordering::Release);
    }

    /// Publishes the pushed item as a new edge of the owning queue.
    ///
    /// Must run *after* the item was pushed and *while the item mutex is
    /// still held*: the marker also takes that mutex across the queue's black
    /// transition, so the barrier cannot observe a stale colour after the
    /// marker already traced the queue.
    fn barrier(&self, item: i64) {
        let owner = self.owner.load(std::sync::atomic::Ordering::Acquire);
        if owner != 0 {
            crate::heap::write_barrier_edge(owner, item);
        }
    }

    fn items(&self) -> std::sync::MutexGuard<'_, VecDeque<i64>> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Locks the queue for tracing. The concurrent marker takes this per
    /// queue while mutators run; a blocking queue operation only ever holds
    /// it inside short bookkeeping and never while it sleeps.
    pub(crate) fn trace_items(&self) -> std::sync::MutexGuard<'_, VecDeque<i64>> {
        self.items()
    }

    /// True when the configured capacity is reached (`maxsize == 0` means
    /// unbounded).
    fn is_full(&self, items: &VecDeque<i64>) -> bool {
        self.maxsize != 0 && items.len() >= self.maxsize
    }

    /// Enqueues `item`. A blocking call parks for the collector **only while
    /// it waits for capacity**, never around the insertion itself: a thread
    /// counted as parked must not modify the heap graph next to a root
    /// snapshot.
    pub(crate) fn put(
        &self,
        item: i64,
        block: bool,
        timeout: Option<Duration>,
    ) -> Result<(), QueueError> {
        if !block {
            return self.try_put(item);
        }
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            if self.try_put(item).is_ok() {
                return Ok(());
            }
            if !self.wait_for_space(deadline) {
                return Err(QueueError::Full);
            }
        }
    }

    /// Inserts without waiting. Returns [`QueueError::Full`] when the queue is
    /// at capacity.
    fn try_put(&self, item: i64) -> Result<(), QueueError> {
        let mut items = self.items();
        if self.is_full(&items) {
            return Err(QueueError::Full);
        }
        items.push_back(item);
        // Barrier under the item mutex: it is then ordered with the marker's
        // black transition (which also runs under that mutex), so a push can
        // never slip past a completed trace.
        self.barrier(item);
        drop(items);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Waits for room while parked for the collector. The item mutex is only
    /// taken inside the park and released by every condvar wait, and nothing
    /// is mutated before the caller unparks and retries [`Self::try_put`].
    /// Returns `false` when a deadline expires.
    fn wait_for_space(&self, deadline: Option<Instant>) -> bool {
        crate::heap::blocking_enter();
        let room = {
            let mut items = self.items();
            match deadline {
                None => {
                    while self.is_full(&items) {
                        items = self
                            .not_full
                            .wait(items)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    true
                }
                Some(deadline) => loop {
                    if !self.is_full(&items) {
                        break true;
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        break false;
                    }
                    let (next, _) = self
                        .not_full
                        .wait_timeout(items, deadline - now)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    items = next;
                },
            }
        };
        crate::heap::blocking_leave();
        room
    }

    /// Dequeues one item. A blocking call parks for the collector **only
    /// while it waits for an item**, never around the pop: a parked thread
    /// must not move a value out of the heap graph unscanned.
    pub(crate) fn get(&self, block: bool, timeout: Option<Duration>) -> Result<i64, QueueError> {
        if !block {
            return self.try_get();
        }
        let deadline = timeout.map(|limit| Instant::now() + limit);
        loop {
            if let Ok(item) = self.try_get() {
                return Ok(item);
            }
            if !self.wait_for_item(deadline) {
                return Err(QueueError::Empty);
            }
        }
    }

    /// Removes and returns one item without waiting.
    fn try_get(&self) -> Result<i64, QueueError> {
        let mut items = self.items();
        let Some(item) = items.pop_front() else {
            return Err(QueueError::Empty);
        };
        drop(items);
        self.not_full.notify_one();
        Ok(item)
    }

    /// Waits for an item while parked for the collector (see
    /// [`Self::wait_for_space`]). Returns `false` when a deadline expires.
    fn wait_for_item(&self, deadline: Option<Instant>) -> bool {
        crate::heap::blocking_enter();
        let available = {
            let mut items = self.items();
            match deadline {
                None => {
                    while items.is_empty() {
                        items = self
                            .not_empty
                            .wait(items)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    true
                }
                Some(deadline) => loop {
                    if !items.is_empty() {
                        break true;
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        break false;
                    }
                    let (next, _) = self
                        .not_empty
                        .wait_timeout(items, deadline - now)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    items = next;
                },
            }
        };
        crate::heap::blocking_leave();
        available
    }

    pub(crate) fn qsize(&self) -> usize {
        self.items().len()
    }

    pub(crate) fn empty(&self) -> bool {
        self.items().is_empty()
    }

    pub(crate) fn full(&self) -> bool {
        let items = self.items();
        self.is_full(&items)
    }
}

/// Sentinel stored in [`ProcessState::exitcode`] while the child has not been
/// reaped (`None` in Python).
pub(crate) const NO_EXIT_CODE: i64 = i64::MIN;

/// Shared state of a `multiprocessing.Process` object.
pub(crate) struct ProcessState {
    pub(crate) target: i64,
    pub(crate) args: Vec<i64>,
    pub(crate) name: String,
    pub(crate) started: AtomicBool,
    pub(crate) pid: AtomicI64,
    pub(crate) joined: AtomicBool,
    /// Set once `waitpid` reaped the child.
    pub(crate) reaped: AtomicBool,
    pub(crate) exitcode: AtomicI64,
}

impl ProcessState {
    fn new(target: i64, args: Vec<i64>, name: String) -> ProcessState {
        ProcessState {
            target,
            args,
            name,
            started: AtomicBool::new(false),
            pid: AtomicI64::new(0),
            joined: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            exitcode: AtomicI64::new(NO_EXIT_CODE),
        }
    }
}

/// Partially received bytes of a `multiprocessing.Queue` frame.
pub(crate) struct MpReadState {
    /// Bytes received but not yet consumed as a complete frame.
    pub(crate) buffer: Vec<u8>,
}

/// Shared state of a `multiprocessing.Queue`: a `socketpair` whose file
/// descriptors survive `fork`, so parent and child can exchange framed
/// pickled values.
///
/// `maxsize` is accepted as metadata only: a cross-process item count is not
/// available, so the queue does not enforce it (documented deviation).
pub(crate) struct MpQueueState {
    pub(crate) read_fd: i32,
    pub(crate) write_fd: i32,
    pub(crate) maxsize: usize,
    pub(crate) read: Mutex<MpReadState>,
    pub(crate) write: Mutex<()>,
}

impl MpQueueState {
    pub(crate) fn new(read_fd: i32, write_fd: i32, maxsize: usize) -> MpQueueState {
        MpQueueState {
            read_fd,
            write_fd,
            maxsize,
            read: Mutex::new(MpReadState { buffer: Vec::new() }),
            write: Mutex::new(()),
        }
    }
}

/// Shared state of a `multiprocessing.Pool` object.
///
/// Every worker owns one task queue and one result queue (never shared), so
/// frames from different writers cannot interleave. The queue vectors are
/// immutable after construction; the pids, round-robin cursor and lifecycle
/// flags are mutable native state.
pub(crate) struct PoolState {
    pub(crate) task_queues: Vec<i64>,
    pub(crate) result_queues: Vec<i64>,
    pub(crate) pids: Mutex<Vec<i32>>,
    pub(crate) next: AtomicUsize,
    pub(crate) closed: AtomicBool,
    pub(crate) joined: AtomicBool,
}

impl PoolState {
    pub(crate) fn new(task_queues: Vec<i64>, result_queues: Vec<i64>) -> PoolState {
        PoolState {
            task_queues,
            result_queues,
            pids: Mutex::new(Vec::new()),
            next: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            joined: AtomicBool::new(false),
        }
    }

    /// Number of workers (always the length of both queue vectors).
    pub(crate) fn size(&self) -> usize {
        self.task_queues.len()
    }
}

/// Failure modes of a file operation.
///
/// The module/method layer maps these to the Python exceptions: a closed file
/// and invalid arguments are `ValueError`, everything else is `OSError`.
#[derive(Debug)]
pub(crate) enum FileError {
    /// The file has been closed (`ValueError`, CPython's message).
    Closed,
    /// Invalid argument such as an unknown `seek` whence (`ValueError`).
    Value(String),
    /// I/O failure, including mode mismatches such as reading a
    /// write-only file (`OSError`).
    Os(String),
}

impl FileError {
    pub(crate) fn message(&self) -> &str {
        match self {
            FileError::Closed => "I/O operation on closed file",
            FileError::Value(message) | FileError::Os(message) => message,
        }
    }
}

/// Raises the Python exception matching a file failure and returns the dummy
/// `None` value.
pub(crate) fn raise_file_error(err: FileError) -> i64 {
    match &err {
        FileError::Closed | FileError::Value(_) => {
            crate::exc::raise_with_message(crate::exc::VALUE_ERROR, err.message())
        }
        FileError::Os(_) => crate::exc::raise_with_message(crate::exc::OS_ERROR, err.message()),
    }
}

/// Native payload of a file object: either the fully read content (read mode)
/// or the OS handle (write/append mode).
///
/// Text files hold the content as `char`s and count the cursor in characters;
/// binary files hold raw bytes and count in bytes. A file holds no PyX values;
/// the `std::fs::File` is closed when the object is dropped, and `close()`
/// drops it explicitly (idempotent).
enum FileBody {
    Read(Vec<char>),
    ReadBytes(Vec<u8>),
    Write(std::fs::File),
}

struct FileInner {
    body: Option<FileBody>,
    /// Cursor position: characters for a text read file, bytes for a binary
    /// read file, and the number of characters/bytes written for write/append
    /// mode.
    cursor: usize,
}

/// Shared state of a file object.
///
/// `binary` selects the read/write representation: binary files return and
/// accept `bytes`, text files return and accept `str`. The mutex serialises
/// operations; blocking file I/O is not a runtime safepoint because no runtime
/// lock is held (a blocked read/write only parks the OS thread inside the
/// syscall, and the collector's root scanning does not depend on this object).
pub(crate) struct FileState {
    inner: Mutex<FileInner>,
    path: String,
    /// One of `r`, `w`, `a`, `x`; used by `repr`.
    mode: char,
    binary: bool,
}

impl FileState {
    fn new_read(path: String, content: String) -> FileState {
        FileState {
            inner: Mutex::new(FileInner {
                body: Some(FileBody::Read(content.chars().collect())),
                cursor: 0,
            }),
            path,
            mode: 'r',
            binary: false,
        }
    }

    fn new_read_bytes(path: String, content: Vec<u8>) -> FileState {
        FileState {
            inner: Mutex::new(FileInner {
                body: Some(FileBody::ReadBytes(content)),
                cursor: 0,
            }),
            path,
            mode: 'r',
            binary: true,
        }
    }

    fn new_write(path: String, mode: char, file: std::fs::File, binary: bool) -> FileState {
        FileState {
            inner: Mutex::new(FileInner {
                body: Some(FileBody::Write(file)),
                cursor: 0,
            }),
            path,
            mode,
            binary,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FileInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn mode(&self) -> char {
        self.mode
    }

    pub(crate) fn binary(&self) -> bool {
        self.binary
    }

    pub(crate) fn closed(&self) -> bool {
        self.lock().body.is_none()
    }

    /// Closes the file; calling it again is a no-op.
    pub(crate) fn close(&self) {
        let mut inner = self.lock();
        inner.body = None;
    }

    pub(crate) fn tell(&self) -> Result<usize, FileError> {
        let inner = self.lock();
        if inner.body.is_none() {
            return Err(FileError::Closed);
        }
        Ok(inner.cursor)
    }

    /// Reads up to `limit` characters (all remaining when `None` or
    /// negative).
    pub(crate) fn read(&self, limit: Option<i64>) -> Result<String, FileError> {
        let mut inner = self.lock();
        let FileInner { body, cursor } = &mut *inner;
        let chars = read_chars(body)?;
        let start = *cursor;
        if start >= chars.len() {
            // A seek may position past the end; reads then yield "".
            return Ok(String::new());
        }
        let end = match limit {
            Some(n) if n >= 0 => start
                .saturating_add(usize::try_from(n).unwrap_or(usize::MAX))
                .min(chars.len()),
            _ => chars.len(),
        };
        let text = chars[start..end].iter().copied().collect();
        *cursor = end;
        Ok(text)
    }

    /// Reads up to `limit` bytes (all remaining when `None` or negative).
    ///
    /// Only valid for a binary file; text consumers such as `csv`/`json` call
    /// [`FileState::read`], which rejects binary files instead of decoding
    /// them.
    pub(crate) fn read_bytes(&self, limit: Option<i64>) -> Result<Vec<u8>, FileError> {
        let mut inner = self.lock();
        let FileInner { body, cursor } = &mut *inner;
        let bytes = read_bytes_body(body)?;
        let start = *cursor;
        if start >= bytes.len() {
            // A seek may position past the end; reads then yield b"".
            return Ok(Vec::new());
        }
        let end = match limit {
            Some(n) if n >= 0 => start
                .saturating_add(usize::try_from(n).unwrap_or(usize::MAX))
                .min(bytes.len()),
            _ => bytes.len(),
        };
        let data = bytes[start..end].to_vec();
        *cursor = end;
        Ok(data)
    }

    /// Reads one line including its trailing `\n`. `None` signals end of file
    /// (no characters read); a zero limit returns an empty string while not
    /// at the end, matching CPython's `readline(0)`.
    pub(crate) fn readline(&self, limit: Option<i64>) -> Result<Option<String>, FileError> {
        let mut inner = self.lock();
        let FileInner { body, cursor } = &mut *inner;
        let chars = read_chars(body)?;
        let start = *cursor;
        if start >= chars.len() {
            return Ok(None);
        }
        let max = limit.and_then(|n| {
            if n < 0 {
                None
            } else {
                Some(usize::try_from(n).unwrap_or(usize::MAX))
            }
        });
        if max == Some(0) {
            return Ok(Some(String::new()));
        }
        let mut end = start;
        let mut count = 0usize;
        while end < chars.len() {
            let ch = chars[end];
            end += 1;
            count += 1;
            if ch == '\n' {
                break;
            }
            if let Some(max) = max {
                if count >= max {
                    break;
                }
            }
        }
        let text = chars[start..end].iter().copied().collect();
        *cursor = end;
        Ok(Some(text))
    }

    /// Binary counterpart of [`FileState::readline`]: the limit counts bytes
    /// and the returned line keeps its trailing `\n`.
    pub(crate) fn readline_bytes(&self, limit: Option<i64>) -> Result<Option<Vec<u8>>, FileError> {
        let mut inner = self.lock();
        let FileInner { body, cursor } = &mut *inner;
        let bytes = read_bytes_body(body)?;
        let start = *cursor;
        if start >= bytes.len() {
            return Ok(None);
        }
        let max = limit.and_then(|n| {
            if n < 0 {
                None
            } else {
                Some(usize::try_from(n).unwrap_or(usize::MAX))
            }
        });
        if max == Some(0) {
            return Ok(Some(Vec::new()));
        }
        let mut end = start;
        let mut count = 0usize;
        while end < bytes.len() {
            let byte = bytes[end];
            end += 1;
            count += 1;
            if byte == b'\n' {
                break;
            }
            if let Some(max) = max {
                if count >= max {
                    break;
                }
            }
        }
        let data = bytes[start..end].to_vec();
        *cursor = end;
        Ok(Some(data))
    }

    pub(crate) fn readlines(&self) -> Result<Vec<String>, FileError> {
        let mut lines = Vec::new();
        while let Some(line) = self.readline(None)? {
            lines.push(line);
        }
        Ok(lines)
    }

    pub(crate) fn readlines_bytes(&self) -> Result<Vec<Vec<u8>>, FileError> {
        let mut lines = Vec::new();
        while let Some(line) = self.readline_bytes(None)? {
            lines.push(line);
        }
        Ok(lines)
    }

    /// Writes `text`; returns the number of characters written. Text files
    /// only: writing a `str` into a binary file raises `ValueError`.
    pub(crate) fn write(&self, text: &str) -> Result<usize, FileError> {
        let binary = self.binary;
        let mut inner = self.lock();
        let FileInner { body, cursor } = &mut *inner;
        use std::io::Write as _;
        let file = write_file(body)?;
        if binary {
            return Err(FileError::Value(
                "cannot write text to binary file".to_string(),
            ));
        }
        file.write_all(text.as_bytes())
            .map_err(|error| FileError::Os(error.to_string()))?;
        let count = text.chars().count();
        *cursor = cursor.saturating_add(count);
        Ok(count)
    }

    /// Writes raw bytes; returns the number of bytes written. Binary files
    /// only: writing bytes into a text file raises `ValueError`.
    pub(crate) fn write_bytes(&self, data: &[u8]) -> Result<usize, FileError> {
        let binary = self.binary;
        let mut inner = self.lock();
        let FileInner { body, cursor } = &mut *inner;
        use std::io::Write as _;
        let file = write_file(body)?;
        if !binary {
            return Err(FileError::Value(
                "cannot write bytes to text file".to_string(),
            ));
        }
        file.write_all(data)
            .map_err(|error| FileError::Os(error.to_string()))?;
        let count = data.len();
        *cursor = cursor.saturating_add(count);
        Ok(count)
    }

    pub(crate) fn flush(&self) -> Result<(), FileError> {
        let mut inner = self.lock();
        let FileInner { body, .. } = &mut *inner;
        match body {
            Some(FileBody::Read(_)) | Some(FileBody::ReadBytes(_)) => Ok(()),
            Some(FileBody::Write(file)) => {
                use std::io::Write as _;
                file.flush()
                    .map_err(|error| FileError::Os(error.to_string()))
            }
            None => Err(FileError::Closed),
        }
    }

    /// Repositions the read cursor. `whence` is `0` (start), `1` (current) or
    /// `2` (end). Seeking past the end succeeds (reads then return `""`);
    /// a negative target raises `OSError`, like CPython's text layer.
    pub(crate) fn seek(&self, offset: i64, whence: i64) -> Result<(), FileError> {
        let mut inner = self.lock();
        let FileInner { body, cursor } = &mut *inner;
        let len = body_len(body)?;
        let base = match whence {
            0 => 0,
            1 => i64::try_from(*cursor).unwrap_or(i64::MAX),
            2 => i64::try_from(len).unwrap_or(i64::MAX),
            _ => {
                return Err(FileError::Value(
                    "seek() whence must be 0, 1 or 2".to_string(),
                ))
            }
        };
        let target = base
            .checked_add(offset)
            .ok_or_else(|| FileError::Value("seek() offset is out of range".to_string()))?;
        if target < 0 {
            return Err(FileError::Os("invalid argument".to_string()));
        }
        *cursor = usize::try_from(target).unwrap_or(usize::MAX);
        Ok(())
    }
}

fn read_chars(body: &mut Option<FileBody>) -> Result<&mut Vec<char>, FileError> {
    match body {
        Some(FileBody::Read(chars)) => Ok(chars),
        Some(FileBody::ReadBytes(_)) => Err(FileError::Value(
            "cannot read binary file as text".to_string(),
        )),
        Some(FileBody::Write(_)) => Err(FileError::Os("file not open for reading".to_string())),
        None => Err(FileError::Closed),
    }
}

fn read_bytes_body(body: &mut Option<FileBody>) -> Result<&mut Vec<u8>, FileError> {
    match body {
        Some(FileBody::ReadBytes(bytes)) => Ok(bytes),
        Some(FileBody::Read(_)) => Err(FileError::Value(
            "cannot read text file as bytes".to_string(),
        )),
        Some(FileBody::Write(_)) => Err(FileError::Os("file not open for reading".to_string())),
        None => Err(FileError::Closed),
    }
}

/// Length of a read body in its cursor unit (characters or bytes).
fn body_len(body: &Option<FileBody>) -> Result<usize, FileError> {
    match body {
        Some(FileBody::Read(chars)) => Ok(chars.len()),
        Some(FileBody::ReadBytes(bytes)) => Ok(bytes.len()),
        Some(FileBody::Write(_)) => Err(FileError::Os("file not open for reading".to_string())),
        None => Err(FileError::Closed),
    }
}

fn write_file(body: &mut Option<FileBody>) -> Result<&mut std::fs::File, FileError> {
    match body {
        Some(FileBody::Write(file)) => Ok(file),
        Some(FileBody::Read(_)) | Some(FileBody::ReadBytes(_)) => {
            Err(FileError::Os("file not open for writing".to_string()))
        }
        None => Err(FileError::Closed),
    }
}

/// State of a `csv.reader`/`csv.DictReader` object.
///
/// The whole remaining file content is parsed at construction time. Rows and
/// the optional header are native strings, so the object has no PyX children;
/// yielding a row builds a fresh list (or dict) whose string children the
/// collector then owns.
pub(crate) struct CsvReaderState {
    pub(crate) rows: Vec<Vec<String>>,
    /// `Some` for a `DictReader`: the first record, used as the field names.
    pub(crate) header: Option<Vec<String>>,
    /// Next row to yield. Shared by every iterator over this reader, like a
    /// file cursor; atomic so concurrent iterations stay memory-safe.
    pub(crate) index: AtomicUsize,
}

/// State of a `csv.writer`/`csv.DictWriter` object.
pub(crate) struct CsvWriterState {
    /// Tagged target file; traced as a GC edge so the writer keeps it alive.
    pub(crate) file: i64,
    /// `Some` for a `DictWriter`: the column order and allowed keys.
    pub(crate) fieldnames: Option<Vec<String>>,
}

/// State of an `argparse.Namespace`: attribute names and their values, in
/// declaration order. The parser stores `String` keys, so there are no
/// key edges to trace; every value is a GC edge.
pub(crate) struct NamespaceState {
    pub(crate) pairs: Vec<(String, i64)>,
}

/// Attribute table of a class instance.
///
/// Names are owned strings, so only the values are GC edges. The table has its
/// own mutex: [`Attributes::set`] and the collector's trace both hold it,
/// which serialises an insertion with the marker's black transition without
/// blocking user code (no user call runs under the lock).
pub(crate) struct Attributes {
    entries: Mutex<Vec<(String, i64)>>,
}

impl Attributes {
    fn new() -> Attributes {
        Attributes {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Locks the table, ignoring poisoning: a panic in user code must not make
    /// the instance unusable.
    fn lock(&self) -> MutexGuard<'_, Vec<(String, i64)>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns the value of `name`, in insertion order.
    pub(crate) fn get(&self, name: &str) -> Option<i64> {
        let entries = self.lock();
        entries
            .iter()
            .find(|(existing, _)| existing.as_str() == name)
            .map(|(_, value)| *value)
    }

    /// Sets `name` to `value`, keeping the first insertion position. The
    /// insertion barrier is published while the table is still locked, so a
    /// concurrent marker cannot miss the new edge.
    pub(crate) fn set(&self, owner: i64, name: String, value: i64) {
        let mut entries = self.lock();
        let position = entries
            .iter()
            .position(|(existing, _)| existing.as_str() == name.as_str());
        match position {
            Some(index) => entries[index].1 = value,
            None => entries.push((name, value)),
        }
        crate::heap::write_barrier_edge(owner, value);
    }
}

/// State of a class object: its Python-visible name, its base class (or
/// `NONE`) and the methods collected from `class Name(Base):`.
///
/// A method is an ordinary function value. The namespace is immutable after
/// construction, so lookups need no lock and the collector can read it
/// directly.
pub(crate) struct ClassState {
    pub(crate) name: String,
    pub(crate) base: i64,
    pub(crate) methods: Vec<(String, i64)>,
}

impl ClassState {
    fn method(&self, name: &str) -> Option<i64> {
        self.methods
            .iter()
            .find(|(existing, _)| existing.as_str() == name)
            .map(|(_, function)| *function)
    }
}

/// State of a class instance: the class it was created from and its mutable
/// attribute table.
pub(crate) struct InstanceState {
    pub(crate) class: i64,
    pub(crate) attrs: Attributes,
}

impl InstanceState {
    /// Locks the attribute table for the collector: the guard must stay alive
    /// until the marker turns the instance black.
    pub(crate) fn lock_attrs(&self) -> MutexGuard<'_, Vec<(String, i64)>> {
        self.attrs.lock()
    }
}

/// Callable member of a loaded extension module.
///
/// `module` is the shim's process-wide module handle and `method` its cache
/// index; both are opaque outside `crate::capi`. `name` is the Python-visible
/// attribute, used for `repr` and error messages.
pub(crate) struct ExtFunctionState {
    pub(crate) module: u64,
    pub(crate) method: u32,
    pub(crate) name: String,
}

#[repr(C)]
pub(crate) enum ObjData {
    Str(String),
    Float(f64),
    Exception(ExcData),
    /// Insertion-ordered list elements.
    List(Vec<i64>),
    /// Insertion-ordered `(key, value)` pairs with a hash index over the
    /// keys; duplicate keys are collapsed by the constructor (last value,
    /// first position).
    Dict(DictData),
    /// Active iterator over `target`.
    Iter(Box<IterState>),
    /// Immutable sequence of tagged elements.
    Tuple(Vec<i64>),
    /// Insertion-ordered unique elements (Python equality, all hashable).
    Set(Vec<i64>),
    /// Lazy integer range; holds no PyX values.
    Range {
        start: i64,
        stop: i64,
        step: i64,
    },
    /// Callable function value; wrapper closures trace their captured values.
    Function(FunctionObj),
    /// Binary/reentrant thread lock; holds no PyX values.
    Lock(LockState),
    /// `threading.Thread`; traces target and arguments.
    Thread(ThreadState),
    /// `threading.Event`; holds no PyX values.
    Event(EventState),
    /// `queue.Queue`; traces every queued item.
    Queue(QueueState),
    /// `multiprocessing.Process`; traces target and arguments.
    Process(ProcessState),
    /// `multiprocessing.Queue`; items are pickled bytes, no PyX children.
    MpQueue(MpQueueState),
    /// `multiprocessing.Pool`; traces the per-worker queues.
    Pool(PoolState),
    /// File from `io.open` (text or binary); holds no PyX values.
    File(FileState),
    /// Parsed `csv.reader`/`csv.DictReader`; holds no PyX values.
    CsvReader(CsvReaderState),
    /// `csv.writer`/`csv.DictWriter`; traces its target file.
    CsvWriter(CsvWriterState),
    /// Immutable byte string; holds no PyX values.
    Bytes(Vec<u8>),
    /// `random.Random` generator; holds no PyX values.
    Random(RandomState),
    /// `pathlib.Path`; a plain path string, so no PyX children.
    Path(PathState),
    /// `argparse.ArgumentParser`; traces its option definitions.
    ArgParser(crate::modules::argparse::ArgParserState),
    /// `argparse.Namespace`; traces every attribute value.
    Namespace(NamespaceState),
    /// `collections.defaultdict`; traces the factory and every pair.
    DefaultDict(crate::modules::collections::DefaultDictState),
    /// Compiled `re.Pattern`; no PyX children.
    Pattern(crate::modules::re::PatternState),
    /// `re.Match`; no PyX children.
    Match(crate::modules::re::MatchState),
    /// Blocking TCP socket; no PyX children. Dropping it closes the socket.
    Socket(crate::modules::socket::SocketState),
    /// Class object; traces its base class and every method value.
    Class(ClassState),
    /// Class instance; traces its class and every attribute value.
    Instance(InstanceState),
    /// Stackful generator/coroutine. Traces its arguments and result; the
    /// suspended stack is registered as a separate conservative root.
    Fiber(Box<crate::fiber::FiberState>),
    /// Callable member of a loaded extension module; holds no PyX values.
    ExtFunction(ExtFunctionState),
    /// Opaque pointer capsule from an extension module. The pointer belongs
    /// to the extension; PyX never dereferences or frees it, and the
    /// destructor is not stored here, matching the documented slice
    /// limitation (see [`crate::capi`]).
    Capsule {
        ptr: usize,
        name: Option<String>,
    },
}

// -------------------------------------------------------------------------
// Tag helpers
// -------------------------------------------------------------------------

#[inline]
pub(crate) fn is_int(v: i64) -> bool {
    v & TAG_MASK == TAG_INT
}

#[inline]
pub(crate) fn is_special(v: i64) -> bool {
    v & TAG_MASK == TAG_SPECIAL
}

#[inline]
pub(crate) fn is_heap(v: i64) -> bool {
    v & TAG_MASK == TAG_HEAP
}

/// Decodes an immediate integer, treating `True`/`False` as `1`/`0`
/// (Python's `bool` is numerically an `int`). `None` and heap values yield
/// `None`.
pub(crate) fn as_integer(v: i64) -> Option<i64> {
    if is_int(v) {
        return Some(v >> 3);
    }
    if is_special(v) {
        return match v >> 3 {
            1 => Some(0), // False
            2 => Some(1), // True
            _ => None,
        };
    }
    None
}

/// Encodes a boolean as the `True`/`False` immediates.
#[inline]
pub(crate) fn bool_value(b: bool) -> i64 {
    if b {
        TRUE
    } else {
        FALSE
    }
}

/// Encodes a Python integer as an immediate, or `None` if it does not fit in
/// the 61 bits available to the tag.
pub(crate) fn encode_int(n: i64) -> Option<i64> {
    if (MIN_INT..=MAX_INT).contains(&n) {
        Some(n << 3)
    } else {
        None
    }
}

/// `pyx_int` helper: identity for already-tagged values, otherwise encodes a
/// raw integer literal. Callers following ARCHITECTURE §3.1 always pass a
/// tagged immediate, for which this is exactly the identity.
pub(crate) fn normalize_int(v: i64) -> i64 {
    if is_int(v) {
        v
    } else {
        encode_int(v).unwrap_or(v)
    }
}

/// Numeric view of a value: booleans are `0/1`, heap floats are `Float`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Num {
    Int(i64),
    Float(f64),
}

pub(crate) fn num_of(v: i64) -> Option<Num> {
    if let Some(i) = as_integer(v) {
        return Some(Num::Int(i));
    }
    as_float(v).map(Num::Float)
}

// -------------------------------------------------------------------------
// Heap access
// -------------------------------------------------------------------------

/// Reinterprets a tagged heap value as an object.
///
/// # Safety
///
/// The caller must pass a value whose low bits are [`TAG_HEAP`] and that was
/// produced by this runtime's allocator. The reference is valid while the
/// object is reachable from a runtime root: the conservative collector
/// (ARCHITECTURE §7) never frees reachable objects. The runtime only mutates
/// containers/iterators through the dedicated `*_mut` helpers below, never
/// while an `as_obj` reference is in use.
pub(crate) unsafe fn as_obj(v: i64) -> Option<&'static PyxObj> {
    if v & TAG_MASK != TAG_HEAP {
        return None;
    }
    let addr = (v & !TAG_MASK) as usize;
    if addr == 0 {
        // Malformed value: tag without a pointer.
        return None;
    }
    Some(&*(addr as *const PyxObj))
}

pub(crate) fn as_float(v: i64) -> Option<f64> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted/immutable for the
    // duration of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_FLOAT {
        return None;
    }
    match &obj.data {
        ObjData::Float(f) => Some(*f),
        _ => None,
    }
}

pub(crate) fn as_str(v: i64) -> Option<&'static str> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted/immutable for the
    // duration of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_STR {
        return None;
    }
    match &obj.data {
        ObjData::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Borrows the contents of a `bytes` object. `None` for any other value.
pub(crate) fn as_bytes(v: i64) -> Option<&'static [u8]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted/immutable for the
    // duration of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_BYTES {
        return None;
    }
    match &obj.data {
        ObjData::Bytes(bytes) => Some(bytes.as_slice()),
        _ => None,
    }
}

/// Borrows the PRNG state of a `random.Random` object.
pub(crate) fn as_random(v: i64) -> Option<&'static RandomState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration
    // of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_RANDOM {
        return None;
    }
    match &obj.data {
        ObjData::Random(state) => Some(state),
        _ => None,
    }
}

/// Borrows the payload of a `pathlib.Path` object.
pub(crate) fn as_path(v: i64) -> Option<&'static PathState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration
    // of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_PATH {
        return None;
    }
    match &obj.data {
        ObjData::Path(state) => Some(state),
        _ => None,
    }
}

/// Borrows a path-like operand: a `str` or a `Path` payload.
pub(crate) fn as_path_like(v: i64) -> Option<&'static str> {
    if let Some(text) = as_str(v) {
        return Some(text);
    }
    as_path(v).map(PathState::path)
}

/// Borrows the state of an `argparse.ArgumentParser`.
pub(crate) fn as_arg_parser(v: i64) -> Option<&'static crate::modules::argparse::ArgParserState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration
    // of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_ARG_PARSER {
        return None;
    }
    match &obj.data {
        ObjData::ArgParser(state) => Some(state),
        _ => None,
    }
}

/// Borrows the attribute list of an `argparse.Namespace`.
pub(crate) fn as_namespace(v: i64) -> Option<&'static [(String, i64)]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration
    // of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_NAMESPACE {
        return None;
    }
    match &obj.data {
        ObjData::Namespace(state) => Some(state.pairs.as_slice()),
        _ => None,
    }
}

/// Borrows a `collections.defaultdict` as its pairs plus the factory value.
pub(crate) fn as_defaultdict(v: i64) -> Option<(&'static [(i64, i64)], i64)> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted/immutable for the
    // duration of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_DEFAULTDICT {
        return None;
    }
    match &obj.data {
        ObjData::DefaultDict(state) => Some((state.pairs.as_slice(), state.factory)),
        _ => None,
    }
}

/// Borrows the state of a compiled `re.Pattern`.
pub(crate) fn as_pattern(v: i64) -> Option<&'static crate::modules::re::PatternState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted/immutable for the
    // duration of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_PATTERN {
        return None;
    }
    match &obj.data {
        ObjData::Pattern(state) => Some(state),
        _ => None,
    }
}

/// Borrows the state of an `re.Match`.
pub(crate) fn as_match(v: i64) -> Option<&'static crate::modules::re::MatchState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted/immutable for the
    // duration of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_MATCH {
        return None;
    }
    match &obj.data {
        ObjData::Match(state) => Some(state),
        _ => None,
    }
}

/// Borrows the native state of a `socket` object.
pub(crate) fn as_socket(v: i64) -> Option<&'static crate::modules::socket::SocketState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration
    // of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_SOCKET {
        return None;
    }
    match &obj.data {
        ObjData::Socket(state) => Some(state),
        _ => None,
    }
}

pub(crate) fn exception_data(v: i64) -> Option<&'static ExcData> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted/immutable for the
    // duration of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_EXCEPTION {
        return None;
    }
    match &obj.data {
        ObjData::Exception(e) => Some(e),
        _ => None,
    }
}

/// Borrows the state of a class object.
pub(crate) fn as_class(v: i64) -> Option<&'static ClassState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted/immutable for the
    // duration of this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_CLASS {
        return None;
    }
    match &obj.data {
        ObjData::Class(state) => Some(state),
        _ => None,
    }
}

/// Borrows the state of a class instance.
pub(crate) fn as_instance(v: i64) -> Option<&'static InstanceState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_INSTANCE {
        return None;
    }
    match &obj.data {
        ObjData::Instance(state) => Some(state),
        _ => None,
    }
}

/// The Python-visible name of a class object.
pub(crate) fn class_display_name(v: i64) -> Option<&'static str> {
    as_class(v).map(|state| state.name.as_str())
}

/// The name of the class an instance belongs to.
pub(crate) fn instance_class_name(v: i64) -> Option<&'static str> {
    let state = as_instance(v)?;
    class_display_name(state.class)
}

/// Looks up `name` on `class` and its bases, returning the method value.
///
/// The class namespace is immutable after construction, so bases form a finite
/// chain; the iteration cap is defensive against a malformed heap object.
pub(crate) fn class_method(class: i64, name: &str) -> Option<i64> {
    let mut current = class;
    for _ in 0..1024 {
        let state = as_class(current)?;
        if let Some(function) = state.method(name) {
            return Some(function);
        }
        if state.base == NONE {
            return None;
        }
        current = state.base;
    }
    None
}

/// Reads an instance data attribute. Method attributes are resolved by
/// [`crate::methods::call`] instead.
pub(crate) fn instance_attr(instance: i64, name: &str) -> Option<i64> {
    as_instance(instance)?.attrs.get(name)
}

/// Stores an instance data attribute, preserving its first insertion position.
pub(crate) fn instance_setattr(instance: i64, name: String, value: i64) {
    if let Some(state) = as_instance(instance) {
        state.attrs.set(instance, name, value);
    }
}

/// Guarded kind checks: `None` for every non-heap value and for heap objects
/// of a different kind, so a malformed or mistyped value is never
/// dereferenced as a container.
pub(crate) fn as_list(v: i64) -> Option<&'static [i64]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_LIST {
        return None;
    }
    match &obj.data {
        ObjData::List(items) => Some(items.as_slice()),
        _ => None,
    }
}

/// Borrows a dict's payload (entries plus key index).
pub(crate) fn as_dict_data(v: i64) -> Option<&'static DictData> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_DICT {
        return None;
    }
    match &obj.data {
        ObjData::Dict(dict) => Some(dict),
        _ => None,
    }
}

pub(crate) fn as_dict(v: i64) -> Option<&'static [(i64, i64)]> {
    as_dict_data(v).map(|dict| dict.entries())
}

pub(crate) fn as_tuple(v: i64) -> Option<&'static [i64]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_TUPLE {
        return None;
    }
    match &obj.data {
        ObjData::Tuple(items) => Some(items.as_slice()),
        _ => None,
    }
}

pub(crate) fn as_set(v: i64) -> Option<&'static [i64]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_SET {
        return None;
    }
    match &obj.data {
        ObjData::Set(items) => Some(items.as_slice()),
        _ => None,
    }
}

/// Returns `(start, stop, step)` for a range object.
pub(crate) fn as_range(v: i64) -> Option<(i64, i64, i64)> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_RANGE {
        return None;
    }
    match &obj.data {
        ObjData::Range { start, stop, step } => Some((*start, *stop, *step)),
        _ => None,
    }
}

pub(crate) fn as_function(v: i64) -> Option<&'static FunctionObj> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_FUNCTION {
        return None;
    }
    match &obj.data {
        ObjData::Function(function) => Some(function),
        _ => None,
    }
}

/// Returns the shim handle of an extension-module function value.
pub(crate) fn as_ext_function(v: i64) -> Option<&'static ExtFunctionState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_EXT_FUNCTION {
        return None;
    }
    match &obj.data {
        ObjData::ExtFunction(state) => Some(state),
        _ => None,
    }
}

/// Allocates an opaque pointer capsule.
///
/// `ptr` is owned by the extension and is never dereferenced or freed by PyX;
/// capsules carry no PyX children and have no destructor.
pub(crate) fn capsule_new(ptr: usize, name: Option<String>) -> i64 {
    alloc(KIND_CAPSULE, ObjData::Capsule { ptr, name })
}

/// Returns a capsule's pointer and name, or `None` for other values.
pub(crate) fn as_capsule(v: i64) -> Option<(usize, Option<&'static str>)> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_CAPSULE {
        return None;
    }
    match &obj.data {
        ObjData::Capsule { ptr, name } => Some((*ptr, name.as_deref())),
        _ => None,
    }
}

/// Returns the shared state of a `_thread` lock object. The returned handle is
/// usable from any thread; its internal `Mutex`/`Condvar` do the
/// synchronisation, so no heap lock is involved.
pub(crate) fn as_lock(v: i64) -> Option<&'static LockState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects). The payload is
    // independent of the heap registry and shared through `Mutex`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_LOCK {
        return None;
    }
    match &obj.data {
        ObjData::Lock(lock) => Some(lock),
        _ => None,
    }
}

/// Returns the state of a `threading.Thread` object.
pub(crate) fn as_thread(v: i64) -> Option<&'static ThreadState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration
    // of this call and is shared through atomics/mutexes.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_THREAD {
        return None;
    }
    match &obj.data {
        ObjData::Thread(thread) => Some(thread),
        _ => None,
    }
}

/// Returns the state of a `threading.Event` object.
pub(crate) fn as_event(v: i64) -> Option<&'static EventState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: see `as_thread`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_EVENT {
        return None;
    }
    match &obj.data {
        ObjData::Event(event) => Some(event),
        _ => None,
    }
}

/// Returns the state of a `queue.Queue` object.
pub(crate) fn as_queue(v: i64) -> Option<&'static QueueState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: see `as_thread`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_QUEUE {
        return None;
    }
    match &obj.data {
        ObjData::Queue(queue) => Some(queue),
        _ => None,
    }
}

/// Returns the state of a `multiprocessing.Process` object.
pub(crate) fn as_process(v: i64) -> Option<&'static ProcessState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: see `as_thread`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_PROCESS {
        return None;
    }
    match &obj.data {
        ObjData::Process(process) => Some(process),
        _ => None,
    }
}

/// Returns the state of a `multiprocessing.Queue` object.
pub(crate) fn as_mp_queue(v: i64) -> Option<&'static MpQueueState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: see `as_thread`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_MPQUEUE {
        return None;
    }
    match &obj.data {
        ObjData::MpQueue(queue) => Some(queue),
        _ => None,
    }
}

/// Returns the state of a `multiprocessing.Pool` object.
pub(crate) fn as_pool(v: i64) -> Option<&'static PoolState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: see `as_thread`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_POOL {
        return None;
    }
    match &obj.data {
        ObjData::Pool(pool) => Some(pool),
        _ => None,
    }
}

/// Returns the state of a text file object (`io.open`).
pub(crate) fn as_file(v: i64) -> Option<&'static FileState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: see `as_thread`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_FILE {
        return None;
    }
    match &obj.data {
        ObjData::File(file) => Some(file),
        _ => None,
    }
}

/// Returns the state of a `csv.reader`/`csv.DictReader` object.
pub(crate) fn as_csv_reader(v: i64) -> Option<&'static CsvReaderState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: see `as_thread`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_CSVREADER {
        return None;
    }
    match &obj.data {
        ObjData::CsvReader(reader) => Some(reader),
        _ => None,
    }
}

/// Returns the state of a `csv.writer`/`csv.DictWriter` object.
pub(crate) fn as_csv_writer(v: i64) -> Option<&'static CsvWriterState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: see `as_thread`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_CSVWRITER {
        return None;
    }
    match &obj.data {
        ObjData::CsvWriter(writer) => Some(writer),
        _ => None,
    }
}

pub(crate) fn as_iter(v: i64) -> Option<&'static IterState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_ITER {
        return None;
    }
    match &obj.data {
        ObjData::Iter(state) => Some(state.as_ref()),
        _ => None,
    }
}

/// Borrows the fiber state behind a generator/coroutine object.
pub(crate) fn as_fiber(v: i64) -> Option<&'static crate::fiber::FiberState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked above; the object is rooted for the duration of
    // this call (the collector only frees unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_FIBER {
        return None;
    }
    match &obj.data {
        ObjData::Fiber(state) => Some(state.as_ref()),
        _ => None,
    }
}

/// Exclusive access to a fiber's state. Used by the driver and the iterator
/// integration; the fiber is owned by the executing program.
pub(crate) fn fiber_state_mut(v: i64) -> Option<&'static mut crate::fiber::FiberState> {
    if !is_heap(v) || (v & !TAG_MASK) == 0 {
        return None;
    }
    // SAFETY: heap tag checked above; the fiber is rooted by its caller.
    let obj = unsafe { &mut *((v & !TAG_MASK) as *mut PyxObj) };
    if obj.kind != KIND_FIBER {
        return None;
    }
    match &mut obj.data {
        ObjData::Fiber(state) => Some(state.as_mut()),
        _ => None,
    }
}

/// Raw address of a fiber payload, or `None` for non-fiber values.
pub(crate) fn fiber_raw(v: i64) -> Option<*mut crate::fiber::FiberState> {
    fiber_state_mut(v).map(|state| state as *mut crate::fiber::FiberState)
}

/// Exclusive, graph-locked access to a container payload.
///
/// The guard holds the collector's graph lock for the whole borrow: container
/// mutators and the concurrent marker are serialised by it, so the marker
/// never observes a half-updated payload. Callers that insert a new child
/// value publish an insertion barrier ([`crate::heap::write_barrier_edge`])
/// while the guard is still held, so the edge cannot race with the marker's
/// black transition; the guard itself only releases the graph lock on drop.
///
/// Rules for callers: no PyX heap allocation, no safepoint and no user code
/// while the guard is alive. A collection started by this thread would
/// otherwise wait for a graph lock this thread still holds.
pub(crate) struct ContainerGuard<T: 'static> {
    data: &'static mut T,
    /// Insertion barriers are published by the mutation site while this guard
    /// is alive. Once the shared heap can be marked concurrently the graph
    /// lock serialises the payload; in local (single-threaded) mode it is
    /// skipped because the only collector runs on this same thread.
    _graph: GraphGuard,
}

/// Graph-lock variant held by a container guard.
pub(crate) enum GraphGuard {
    /// Local heap: the only collector runs on this thread.
    Local,
    /// Shared heap: a concurrent marker may inspect the payload.
    Shared { _lock: MutexGuard<'static, ()> },
}

impl<T> ContainerGuard<T> {
    fn new(data: &'static mut T, graph: GraphGuard) -> ContainerGuard<T> {
        ContainerGuard {
            data,
            _graph: graph,
        }
    }
}

impl<T> Deref for ContainerGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &*self.data
    }
}

impl<T> DerefMut for ContainerGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut *self.data
    }
}

/// Takes the graph lock and validates that `v` is a heap object of `kind`.
fn lock_container(v: i64, kind: u32) -> Option<GraphGuard> {
    if !is_heap(v) || (v & !TAG_MASK) == 0 {
        return None;
    }
    let graph = if crate::heap::shared_mode() {
        GraphGuard::Shared {
            _lock: crate::heap::graph_lock(),
        }
    } else {
        GraphGuard::Local
    };
    // SAFETY: heap tag checked above; the object is rooted for the duration
    // of the borrow (the collector only frees unreachable objects).
    let obj = unsafe { &*((v & !TAG_MASK) as *const PyxObj) };
    if obj.kind != kind {
        return None;
    }
    Some(graph)
}

/// Returns exclusive access to a list's elements.
///
/// # Safety
///
/// Only the runtime calls this, and only for values that were just created or
/// are owned by the executing program. The object is kept alive by the
/// executing program's roots, so the reference is valid for the duration of
/// the ABI call that obtained it.
///
/// With `_thread`, two user threads can call this for the same list. The
/// runtime does not add a lock around container payloads (no GIL): the
/// programmer must serialise shared mutation with a `_thread` lock, exactly as
/// documented for the `_thread` module. The guard does hold the collector's
/// graph lock, so a concurrent marking pass never reads a payload mid-update.
pub(crate) fn list_items_mut(v: i64) -> Option<ContainerGuard<Vec<i64>>> {
    let graph = lock_container(v, KIND_LIST)?;
    // SAFETY: heap tag and kind were checked; the value is owned by the
    // executing program, see the function contract.
    let obj: &'static mut PyxObj = unsafe { &mut *((v & !TAG_MASK) as *mut PyxObj) };
    match &mut obj.data {
        ObjData::List(items) => Some(ContainerGuard::new(items, graph)),
        _ => None,
    }
}

/// Returns exclusive access to a dict's payload. See [`list_items_mut`].
pub(crate) fn dict_pairs_mut(v: i64) -> Option<ContainerGuard<DictData>> {
    let graph = lock_container(v, KIND_DICT)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut PyxObj = unsafe { &mut *((v & !TAG_MASK) as *mut PyxObj) };
    match &mut obj.data {
        ObjData::Dict(dict) => Some(ContainerGuard::new(dict, graph)),
        _ => None,
    }
}

/// Returns exclusive access to a set's elements. See [`list_items_mut`].
pub(crate) fn set_items_mut(v: i64) -> Option<ContainerGuard<Vec<i64>>> {
    let graph = lock_container(v, KIND_SET)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut PyxObj = unsafe { &mut *((v & !TAG_MASK) as *mut PyxObj) };
    match &mut obj.data {
        ObjData::Set(items) => Some(ContainerGuard::new(items, graph)),
        _ => None,
    }
}

/// Returns exclusive access to an `argparse.ArgumentParser`'s definitions.
pub(crate) fn arg_parser_mut(
    v: i64,
) -> Option<ContainerGuard<crate::modules::argparse::ArgParserState>> {
    let graph = lock_container(v, KIND_ARG_PARSER)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut PyxObj = unsafe { &mut *((v & !TAG_MASK) as *mut PyxObj) };
    match &mut obj.data {
        ObjData::ArgParser(state) => Some(ContainerGuard::new(state, graph)),
        _ => None,
    }
}

/// Returns exclusive access to a `collections.defaultdict`'s pairs/factory.
pub(crate) fn defaultdict_state_mut(
    v: i64,
) -> Option<ContainerGuard<crate::modules::collections::DefaultDictState>> {
    let graph = lock_container(v, KIND_DEFAULTDICT)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut PyxObj = unsafe { &mut *((v & !TAG_MASK) as *mut PyxObj) };
    match &mut obj.data {
        ObjData::DefaultDict(state) => Some(ContainerGuard::new(state, graph)),
        _ => None,
    }
}

/// Returns exclusive access to iterator state. See [`list_items_mut`].
pub(crate) fn iter_state_mut(v: i64) -> Option<&'static mut IterState> {
    if !is_heap(v) || (v & !TAG_MASK) == 0 {
        return None;
    }
    // SAFETY: see `list_items_mut`; iterators are created per loop and are
    // only advanced by the runtime.
    let obj = unsafe { &mut *((v & !TAG_MASK) as *mut PyxObj) };
    if obj.kind != KIND_ITER {
        return None;
    }
    match &mut obj.data {
        ObjData::Iter(state) => Some(state.as_mut()),
        _ => None,
    }
}

/// Clones the exception payload behind a tagged value (used by tracebacks so
/// that the thread-local state can be cleared afterwards).
pub(crate) fn exception_clone(v: i64) -> Option<ExcData> {
    exception_data(v).cloned()
}

// -------------------------------------------------------------------------
// Allocation (interim conservative GC, ARCHITECTURE §7)
// -------------------------------------------------------------------------

/// Allocates a heap object owned by the collector.
///
/// The allocation, registration and any policy-driven collection happen in
/// [`crate::heap::allocate`] (under the global-heap lock once `_thread` has
/// switched to the shared heap).
///
/// Audit note: an `&str`/`&ObjData`/`&mut` reference into *another* heap
/// object is a plain pointer, not a tagged root, so it must not be read
/// after an allocation that may collect. All current call sites finish
/// reading their inputs before calling an allocator and only retain owned
/// Rust data (e.g. the `String`/`Vec` handed to this function) afterwards.
fn alloc(kind: u32, data: ObjData) -> i64 {
    crate::heap::allocate(kind, data)
}

pub(crate) fn str_alloc(s: String) -> i64 {
    alloc(KIND_STR, ObjData::Str(s))
}

/// Copies `bytes` into a fresh string object. Invalid UTF-8 is replaced
/// defensively (the ABI only ever carries UTF-8).
pub(crate) fn str_from_bytes(bytes: &[u8]) -> i64 {
    str_alloc(String::from_utf8_lossy(bytes).into_owned())
}

/// Copies `len` bytes at `ptr` into a string object. `NULL` yields `""`.
pub(crate) fn str_from_ptr(ptr: *const u8, len: usize) -> i64 {
    if ptr.is_null() || len == 0 {
        return str_alloc(String::new());
    }
    if len > isize::MAX as usize {
        // Defensive: not a valid Rust slice length.
        return str_alloc(String::new());
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    str_from_bytes(bytes)
}

pub(crate) fn float_new(f: f64) -> i64 {
    alloc(KIND_FLOAT, ObjData::Float(f))
}

/// Allocates a `bytes` object from an owned buffer.
pub(crate) fn bytes_new(bytes: Vec<u8>) -> i64 {
    alloc(KIND_BYTES, ObjData::Bytes(bytes))
}

/// Allocates a `random.Random` object. `seed == None` draws entropy.
pub(crate) fn random_new(seed: Option<u64>) -> i64 {
    let state = match seed {
        Some(seed) => RandomState::seeded(seed),
        None => RandomState::from_entropy(),
    };
    alloc(KIND_RANDOM, ObjData::Random(state))
}

/// Allocates a `pathlib.Path` from an owned path string.
pub(crate) fn path_new(path: String) -> i64 {
    alloc(KIND_PATH, ObjData::Path(PathState { path }))
}

pub(crate) fn arg_parser_new(prog: Option<String>, description: Option<String>) -> i64 {
    alloc(
        KIND_ARG_PARSER,
        ObjData::ArgParser(crate::modules::argparse::ArgParserState {
            prog,
            description,
            options: Vec::new(),
        }),
    )
}

pub(crate) fn namespace_new(pairs: Vec<(String, i64)>) -> i64 {
    alloc(KIND_NAMESPACE, ObjData::Namespace(NamespaceState { pairs }))
}

/// Allocates a class object. `base` is `NONE` or an existing class; methods
/// are function values in declaration order (duplicates already collapsed by
/// the ABI entry point).
pub(crate) fn class_new(name: String, base: i64, methods: Vec<(String, i64)>) -> i64 {
    alloc(
        KIND_CLASS,
        ObjData::Class(ClassState {
            name,
            base,
            methods,
        }),
    )
}

/// Allocates an instance of `class` with an empty attribute table.
pub(crate) fn instance_new(class: i64) -> i64 {
    alloc(
        KIND_INSTANCE,
        ObjData::Instance(InstanceState {
            class,
            attrs: Attributes::new(),
        }),
    )
}

pub(crate) fn defaultdict_new(factory: i64, pairs: Vec<(i64, i64)>) -> i64 {
    alloc(
        KIND_DEFAULTDICT,
        ObjData::DefaultDict(crate::modules::collections::DefaultDictState { factory, pairs }),
    )
}

pub(crate) fn pattern_new(source: String) -> i64 {
    alloc(
        KIND_PATTERN,
        ObjData::Pattern(crate::modules::re::PatternState { source }),
    )
}

pub(crate) fn match_new(pattern: String, text: String, groups: Vec<Option<(usize, usize)>>) -> i64 {
    alloc(
        KIND_MATCH,
        ObjData::Match(crate::modules::re::MatchState {
            pattern,
            text,
            groups,
        }),
    )
}

/// Joins a path-like `part` onto `base` with pathlib semantics: an absolute
/// part replaces the base, `"."` (or an empty base) is transparent, and an
/// empty part leaves the base unchanged.
pub(crate) fn join_paths(base: &str, part: &str) -> String {
    if part.starts_with('/') {
        return part.to_string();
    }
    if part.is_empty() {
        return base.to_string();
    }
    if base.is_empty() || base == "." {
        return part.to_string();
    }
    if base.ends_with('/') {
        format!("{base}{part}")
    } else {
        format!("{base}/{part}")
    }
}

/// Copies `bytes` into a fresh `bytes` object.
pub(crate) fn bytes_from_slice(bytes: &[u8]) -> i64 {
    bytes_new(bytes.to_vec())
}

/// Copies `len` bytes at `ptr` into a bytes object. `NULL` or a zero length
/// yields `b""`; an unrepresentable length is treated defensively as empty.
pub(crate) fn bytes_from_ptr(ptr: *const u8, len: usize) -> i64 {
    if ptr.is_null() || len == 0 {
        return bytes_new(Vec::new());
    }
    if len > isize::MAX as usize {
        return bytes_new(Vec::new());
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    bytes_from_slice(bytes)
}

pub(crate) fn list_new(items: Vec<i64>) -> i64 {
    alloc(KIND_LIST, ObjData::List(items))
}

pub(crate) fn dict_new(pairs: Vec<(i64, i64)>) -> i64 {
    dict_new_data(DictData::new(pairs))
}

/// Allocates a dict from an already indexed payload (the constructor path,
/// which deduplicates while building the index).
pub(crate) fn dict_new_data(data: DictData) -> i64 {
    alloc(KIND_DICT, ObjData::Dict(data))
}

pub(crate) fn tuple_new(items: Vec<i64>) -> i64 {
    alloc(KIND_TUPLE, ObjData::Tuple(items))
}

pub(crate) fn set_new(items: Vec<i64>) -> i64 {
    alloc(KIND_SET, ObjData::Set(items))
}

pub(crate) fn range_new(start: i64, stop: i64, step: i64) -> i64 {
    alloc(KIND_RANGE, ObjData::Range { start, stop, step })
}

pub(crate) fn iter_new(target: i64) -> i64 {
    alloc(
        KIND_ITER,
        ObjData::Iter(Box::new(IterState { target, index: 0 })),
    )
}

/// Allocates a function value. `wrapper` is the generated entry point and
/// must be non-zero; `name` is the Python-visible name.
pub(crate) fn function_new(wrapper: usize, arity: usize, name: String) -> i64 {
    function_new_with_flags(wrapper, arity, name, 0)
}

/// Allocates a function value carrying the given generator/coroutine flags.
pub(crate) fn function_new_with_flags(
    wrapper: usize,
    arity: usize,
    name: String,
    flags: u32,
) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Wrapper {
                entry: wrapper,
                captures: Vec::new(),
            },
            arity,
            name,
            flags,
        }),
    )
}

/// Allocates a closure value carrying the given generator/coroutine flags.
pub(crate) fn closure_new_with_flags(
    wrapper: usize,
    arity: usize,
    name: String,
    captures: Vec<i64>,
    flags: u32,
) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Wrapper {
                entry: wrapper,
                captures,
            },
            arity,
            name,
            flags,
        }),
    )
}

/// Allocates a native module function value. `arity` is the upper bound from
/// `pyx_ir::stdlib`; the actual call arity is validated by the module
/// dispatcher against the central spec.
pub(crate) fn native_function_new(module: &'static str, name: &'static str, arity: usize) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Native { module, name },
            arity,
            name: name.to_string(),
            flags: 0,
        }),
    )
}

/// Allocates a builtin function value. Builtins validate their own arity, so
/// `arity` is `usize::MAX` (variadic); callers never rely on it.
pub(crate) fn builtin_function_new(builtin: pyx_ir::Builtin) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Builtin(builtin),
            arity: usize::MAX,
            name: builtin.name().to_string(),
            flags: 0,
        }),
    )
}

/// Allocates a callable member of a loaded extension module.
pub(crate) fn ext_function_new(module: u64, method: u32, name: String) -> i64 {
    alloc(
        KIND_EXT_FUNCTION,
        ObjData::ExtFunction(ExtFunctionState {
            module,
            method,
            name,
        }),
    )
}

/// Creates a lazy generator object. No body code runs until the first
/// `pyx_iter_next`.
pub(crate) fn fiber_generator_new(entry: usize, name: String, args: Vec<i64>) -> Option<i64> {
    fiber_alloc(crate::fiber::FiberKind::Generator, entry, name, args)
}

/// Creates a coroutine object. No body code runs until the first `await`.
pub(crate) fn fiber_coroutine_new(entry: usize, name: String, args: Vec<i64>) -> Option<i64> {
    fiber_alloc(crate::fiber::FiberKind::Coroutine, entry, name, args)
}

fn fiber_alloc(
    kind: crate::fiber::FiberKind,
    entry: usize,
    name: String,
    args: Vec<i64>,
) -> Option<i64> {
    let state = crate::fiber::FiberState::new(kind, entry, name, args)?;
    Some(alloc(KIND_FIBER, ObjData::Fiber(state)))
}

pub(crate) fn exception_new(data: ExcData) -> i64 {
    alloc(KIND_EXCEPTION, ObjData::Exception(data))
}

/// Allocates a `_thread` lock object (`locked() == False`).
pub(crate) fn lock_new() -> i64 {
    alloc(KIND_LOCK, ObjData::Lock(LockState::new()))
}

/// Allocates a reentrant lock (`threading.RLock`).
pub(crate) fn lock_new_reentrant() -> i64 {
    alloc(KIND_LOCK, ObjData::Lock(LockState::new_reentrant()))
}

/// Allocates a `threading.Thread` object. `args` is immutable afterwards.
pub(crate) fn thread_new(target: i64, args: Vec<i64>, name: String) -> i64 {
    alloc(
        KIND_THREAD,
        ObjData::Thread(ThreadState::new(target, args, name)),
    )
}

/// Allocates a `threading.Event` object.
pub(crate) fn event_new() -> i64 {
    alloc(KIND_EVENT, ObjData::Event(EventState::new()))
}

/// Allocates a `queue.Queue` object. `maxsize <= 0` means unbounded.
///
/// The tagged value is installed on the queue state right after the
/// allocation so that a later push can publish an insertion barrier for its
/// item. No allocation and no safepoint happen between the two statements, so
/// the queue is never traced with a missing owner tag.
pub(crate) fn queue_new(maxsize: usize) -> i64 {
    let value = alloc(KIND_QUEUE, ObjData::Queue(QueueState::new(maxsize)));
    if let Some(queue) = as_queue(value) {
        queue.set_owner(value);
    }
    value
}

/// Allocates a `multiprocessing.Process` object. `args` is immutable.
pub(crate) fn process_new(target: i64, args: Vec<i64>, name: String) -> i64 {
    alloc(
        KIND_PROCESS,
        ObjData::Process(ProcessState::new(target, args, name)),
    )
}

/// Allocates a `multiprocessing.Queue` object around an inherited socketpair.
pub(crate) fn mp_queue_new(read_fd: i32, write_fd: i32, maxsize: usize) -> i64 {
    alloc(
        KIND_MPQUEUE,
        ObjData::MpQueue(MpQueueState::new(read_fd, write_fd, maxsize)),
    )
}

/// Rebuilds a `multiprocessing.Queue` from descriptors inherited across the
/// `exec` performed by the spawn start method; no new socketpair is created.
pub(crate) fn mp_queue_from_fds(read_fd: i32, write_fd: i32) -> i64 {
    mp_queue_new(read_fd, write_fd, 0)
}

/// Allocates a `multiprocessing.Pool` object. Both queue vectors are immutable
/// afterwards and are traced as GC edges.
pub(crate) fn pool_new(task_queues: Vec<i64>, result_queues: Vec<i64>) -> i64 {
    alloc(
        KIND_POOL,
        ObjData::Pool(PoolState::new(task_queues, result_queues)),
    )
}

/// Allocates a `socket` object around native socket state.
pub(crate) fn socket_new(state: crate::modules::socket::SocketState) -> i64 {
    alloc(KIND_SOCKET, ObjData::Socket(state))
}

/// Allocates a text read-mode file object with the content already decoded.
pub(crate) fn file_new_read(path: String, content: String) -> i64 {
    alloc(KIND_FILE, ObjData::File(FileState::new_read(path, content)))
}

/// Allocates a binary read-mode file object holding the raw file content.
pub(crate) fn file_new_read_bytes(path: String, content: Vec<u8>) -> i64 {
    alloc(
        KIND_FILE,
        ObjData::File(FileState::new_read_bytes(path, content)),
    )
}

/// Allocates a write/append-mode file object around an open OS handle.
pub(crate) fn file_new_write(path: String, mode: char, file: std::fs::File, binary: bool) -> i64 {
    alloc(
        KIND_FILE,
        ObjData::File(FileState::new_write(path, mode, file, binary)),
    )
}

/// Allocates a `csv.reader` from already parsed rows. `header` is `Some` for
/// a `DictReader`.
pub(crate) fn csv_reader_new(rows: Vec<Vec<String>>, header: Option<Vec<String>>) -> i64 {
    alloc(
        KIND_CSVREADER,
        ObjData::CsvReader(CsvReaderState {
            rows,
            header,
            index: AtomicUsize::new(0),
        }),
    )
}

/// Allocates a `csv.writer` around a file object. `fieldnames` is `Some` for
/// a `DictWriter`.
pub(crate) fn csv_writer_new(file: i64, fieldnames: Option<Vec<String>>) -> i64 {
    alloc(
        KIND_CSVWRITER,
        ObjData::CsvWriter(CsvWriterState { file, fieldnames }),
    )
}

// -------------------------------------------------------------------------
// Python-visible semantics
// -------------------------------------------------------------------------

/// Number of values in `range(start, stop, step)`, saturating at `i64::MAX`.
///
/// The computation is done in `i128` so no intermediate can overflow; a
/// degenerate `step == 0` is treated as an empty range.
pub(crate) fn range_len(start: i64, stop: i64, step: i64) -> i64 {
    if step == 0 {
        return 0;
    }
    let (start, stop, step) = (start as i128, stop as i128, step as i128);
    let length = if step > 0 {
        if start >= stop {
            0
        } else {
            (stop - start + step - 1) / step
        }
    } else if start <= stop {
        0
    } else {
        (start - stop + (-step) - 1) / (-step)
    };
    i64::try_from(length).unwrap_or(i64::MAX)
}

/// The `index`-th value of `range(start, stop, step)`, or `None` when the
/// index is past the end. Arithmetic is done in `i128`; out-of-range
/// intermediates are reported as the end of the range.
pub(crate) fn range_item(start: i64, stop: i64, step: i64, index: usize) -> Option<i64> {
    if step == 0 {
        return None;
    }
    let value = (start as i128).checked_add((index as i128).checked_mul(step as i128)?)?;
    let in_bounds = if step > 0 {
        value < stop as i128
    } else {
        value > stop as i128
    };
    if in_bounds {
        i64::try_from(value).ok()
    } else {
        None
    }
}

/// `repr()` of a range, matching Python (`range(0, 5)` for step 1).
pub(crate) fn range_repr(start: i64, stop: i64, step: i64) -> String {
    if step == 1 {
        format!("range({start}, {stop})")
    } else {
        format!("range({start}, {stop}, {step})")
    }
}

/// Python type name, used in error messages.
pub(crate) fn type_name(v: i64) -> &'static str {
    if is_int(v) {
        return "int";
    }
    if is_special(v) {
        return match v >> 3 {
            0 => "NoneType",
            1 | 2 => "bool",
            _ => "object",
        };
    }
    if is_heap(v) {
        // SAFETY: heap tag checked above; the object is rooted for the duration of
        // this call (the collector only frees unreachable objects).
        return match unsafe { as_obj(v) }.map(|o| &o.data) {
            Some(ObjData::Str(_)) => "str",
            Some(ObjData::Float(_)) => "float",
            Some(ObjData::Exception(e)) => e.name,
            Some(ObjData::List(_)) => "list",
            Some(ObjData::Dict(_)) => "dict",
            Some(ObjData::Iter(_)) => "iterator",
            Some(ObjData::Tuple(_)) => "tuple",
            Some(ObjData::Set(_)) => "set",
            Some(ObjData::Range { .. }) => "range",
            Some(ObjData::Function(_)) => "function",
            Some(ObjData::Lock(_)) => "lock",
            Some(ObjData::Thread(_)) => "Thread",
            Some(ObjData::Event(_)) => "Event",
            Some(ObjData::Queue(_)) => "Queue",
            Some(ObjData::Process(_)) => "Process",
            Some(ObjData::MpQueue(_)) => "Queue",
            Some(ObjData::Pool(_)) => "Pool",
            Some(ObjData::File(_)) => "file",
            Some(ObjData::CsvReader(_)) => "csv.reader",
            Some(ObjData::CsvWriter(_)) => "csv.writer",
            Some(ObjData::Bytes(_)) => "bytes",
            Some(ObjData::Random(_)) => "Random",
            Some(ObjData::Path(_)) => "Path",
            Some(ObjData::ArgParser(_)) => "ArgumentParser",
            Some(ObjData::Namespace(_)) => "Namespace",
            Some(ObjData::DefaultDict(_)) => "defaultdict",
            Some(ObjData::Pattern(_)) => "Pattern",
            Some(ObjData::Match(_)) => "Match",
            Some(ObjData::Socket(_)) => "socket",
            Some(ObjData::Class(_)) => "type",
            Some(ObjData::Instance(_)) => "instance",
            Some(ObjData::ExtFunction(_)) => "builtin_function_or_method",
            Some(ObjData::Capsule { .. }) => "PyCapsule",
            Some(ObjData::Fiber(state)) => match state.kind {
                crate::fiber::FiberKind::Generator => "generator",
                crate::fiber::FiberKind::Coroutine => "coroutine",
            },
            None => "object",
        };
    }
    "object"
}

/// Python-style type name that resolves class objects and instances to their
/// Python-visible class name. Used in user-facing error messages; most callers
/// only need the static [`type_name`].
pub(crate) fn type_name_of(v: i64) -> String {
    if let Some(name) = instance_class_name(v) {
        return name.to_string();
    }
    if as_instance(v).is_some() {
        return "instance".to_string();
    }
    type_name(v).to_string()
}

pub(crate) fn truthy(v: i64) -> bool {
    if is_int(v) {
        return (v >> 3) != 0;
    }
    if is_special(v) {
        return matches!(v >> 3, 2);
    }
    if is_heap(v) {
        // SAFETY: heap tag checked above; the object is rooted for the duration of
        // this call (the collector only frees unreachable objects).
        return match unsafe { as_obj(v) }.map(|o| &o.data) {
            Some(ObjData::Str(s)) => !s.is_empty(),
            Some(ObjData::Float(f)) => *f != 0.0,
            Some(ObjData::Exception(_)) => true,
            Some(ObjData::List(items)) => !items.is_empty(),
            Some(ObjData::Dict(pairs)) => !pairs.is_empty(),
            Some(ObjData::Iter(_)) => true,
            Some(ObjData::Tuple(items)) => !items.is_empty(),
            Some(ObjData::Set(items)) => !items.is_empty(),
            Some(ObjData::Bytes(bytes)) => !bytes.is_empty(),
            Some(ObjData::Range { start, stop, step }) => range_len(*start, *stop, *step) > 0,
            Some(ObjData::Function(_)) => true,
            // Like CPython, a lock object is always truthy.
            Some(ObjData::Lock(_)) => true,
            // Synchronisation objects are always truthy.
            Some(ObjData::Thread(_))
            | Some(ObjData::Event(_))
            | Some(ObjData::Queue(_))
            | Some(ObjData::Process(_))
            | Some(ObjData::MpQueue(_))
            | Some(ObjData::Pool(_))
            | Some(ObjData::File(_))
            | Some(ObjData::CsvReader(_))
            | Some(ObjData::CsvWriter(_)) => true,
            // A generator object is always truthy.
            Some(ObjData::Random(_)) => true,
            // Like CPython, a path is always truthy.
            Some(ObjData::Path(_)) => true,
            // Parser objects and namespaces are always truthy.
            Some(ObjData::ArgParser(_)) | Some(ObjData::Namespace(_)) => true,
            // A defaultdict is truthy exactly when it has entries, like dict.
            Some(ObjData::DefaultDict(state)) => !state.pairs.is_empty(),
            Some(ObjData::Pattern(_)) | Some(ObjData::Match(_)) => true,
            // A socket object is always truthy.
            Some(ObjData::Socket(_)) => true,
            // Classes and instances are always truthy (there is no `__bool__`).
            Some(ObjData::Class(_)) | Some(ObjData::Instance(_)) => true,
            // Generator and coroutine objects are always truthy.
            Some(ObjData::Fiber(_)) => true,
            // Function objects are always truthy.
            Some(ObjData::ExtFunction(_)) => true,
            // Capsules are always truthy, like CPython.
            Some(ObjData::Capsule { .. }) => true,
            None => true,
        };
    }
    // Reserved tags are treated defensively as falsy.
    false
}

/// Deterministic default rendering of an instance: `<ClassName object>`.
/// The heap address is deliberately omitted so output is reproducible.
fn instance_default_repr(state: &InstanceState) -> String {
    let name = as_class(state.class)
        .map(|class| class.name.as_str())
        .unwrap_or("instance");
    format!("<{name} object>")
}

/// CPython-style identity rendering of a generator/coroutine object.
fn fiber_repr(state: &crate::fiber::FiberState) -> String {
    let (kind, fallback) = match state.kind {
        crate::fiber::FiberKind::Generator => ("generator", "<generator object>"),
        crate::fiber::FiberKind::Coroutine => ("coroutine", "<coroutine object>"),
    };
    if state.name.is_empty() {
        fallback.to_string()
    } else {
        format!("<{} object {}>", kind, state.name)
    }
}

/// `str()` of any runtime value (used by `print` and `pyx_to_str`).
///
/// Containers use Python's `repr` (which equals `str` for lists/dicts/tuples):
/// `[1, 'a']`, `{'k': 1}`, `(1, 'a')`.
pub(crate) fn to_string(v: i64) -> String {
    if is_int(v) {
        return (v >> 3).to_string();
    }
    if is_special(v) {
        return match v >> 3 {
            0 => "None".to_string(),
            1 => "False".to_string(),
            2 => "True".to_string(),
            // Malformed special payload: fall back to a stable rendering.
            _ => "None".to_string(),
        };
    }
    if is_heap(v) {
        // SAFETY: heap tag checked above; the object is rooted for the duration of
        // this call (the collector only frees unreachable objects).
        if let Some(obj) = unsafe { as_obj(v) } {
            return match &obj.data {
                ObjData::Str(s) => s.clone(),
                ObjData::Float(f) => format_float(*f),
                // `str(exception)` is its message (Python behaviour).
                ObjData::Exception(e) => e.msg.clone(),
                ObjData::List(_) | ObjData::Dict(_) | ObjData::Tuple(_) | ObjData::Set(_) => {
                    repr_depth(v, 0)
                }
                ObjData::Range { .. } => repr_depth(v, 0),
                ObjData::Function(function) => format!("<function {}>", function.name),
                ObjData::Lock(_) => "<lock object>".to_string(),
                ObjData::Iter(_) => "<iterator>".to_string(),
                ObjData::Thread(thread) => {
                    let state = if thread.started.load(std::sync::atomic::Ordering::Acquire) {
                        "started"
                    } else {
                        "initial"
                    };
                    format!("<Thread({}, {state})>", thread.name)
                }
                ObjData::Event(event) => {
                    if event.is_set() {
                        "<Event set>".to_string()
                    } else {
                        "<Event unset>".to_string()
                    }
                }
                ObjData::Queue(queue) => format!("<Queue size={}>", queue.qsize()),
                // Listing the peer's items would require reading the socket;
                // render the metadata only. `maxsize` is informational for a
                // cross-process queue (see `process`).
                ObjData::MpQueue(queue) => format!("<Queue maxsize={}>", queue.maxsize),
                ObjData::Pool(pool) => {
                    format!("<Pool({} processes)>", pool.size())
                }
                ObjData::Process(process) => {
                    let state = if process.started.load(std::sync::atomic::Ordering::Acquire) {
                        "started"
                    } else {
                        "initial"
                    };
                    format!("<Process({}, {state})>", process.name)
                }
                ObjData::File(file) => {
                    if file.binary() {
                        format!("<file name='{}' mode='{}b'>", file.path(), file.mode())
                    } else {
                        format!(
                            "<file name='{}' mode='{}' encoding='UTF-8'>",
                            file.path(),
                            file.mode()
                        )
                    }
                }
                ObjData::CsvReader(_) => "<csv.reader>".to_string(),
                ObjData::CsvWriter(_) => "<csv.writer>".to_string(),
                // `str(bytes)` is its `repr`, like CPython.
                ObjData::Bytes(bytes) => quote_bytes(bytes),
                ObjData::Random(_) => "<random.Random>".to_string(),
                // `str(Path)` is the path itself.
                ObjData::Path(state) => state.path.clone(),
                ObjData::ArgParser(state) => match &state.prog {
                    Some(prog) => format!("ArgumentParser(prog='{prog}')"),
                    None => "ArgumentParser(prog='program')".to_string(),
                },
                ObjData::Namespace(_) => repr_depth(v, 0),
                ObjData::DefaultDict(_) => repr_depth(v, 0),
                ObjData::Pattern(state) => format!("re.compile({})", quote_str(&state.source)),
                ObjData::Match(state) => {
                    format!("<re.Match object; pattern={}>", quote_str(&state.pattern))
                }
                ObjData::Socket(state) => state.render(),
                ObjData::Class(state) => format!("<class '{}'>", state.name),
                ObjData::Instance(state) => instance_default_repr(state),
                ObjData::Fiber(state) => fiber_repr(state),
                ObjData::ExtFunction(state) => {
                    format!("<built-in function {}>", state.name)
                }
                ObjData::Capsule { name, .. } => match name {
                    Some(name) => format!("<capsule object \"{name}\">"),
                    None => "<capsule object NULL>".to_string(),
                },
            };
        }
    }
    "None".to_string()
}

/// Python `repr()` of a value. Used for container elements and `KeyError`
/// messages; top-level strings are quoted here.
pub(crate) fn repr(v: i64) -> String {
    repr_depth(v, 0)
}

/// Maximum container nesting rendered before falling back to `...`. Guards
/// against self-referential lists/dicts (possible via item assignment)
/// overflowing the stack.
const MAX_REPR_DEPTH: usize = 64;

/// Python-style rendering of a `defaultdict` factory.
fn factory_repr(factory: i64) -> String {
    if factory == NONE {
        return "None".to_string();
    }
    if let Some(function) = as_function(factory) {
        return match &function.body {
            FunctionBody::Builtin(builtin) => format!("<class '{}'>", builtin.name()),
            _ => format!("<function {}>", function.name),
        };
    }
    repr_depth(factory, 0)
}

fn repr_depth(v: i64, depth: usize) -> String {
    if let Some(s) = as_str(v) {
        return quote_str(s);
    }
    if let Some(bytes) = as_bytes(v) {
        return quote_bytes(bytes);
    }
    if let Some((start, stop, step)) = as_range(v) {
        return range_repr(start, stop, step);
    }
    if let Some(state) = as_path(v) {
        // Like CPython: `repr(Path("a"))` is `PosixPath('a')` (the concrete
        // class name follows the platform).
        let class = if cfg!(windows) {
            "WindowsPath"
        } else {
            "PosixPath"
        };
        return format!("{class}({})", quote_str(state.path()));
    }
    if let Some(state) = as_class(v) {
        return format!("<class '{}'>", state.name);
    }
    if let Some(state) = as_instance(v) {
        return instance_default_repr(state);
    }
    if let Some(state) = as_fiber(v) {
        return fiber_repr(state);
    }
    if depth >= MAX_REPR_DEPTH {
        return "...".to_string();
    }
    if let Some(items) = as_list(v) {
        let mut out = String::from("[");
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&repr_depth(*item, depth + 1));
        }
        out.push(']');
        return out;
    }
    if let Some(items) = as_tuple(v) {
        let mut out = String::from("(");
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&repr_depth(*item, depth + 1));
        }
        if items.len() == 1 {
            out.push(',');
        }
        out.push(')');
        return out;
    }
    if let Some(items) = as_set(v) {
        if items.is_empty() {
            return "set()".to_string();
        }
        let mut out = String::from("{");
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&repr_depth(*item, depth + 1));
        }
        out.push('}');
        return out;
    }
    if let Some(pairs) = as_namespace(v) {
        let mut out = String::from("Namespace(");
        for (index, (name, item)) in pairs.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(name);
            out.push('=');
            out.push_str(&repr_depth(*item, depth + 1));
        }
        out.push(')');
        return out;
    }
    if let Some((pairs, factory)) = as_defaultdict(v) {
        let mut out = format!("defaultdict({}, {{", factory_repr(factory));
        for (index, (key, item)) in pairs.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&repr_depth(*key, depth + 1));
            out.push_str(": ");
            out.push_str(&repr_depth(*item, depth + 1));
        }
        out.push_str("})");
        return out;
    }
    if let Some(state) = as_pattern(v) {
        return format!("re.compile({})", quote_str(&state.source));
    }
    if let Some(state) = as_match(v) {
        let span = state.groups.first().copied().flatten();
        return match span {
            Some((start, end)) => format!(
                "<re.Match object; span=({start}, {end}), match={}>",
                quote_str(&state.text[start..end])
            ),
            None => "<re.Match object>".to_string(),
        };
    }
    if let Some(pairs) = as_dict(v) {
        let mut out = String::from("{");
        for (index, (key, value)) in pairs.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&repr_depth(*key, depth + 1));
            out.push_str(": ");
            out.push_str(&repr_depth(*value, depth + 1));
        }
        out.push('}');
        return out;
    }
    to_string(v)
}

/// Quotes a string like Python's `repr`: single quotes by default, double
/// quotes when the string contains a single quote but no double quote; `\\`,
/// the active quote, `\n`, `\r`, `\t` escaped and other control bytes rendered
/// as `\xNN`.
fn quote_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() && (c as u32) <= 0xff => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Quotes a bytes value like CPython's `repr`: `b'...'` by default, `b"..."`
/// when the payload contains a single quote but no double quote. Backslash,
/// the active quote, `\n`, `\r`, `\t` use short escapes; every byte outside
/// printable ASCII is rendered as `\xNN`.
fn quote_bytes(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = String::with_capacity(bytes.len() + 3);
    out.push('b');
    out.push(quote as char);
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c as char);
            }
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("\\x{:02x}", byte)),
        }
    }
    out.push(quote as char);
    out
}

/// Formats a float like Python's `repr`/`str`: shortest round-tripping decimal
/// digits, `.0` for integral values, scientific notation outside `[-4, 16)`.
pub(crate) fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        let text = if f.is_sign_negative() { "-inf" } else { "inf" };
        return text.to_string();
    }
    if f == 0.0 {
        let text = if f.is_sign_negative() { "-0.0" } else { "0.0" };
        return text.to_string();
    }

    let negative = f < 0.0;
    // Rust's `{:e}` emits the shortest representation that round-trips,
    // e.g. `1e20`, `5e-1`, `1.23456e2`.
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp_str) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp_str.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let digits = if digits.is_empty() {
        "0".to_string()
    } else {
        digits
    };

    let mut out = String::with_capacity(digits.len() + 8);
    if negative {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                for _ in digits.len()..int_len {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        if exp < 0 {
            out.push('-');
        } else {
            out.push('+');
        }
        let abs_exp = exp.unsigned_abs();
        if abs_exp < 10 {
            out.push('0');
        }
        out.push_str(&abs_exp.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagging_roundtrip() {
        for n in [0i64, 1, -1, 42, -42, MAX_INT, MIN_INT] {
            let v = encode_int(n).expect("in range");
            assert!(is_int(v));
            assert_eq!(v >> 3, n);
        }
        assert!(is_special(NONE));
        assert!(is_special(TRUE));
        assert!(is_special(FALSE));
        assert_eq!(type_name(TRUE), "bool");
        assert_eq!(type_name(NONE), "NoneType");
        assert_eq!(encode_int(MAX_INT + 1), None);
        assert_eq!(encode_int(MIN_INT - 1), None);
    }

    #[test]
    fn special_truthiness() {
        assert!(!truthy(NONE));
        assert!(!truthy(FALSE));
        assert!(truthy(TRUE));
        assert!(!truthy(encode_int(0).unwrap()));
        assert!(truthy(encode_int(-3).unwrap()));
    }

    #[test]
    fn strings_are_utf8_and_truthy() {
        let s = str_from_bytes("héllo".as_bytes());
        assert_eq!(as_str(s), Some("héllo"));
        assert!(truthy(s));
        assert!(!truthy(str_from_bytes(b"")));
        assert_eq!(type_name(s), "str");
    }

    #[test]
    fn float_formatting_matches_python() {
        assert_eq!(format_float(1.0), "1.0");
        assert_eq!(format_float(0.5), "0.5");
        assert_eq!(format_float(-0.5), "-0.5");
        assert_eq!(format_float(-0.0), "-0.0");
        assert_eq!(format_float(1e20), "1e+20");
        assert_eq!(format_float(1e15), "1000000000000000.0");
        assert_eq!(format_float(1e16), "1e+16");
        assert_eq!(format_float(0.0001), "0.0001");
        assert_eq!(format_float(0.00001), "1e-05");
        assert_eq!(format_float(123.456), "123.456");
        assert_eq!(format_float(f64::NAN), "nan");
        assert_eq!(format_float(f64::INFINITY), "inf");
        assert_eq!(format_float(f64::NEG_INFINITY), "-inf");
    }

    #[test]
    fn str_of_values() {
        assert_eq!(to_string(encode_int(-7).unwrap()), "-7");
        assert_eq!(to_string(TRUE), "True");
        assert_eq!(to_string(FALSE), "False");
        assert_eq!(to_string(NONE), "None");
        assert_eq!(to_string(float_new(2.0)), "2.0");
        assert_eq!(to_string(str_from_bytes(b"hi")), "hi");
    }

    #[test]
    fn container_repr_is_python_style() {
        let one = encode_int(1).unwrap();
        let a = str_from_bytes(b"a");
        let list = list_new(vec![one, a]);
        assert_eq!(to_string(list), "[1, 'a']");
        assert_eq!(repr(list), "[1, 'a']");

        let dict = dict_new(vec![(str_from_bytes(b"k"), one)]);
        assert_eq!(to_string(dict), "{'k': 1}");

        let nested = list_new(vec![list_new(vec![one, encode_int(2).unwrap()]), dict]);
        assert_eq!(to_string(nested), "[[1, 2], {'k': 1}]");
    }

    #[test]
    fn string_repr_escapes() {
        assert_eq!(repr(str_from_bytes(b"a'b")), "\"a'b\"");
        assert_eq!(repr(str_from_bytes(b"a\\b")), "'a\\\\b'");
        assert_eq!(repr(str_from_bytes(b"a\nb\r\t")), "'a\\nb\\r\\t'");
        assert_eq!(repr(str_from_bytes(&[0x01])), "'\\x01'");
    }

    #[test]
    fn tuple_repr_and_semantics() {
        let one = encode_int(1).unwrap();
        let a = str_from_bytes(b"a");
        assert_eq!(repr(tuple_new(Vec::new())), "()");
        assert_eq!(repr(tuple_new(vec![one])), "(1,)");
        assert_eq!(repr(tuple_new(vec![one, a])), "(1, 'a')");
        assert_eq!(
            repr(tuple_new(vec![tuple_new(vec![one]), tuple_new(Vec::new())])),
            "((1,), ())"
        );
        assert_eq!(to_string(tuple_new(vec![one, a])), "(1, 'a')");
        assert_eq!(type_name(tuple_new(Vec::new())), "tuple");
        assert!(!truthy(tuple_new(Vec::new())));
        assert!(truthy(tuple_new(vec![NONE])));
    }

    #[test]
    fn container_truthiness() {
        assert!(!truthy(list_new(Vec::new())));
        assert!(truthy(list_new(vec![NONE])));
        assert!(!truthy(dict_new(Vec::new())));
        assert!(truthy(dict_new(vec![(NONE, NONE)])));
        assert!(truthy(iter_new(NONE)));
        assert_eq!(type_name(list_new(Vec::new())), "list");
        assert_eq!(type_name(dict_new(Vec::new())), "dict");
        assert_eq!(type_name(iter_new(NONE)), "iterator");
    }

    #[test]
    fn capsule_semantics() {
        let named = capsule_new(0x1234, Some("pyx.demo".to_string()));
        assert_eq!(type_name(named), "PyCapsule");
        assert!(truthy(named));
        assert_eq!(to_string(named), "<capsule object \"pyx.demo\">");
        assert_eq!(repr(named), "<capsule object \"pyx.demo\">");
        assert_eq!(as_capsule(named), Some((0x1234, Some("pyx.demo"))));

        let unnamed = capsule_new(0x5678, None);
        assert_eq!(repr(unnamed), "<capsule object NULL>");
        assert_eq!(as_capsule(unnamed), Some((0x5678, None)));
        assert!(as_capsule(NONE).is_none());
        assert!(as_capsule(str_from_bytes(b"x")).is_none());
    }

    #[test]
    fn set_semantics() {
        let one = encode_int(1).unwrap();
        let two = encode_int(2).unwrap();
        let a = str_from_bytes(b"a");

        let empty = set_new(Vec::new());
        assert_eq!(type_name(empty), "set");
        assert!(!truthy(empty));
        assert_eq!(to_string(empty), "set()");
        assert_eq!(repr(empty), "set()");

        let s = set_new(vec![two, one, a]);
        assert!(truthy(s));
        assert_eq!(as_set(s), Some(&[two, one, a][..]));
        assert_eq!(to_string(s), "{2, 1, 'a'}");
        assert_eq!(repr(s), "{2, 1, 'a'}");
    }
}
