//! Tagged value representation and heap object layout (ARCHITECTURE §3.1).
//!
//! All runtime values are `i64`. The low three bits select the representation:
//!
//! | bits  | meaning                                                |
//! |-------|--------------------------------------------------------|
//! | `000` | immediate `int`, value is `v >> 3` (arithmetic shift)   |
//! | `001` | immediate special: `None = 1`, `False = 9`, `True = 17` |
//! | `010` | heap pointer: `ptr = v & !7`, points at [`HydroObj`]      |
//!
//! Heap objects carry a `kind: u32` header and live in a slab cell owned by
//! the collector ([`crate::heap::allocate`], ARCHITECTURE §7); a reference
//! stays valid while the object is reachable from runtime roots. Scalars and
//! tuples are immutable; lists and dicts are mutated in place by item
//! assignment and iterators advance in place.
//! After `_thread.start_new_thread` the heap is process-wide and values are
//! visible to every thread; there is no GIL, so concurrent mutation of the
//! same list/dict requires a user lock.

use std::alloc::Layout;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use num_bigint::{BigInt, Sign};
use num_traits::{FromPrimitive, ToPrimitive};

use hydro_ir::{MethodKind, ParamKind};

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
/// Python's `NotImplemented` singleton (reserved special payload 3). Codegen
/// materialises it for the `NotImplemented` constant; it is deliberately not a
/// distinct type (`type_name` reports `NotImplementedType`) and the equality
/// protocol does not treat it specially.
pub(crate) const NOT_IMPLEMENTED: i64 = 25;

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
/// Holds no Hydro values.
pub(crate) const KIND_LOCK: u32 = 10;
/// `threading.Thread` object. Traces its `target` and `args`.
pub(crate) const KIND_THREAD: u32 = 11;
/// `threading.Event` object. Holds no Hydro values.
pub(crate) const KIND_EVENT: u32 = 12;
/// `queue.Queue` object. Traces every queued item.
pub(crate) const KIND_QUEUE: u32 = 13;
/// `multiprocessing.Process` object. Traces its `target` and `args`.
pub(crate) const KIND_PROCESS: u32 = 14;
/// `multiprocessing.Queue` object. Items travel as pickled bytes, so the
/// native object holds no Hydro children.
pub(crate) const KIND_MPQUEUE: u32 = 15;
/// `multiprocessing.Pool` object. Traces its per-worker task and result
/// queues; the worker pids and flags are plain native state.
pub(crate) const KIND_POOL: u32 = 16;
/// Text file object returned by `io.open`. Holds no Hydro values; the native
/// file handle is closed when the object is dropped.
pub(crate) const KIND_FILE: u32 = 17;
/// Parsed-snapshot `csv.reader`/`csv.DictReader`. Holds Python strings, not
/// tagged values.
pub(crate) const KIND_CSVREADER: u32 = 18;
/// `csv.writer`/`csv.DictWriter`; holds the tagged target file as a GC edge.
pub(crate) const KIND_CSVWRITER: u32 = 19;
/// Immutable `bytes` object. Holds no Hydro values.
pub(crate) const KIND_BYTES: u32 = 20;
/// `random.Random` generator object. Holds no Hydro values; its PRNG state is
/// native and mutex-protected.
pub(crate) const KIND_RANDOM: u32 = 21;
/// `pathlib.Path` object. Holds a single native path string, no Hydro values.
pub(crate) const KIND_PATH: u32 = 22;
/// `argparse.ArgumentParser`. Traces its option conversion functions,
/// defaults and choices.
pub(crate) const KIND_ARG_PARSER: u32 = 23;
/// `argparse.Namespace`. Traces every attribute value.
pub(crate) const KIND_NAMESPACE: u32 = 24;
/// `collections.defaultdict`. Traces its factory and every pair.
pub(crate) const KIND_DEFAULTDICT: u32 = 25;
/// Compiled `re.Pattern`; holds native regex state, no Hydro values.
pub(crate) const KIND_PATTERN: u32 = 26;
/// `re.Match`; holds the searched text and capture positions, no Hydro values.
pub(crate) const KIND_MATCH: u32 = 27;
/// Blocking TCP `socket` object. Holds native state only, no Hydro values; the
/// descriptor is closed when the object is dropped.
pub(crate) const KIND_SOCKET: u32 = 28;
/// Class object created by `hydro_class_new`. Traces its base class, every
/// method value and every frozen class attribute. The class namespace is
/// immutable after creation.
pub(crate) const KIND_CLASS: u32 = 29;
/// Instance of a class. Traces its class and every instance attribute.
pub(crate) const KIND_INSTANCE: u32 = 30;
/// Stackful generator/coroutine fiber. Traces its arguments and result; the
/// suspended stack is registered as a separate GC root.
pub(crate) const KIND_FIBER: u32 = 31;
/// Function obtained from a CPython limited-API extension module. Stores the
/// shim's module handle and method id plus the Python attribute name; it has
/// no Hydro children.
pub(crate) const KIND_EXT_FUNCTION: u32 = 32;
/// Opaque pointer capsule obtained through the CPython C API. Holds a native
/// pointer and an optional name; it has no Hydro children, and its destructor
/// is never called in this slice.
pub(crate) const KIND_CAPSULE: u32 = 33;
/// Module value materialised by `hydro_module_value`; stores only its name and
/// has no Hydro children.
pub(crate) const KIND_MODULE: u32 = 34;
/// Native text stream over a process file descriptor (`sys.stdout`,
/// `sys.stderr`). Holds only the descriptor number, so it has no Hydro children.
pub(crate) const KIND_STD_STREAM: u32 = 35;
/// Arbitrary-precision Python `int` outside the tagged-immediate range. The
/// payload is immutable and holds no Hydro values.
pub(crate) const KIND_BIGINT: u32 = 36;
/// Mutable binding shared between a function and its nested closures
/// (`nonlocal`). Traces the single value it currently holds.
pub(crate) const KIND_CELL: u32 = 37;
/// Mutable string accumulator created by the frontend's string-accumulator
/// pass (`hydro_str_builder_new`). Internal to generated code: it never escapes
/// to user code, and `hydro_str_builder_finish` turns it into a normal `str`.
/// Holds native text only, plus the value of a defensive non-string
/// concatenation as a traced edge.
pub(crate) const KIND_STR_BUILDER: u32 = 38;

/// Function flag: created by `async def`, a call produces a coroutine.
pub(crate) const FUNCTION_FLAG_ASYNC: u32 = 1;
/// Function flag: contains `yield`, a call produces a generator.
pub(crate) const FUNCTION_FLAG_GENERATOR: u32 = 2;
/// Function flag: the wrapper has a dynamic signature (`*args`, `**kwargs` or
/// keyword-only parameters). Its argument buffer starts with the tagged total
/// count, followed by the captured values and the caller's positional
/// arguments; the wrapper binds defaults, the `*args` list and the `**kwargs`
/// dict before calling the concrete function.
pub(crate) const FUNCTION_FLAG_VARARGS: u32 = 4;

/// Largest integer representable in a tagged immediate (`2^60 - 1`).
pub(crate) const MAX_INT: i64 = i64::MAX >> 3;
/// Smallest integer representable in a tagged immediate (`-2^60`).
pub(crate) const MIN_INT: i64 = i64::MIN >> 3;

/// Fixed-size heap object header.
///
/// The collector tracks only this header: `kind` selects the payload type,
/// `marked` is the collector colour and `class` is the payload allocator's
/// size class (`0` means a large system allocation, `1..=N` index
/// [`crate::heap`]'s size-class table). The payload itself lives in a
/// separate block owned by the same pool and is reached through
/// [`HydroObj::data_ref`] / [`HydroObj::data_mut`], which dispatch on `kind`.
///
/// `#[repr(C)]` keeps `kind` at offset 0; the header is exactly 16 bytes and
/// the header slabs are 16-byte aligned, so tagged heap pointers are always
/// 8-byte aligned and the low three tag bits stay free. Payload alignment is
/// at least 8 and satisfies `align_of::<T>()` for the concrete payload type.
#[repr(C)]
pub(crate) struct HydroObj {
    pub(crate) kind: u32,
    /// Collector colour, owned by [`crate::heap`]: `WHITE` (0, not yet
    /// reached), `GREY` (1, queued for tracing) or `BLACK` (2, traced).
    pub(crate) marked: AtomicU8,
    /// Payload size class selected at allocation (`0` = large fallback).
    pub(crate) class: u8,
    /// Payload block; the concrete type is determined by `kind`.
    pub(crate) data: *mut u8,
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
    /// Native implementation from one of the `hydro_ir::stdlib` modules.
    Native {
        module: &'static str,
        name: &'static str,
    },
    /// A Python builtin referenced as a value (`f = int`).
    Builtin(hydro_ir::Builtin),
    /// `operator.attrgetter`: reads one attribute per stored name and returns
    /// the value, or a tuple when several names were given. Names are plain
    /// Rust strings, so the body owns no Hydro values.
    AttributeGetter { names: Vec<String> },
    /// `operator.itemgetter`: subscripts its argument with every stored key and
    /// returns the value, or a tuple when several keys were given. The keys
    /// are Hydro values and are traced as GC edges.
    ItemGetter { keys: Vec<i64> },
    /// `functools.partial`: calls `function` with `args` prepended to the
    /// arguments of each call and binds `keywords` by parameter name. The
    /// values of `args` and `keywords` are traced as GC edges.
    Partial {
        function: i64,
        args: Vec<i64>,
        keywords: Vec<(String, i64)>,
    },
    /// Native bound method (`obj.method` without a call). The receiver is a
    /// traced GC edge; keyword arguments are not supported by this value form.
    BoundMethod { receiver: i64, method: String },
    /// Identity callable used by `functools.cache`/`wraps` and by the
    /// decorator values returned from `lru_cache`/`wraps`: returns its single
    /// argument. Holds no Hydro values.
    Identity,
    /// Interpreter-owned function body. `token` identifies the IR function in
    /// the interpreter's program registry and `captures` are the closure
    /// values (traced as GC edges). The runtime calls back into the
    /// interpreter through the dispatcher installed by `hydro-interp`.
    Host { token: u64, captures: Vec<i64> },
}

/// A function value. For [`FunctionBody::Wrapper`] values `arity` is the exact
/// number of user-visible positional parameters (captured values are not
/// counted), or the minimum count when the wrapper has
/// [`FUNCTION_FLAG_VARARGS`] (the wrapper binds defaults, `*args` and
/// `**kwargs`). Native functions validate their arguments against the central
/// spec (`min_args..=max_args`) and store the upper bound here. Builtin values
/// store `usize::MAX` because they validate their own arity. Getters and
/// identity callables take exactly one argument; a partial stores the caller's
/// remaining count when its target is a generated wrapper. `name` is the
/// Python-visible name (`<lambda>` for anonymous functions). Wrapper functions
/// own their captured values, item getters own their keys and partials own the
/// target callable plus the bound arguments; the collector traces those edges
/// (see `gc::heap`).
pub(crate) struct ParamMeta {
    pub(crate) name: String,
    pub(crate) kind: ParamKind,
    /// Default value for a positional/keyword-only slot, when the source
    /// default was a literal (or materialisable tuple literal).
    pub(crate) default: Option<i64>,
}

pub(crate) struct FunctionObj {
    pub(crate) body: FunctionBody,
    pub(crate) arity: usize,
    pub(crate) name: String,
    /// Uniform `(argv) -> i64` trampoline that forwards every declared
    /// parameter slot, used by runtime keyword binding.
    pub(crate) full_entry: usize,
    /// Full parameter metadata (including `self`/`cls`); empty for native and
    /// derived callables.
    pub(crate) signature: Vec<ParamMeta>,
    /// [`FUNCTION_FLAG_ASYNC`] / [`FUNCTION_FLAG_GENERATOR`] /
    /// [`FUNCTION_FLAG_VARARGS`] bits for a generated wrapper. Native and
    /// builtin bodies keep `0`.
    pub(crate) flags: u32,
    /// User-visible positional parameter names of a generated wrapper.
    /// `functools.partial` uses them to bind keyword arguments by name; derived
    /// callables (native functions, getters, partials) have none.
    pub(crate) params: Vec<String>,
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
/// it on demand. A path holds no Hydro values.
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

impl Drop for ProcessState {
    /// Reaps a started process that became unreachable before `join`.
    ///
    /// Without this hook an unjoined `Process` leaves the child running (or as
    /// a zombie) for the lifetime of the parent. The cleanup is best effort and
    /// bounded, keeps `joined`/`exitcode` semantics, never waits for a child
    /// that was already reaped, and never parks for the collector: payload
    /// drops run inside the sweep, where parking is forbidden.
    fn drop(&mut self) {
        crate::process::reap_orphan_process(self);
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
    /// Set once [`Drop`] released the descriptors, so the endpoints are closed
    /// exactly once even if a teardown path runs twice.
    closed: AtomicBool,
}

impl MpQueueState {
    pub(crate) fn new(read_fd: i32, write_fd: i32, maxsize: usize) -> MpQueueState {
        MpQueueState {
            read_fd,
            write_fd,
            maxsize,
            read: Mutex::new(MpReadState { buffer: Vec::new() }),
            write: Mutex::new(()),
            closed: AtomicBool::new(false),
        }
    }
}

impl Drop for MpQueueState {
    /// Releases both queue endpoints when the last reference dies.
    ///
    /// A queue that is only reachable from Rust-side vectors (a pool that
    /// became unreachable) would otherwise leak its descriptors, keeping the
    /// socket pairs open for the rest of the process. Each descriptor is
    /// closed at most once per state; a descriptor number that is already
    /// closed (a decoded pickle wrapping released endpoints, or an inherited
    /// copy after `fork`) only yields `EBADF`, which is deliberately ignored.
    /// Closing the parent's copy never affects a forked child: the child owns
    /// an independent file-descriptor table.
    fn drop(&mut self) {
        if self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        // `read_fd == write_fd` cannot happen for a socketpair, but a state
        // that wrapped a single descriptor still closes it only once.
        if self.read_fd >= 0 {
            // SAFETY: the descriptor belongs to this queue state and this is
            // its single close.
            unsafe { libc::close(self.read_fd) };
        }
        if self.write_fd >= 0 && self.write_fd != self.read_fd {
            // SAFETY: as above.
            unsafe { libc::close(self.write_fd) };
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

impl Drop for PoolState {
    /// Terminates and reaps every worker of a pool that became unreachable
    /// without `close`/`join`/`terminate`.
    ///
    /// `joined` is claimed first, so a pool that was already torn down (or a
    /// re-entrant path) never signals or waits twice. The pid vector is taken
    /// through `get_mut` (no lock is acquired), so no graph, heap, queue or
    /// pid lock is held across a signal or a wait. The signal/reap loop is the
    /// same bounded best-effort cleanup used by [`ProcessState`]; a worker that
    /// is not a child of this process (a forked child's inherited copy) is
    /// skipped.
    fn drop(&mut self) {
        if self.joined.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.joined
            .store(true, std::sync::atomic::Ordering::Release);
        let pids = match self.pids.get_mut() {
            Ok(pids) => std::mem::take(pids),
            Err(poisoned) => std::mem::take(poisoned.into_inner()),
        };
        for pid in pids {
            crate::process::reap_orphan_child(i64::from(pid));
        }
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
/// binary files hold raw bytes and count in bytes. A file holds no Hydro values;
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
/// the optional header are native strings, so the object has no Hydro children;
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

    /// Copies the `(name, value)` pairs in insertion order.
    ///
    /// The snapshot takes the table lock only for the clone and owns its names,
    /// so the caller may allocate (which can collect) afterwards without
    /// holding the guard.
    pub(crate) fn snapshot(&self) -> Vec<(String, i64)> {
        self.lock().clone()
    }

    /// Drops every attribute (used when `instance.__dict__` is replaced).
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    /// Removes `name` and returns its value, or `None` when the attribute does
    /// not exist. The dropped edge is safe for an in-progress marking cycle:
    /// the value was already marked when the edge was published.
    pub(crate) fn remove(&self, name: &str) -> Option<i64> {
        let mut entries = self.lock();
        let position = entries
            .iter()
            .position(|(existing, _)| existing.as_str() == name)?;
        Some(entries.remove(position).1)
    }
}

/// State of a class object: its Python-visible name, its base class (or
/// `NONE`), the methods collected from `class Name(Base):` and the class
/// attributes frozen when the class statement ran.
///
/// A method is an ordinary function value plus its binding kind. The namespace
/// is immutable after construction, so lookups need no lock and the collector
/// can read it directly.
pub(crate) struct ClassState {
    pub(crate) name: String,
    pub(crate) base: i64,
    pub(crate) methods: Vec<(String, i64, MethodKind)>,
    pub(crate) attrs: Vec<(String, i64)>,
}

impl ClassState {
    fn method(&self, name: &str) -> Option<(MethodKind, i64)> {
        self.methods
            .iter()
            .find(|(existing, _, _)| existing.as_str() == name)
            .map(|(_, function, kind)| (*kind, *function))
    }

    fn attr(&self, name: &str) -> Option<i64> {
        self.attrs
            .iter()
            .find(|(existing, _)| existing.as_str() == name)
            .map(|(_, value)| *value)
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

/// State of a module value: its import name. The value holds no Hydro
/// children; attribute access goes through `crate::modules`.
pub(crate) struct ModuleState {
    pub(crate) name: String,
}

impl ModuleState {
    /// The module's import name, e.g. `"errno"`.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

/// Native text stream behind `sys.stdin` / `sys.stdout` / `sys.stderr`.
///
/// The stream stores only its standard descriptor (0, 1 or 2); reading goes
/// through `std::io::stdin` and writing through `std::io::stdout`/`stderr`,
/// one unbuffered-and-flushed call per `write`. It holds no Hydro values, so the
/// collector never traces children.
pub(crate) struct StdStreamState {
    fd: i32,
}

impl StdStreamState {
    /// The descriptor (`0` stdin, `1` stdout, `2` stderr).
    pub(crate) fn fd(&self) -> i32 {
        self.fd
    }

    /// Python-visible name used in error messages.
    pub(crate) fn name(&self) -> &'static str {
        match self.fd {
            0 => "stdin",
            2 => "stderr",
            _ => "stdout",
        }
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

/// Payload of a string accumulator ([`KIND_STR_BUILDER`]).
pub(crate) enum StrBuilderState {
    /// Text accumulated so far; `append` extends it in place, so the loop
    /// stays linear instead of copying the whole prefix per iteration.
    Building(String),
    /// A non-string right operand forced the generic `+` path. The result of
    /// that concatenation is stored here (and traced) until the next append
    /// or [`str_builder_finish`]; a later string append still works, because
    /// the generic path materialises the accumulated value first.
    Materialized(i64),
}

/// Constructor descriptor for a heap payload.
///
/// The runtime passes one of these to [`crate::heap::allocate`], which writes
/// only the variant's inner value into a separately allocated, size-classed
/// payload block. The enum itself is never stored on the heap; readers use
/// [`ObjDataRef`] and writers [`ObjDataMut`].
#[repr(C)]
#[allow(clippy::large_enum_variant)] // Construction descriptor only; the stored payload is split by size class.
pub(crate) enum ObjData {
    Str(String),
    Float(f64),
    Exception(ExcData),
    /// Text accumulator for a proved `s += ...` loop ([`KIND_STR_BUILDER`]).
    StrBuilder(StrBuilderState),
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
    /// Lazy integer range; holds no Hydro values.
    Range {
        start: i64,
        stop: i64,
        step: i64,
    },
    /// Callable function value; wrapper closures trace their captured values.
    Function(FunctionObj),
    /// Binary/reentrant thread lock; holds no Hydro values.
    Lock(LockState),
    /// `threading.Thread`; traces target and arguments.
    Thread(ThreadState),
    /// `threading.Event`; holds no Hydro values.
    Event(EventState),
    /// `queue.Queue`; traces every queued item.
    Queue(QueueState),
    /// `multiprocessing.Process`; traces target and arguments.
    Process(ProcessState),
    /// `multiprocessing.Queue`; items are pickled bytes, no Hydro children.
    MpQueue(MpQueueState),
    /// `multiprocessing.Pool`; traces the per-worker queues.
    Pool(PoolState),
    /// File from `io.open` (text or binary); holds no Hydro values.
    File(FileState),
    /// Parsed `csv.reader`/`csv.DictReader`; holds no Hydro values.
    CsvReader(CsvReaderState),
    /// `csv.writer`/`csv.DictWriter`; traces its target file.
    CsvWriter(CsvWriterState),
    /// Immutable byte string; holds no Hydro values.
    Bytes(Vec<u8>),
    /// `random.Random` generator; holds no Hydro values.
    Random(RandomState),
    /// `pathlib.Path`; a plain path string, so no Hydro children.
    Path(PathState),
    /// `argparse.ArgumentParser`; traces its option definitions.
    ArgParser(crate::modules::argparse::ArgParserState),
    /// `argparse.Namespace`; traces every attribute value.
    Namespace(NamespaceState),
    /// `collections.defaultdict`; traces the factory and every pair.
    DefaultDict(crate::modules::collections::DefaultDictState),
    /// Compiled `re.Pattern`; no Hydro children.
    Pattern(crate::modules::re::PatternState),
    /// `re.Match`; no Hydro children.
    Match(crate::modules::re::MatchState),
    /// Blocking TCP socket; no Hydro children. Dropping it closes the socket.
    Socket(crate::modules::socket::SocketState),
    /// Class object; traces its base class and every method value.
    Class(ClassState),
    /// Class instance; traces its class and every attribute value.
    Instance(InstanceState),
    /// Stackful generator/coroutine. Traces its arguments and result; the
    /// suspended stack is registered as a separate conservative root.
    Fiber(Box<crate::fiber::FiberState>),
    /// Callable member of a loaded extension module; holds no Hydro values.
    ExtFunction(ExtFunctionState),
    /// Opaque pointer capsule from an extension module. The pointer belongs
    /// to the extension; Hydro never dereferences or frees it, and the
    /// destructor is not stored here, matching the documented slice
    /// limitation (see [`crate::capi`]).
    Capsule {
        ptr: usize,
        name: Option<String>,
    },
    /// Module value (`<module 'name'>`); holds no Hydro values.
    Module(ModuleState),
    /// Native text stream over fd 1/2; holds no Hydro values.
    StdStream(StdStreamState),
    /// Arbitrary-precision Python `int` (immutable, no Hydro children).
    BigInt(BigInt),
    /// Mutable closure cell. Internal to generated code; holds one traced
    /// value that reads and writes go through.
    Cell(i64),
}

/// Payload of a `range` object ([`KIND_RANGE`]).
///
/// The constructor descriptor keeps the fields inline ([`ObjData::Range`]);
/// this is the concrete type written into the sized payload block.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RangeData {
    pub(crate) start: i64,
    pub(crate) stop: i64,
    pub(crate) step: i64,
}

/// Payload of a capsule object ([`KIND_CAPSULE`]).
pub(crate) struct CapsuleData {
    pub(crate) ptr: usize,
    pub(crate) name: Option<String>,
}

/// Borrowed payload of a heap object, selected by [`HydroObj::kind`].
///
/// The variant shapes mirror [`ObjData`] so payload readers can pattern-match
/// with the same arm names they used when the payload was stored inline. The
/// references are valid for the lifetime of the borrow of the header; the
/// collector never frees a reachable object.
#[allow(clippy::borrowed_box)] // View mirrors the concrete boxed payload allocated by kind.
pub(crate) enum ObjDataRef<'a> {
    Str(&'a String),
    Float(f64),
    Exception(&'a ExcData),
    StrBuilder(&'a StrBuilderState),
    List(&'a Vec<i64>),
    Dict(&'a DictData),
    Iter(&'a Box<IterState>),
    Tuple(&'a Vec<i64>),
    Set(&'a Vec<i64>),
    Range {
        start: i64,
        stop: i64,
        step: i64,
    },
    Function(&'a FunctionObj),
    Lock(&'a LockState),
    Thread(&'a ThreadState),
    Event(&'a EventState),
    Queue(&'a QueueState),
    Process(&'a ProcessState),
    MpQueue(&'a MpQueueState),
    Pool(&'a PoolState),
    File(&'a FileState),
    CsvReader(&'a CsvReaderState),
    CsvWriter(&'a CsvWriterState),
    Bytes(&'a Vec<u8>),
    Random(&'a RandomState),
    Path(&'a PathState),
    ArgParser(&'a crate::modules::argparse::ArgParserState),
    Namespace(&'a NamespaceState),
    DefaultDict(&'a crate::modules::collections::DefaultDictState),
    Pattern(&'a crate::modules::re::PatternState),
    Match(&'a crate::modules::re::MatchState),
    Socket(&'a crate::modules::socket::SocketState),
    Class(&'a ClassState),
    Instance(&'a InstanceState),
    Fiber(&'a Box<crate::fiber::FiberState>),
    ExtFunction(&'a ExtFunctionState),
    Capsule {
        ptr: usize,
        name: Option<&'a String>,
    },
    Module(&'a ModuleState),
    StdStream(&'a StdStreamState),
    BigInt(&'a BigInt),
    Cell(i64),
}

/// Exclusive payload view of a heap object, selected by [`HydroObj::kind`].
///
/// Every variant is an exclusive reference to the concrete payload type, so a
/// mutation site cannot accidentally write a differently sized value.
#[allow(dead_code)] // Complete typed view; most variants are used by mutation sites.
pub(crate) enum ObjDataMut<'a> {
    Str(&'a mut String),
    Float(&'a mut f64),
    Exception(&'a mut ExcData),
    StrBuilder(&'a mut StrBuilderState),
    List(&'a mut Vec<i64>),
    Dict(&'a mut DictData),
    Iter(&'a mut Box<IterState>),
    Tuple(&'a mut Vec<i64>),
    Set(&'a mut Vec<i64>),
    Range(&'a mut RangeData),
    Function(&'a mut FunctionObj),
    Lock(&'a mut LockState),
    Thread(&'a mut ThreadState),
    Event(&'a mut EventState),
    Queue(&'a mut QueueState),
    Process(&'a mut ProcessState),
    MpQueue(&'a mut MpQueueState),
    Pool(&'a mut PoolState),
    File(&'a mut FileState),
    CsvReader(&'a mut CsvReaderState),
    CsvWriter(&'a mut CsvWriterState),
    Bytes(&'a mut Vec<u8>),
    Random(&'a mut RandomState),
    Path(&'a mut PathState),
    ArgParser(&'a mut crate::modules::argparse::ArgParserState),
    Namespace(&'a mut NamespaceState),
    DefaultDict(&'a mut crate::modules::collections::DefaultDictState),
    Pattern(&'a mut crate::modules::re::PatternState),
    Match(&'a mut crate::modules::re::MatchState),
    Socket(&'a mut crate::modules::socket::SocketState),
    Class(&'a mut ClassState),
    Instance(&'a mut InstanceState),
    Fiber(&'a mut Box<crate::fiber::FiberState>),
    ExtFunction(&'a mut ExtFunctionState),
    Capsule(&'a mut CapsuleData),
    Module(&'a mut ModuleState),
    StdStream(&'a mut StdStreamState),
    BigInt(&'a mut BigInt),
    Cell(&'a mut i64),
}

impl HydroObj {
    /// Borrows the payload as its concrete type.
    ///
    /// # Panics
    ///
    /// Panics on an unknown `kind`; every value produced by the runtime
    /// allocator has a kind covered by the payload table.
    pub(crate) fn data_ref(&self) -> ObjDataRef<'_> {
        debug_assert!(!self.data.is_null(), "heap object without a payload");
        // SAFETY: the allocator writes exactly the payload type selected by
        // `kind` into `data`; the borrow is tied to the header, which the
        // collector keeps alive while the object is reachable.
        unsafe {
            let payload = self.data;
            match self.kind {
                KIND_STR => ObjDataRef::Str(&*(payload as *const String)),
                KIND_FLOAT => ObjDataRef::Float(*(payload as *const f64)),
                KIND_EXCEPTION => ObjDataRef::Exception(&*(payload as *const ExcData)),
                KIND_STR_BUILDER => ObjDataRef::StrBuilder(&*(payload as *const StrBuilderState)),
                KIND_LIST => ObjDataRef::List(&*(payload as *const Vec<i64>)),
                KIND_DICT => ObjDataRef::Dict(&*(payload as *const DictData)),
                KIND_ITER => ObjDataRef::Iter(&*(payload as *const Box<IterState>)),
                KIND_TUPLE => ObjDataRef::Tuple(&*(payload as *const Vec<i64>)),
                KIND_SET => ObjDataRef::Set(&*(payload as *const Vec<i64>)),
                KIND_RANGE => {
                    let range = &*(payload as *const RangeData);
                    ObjDataRef::Range {
                        start: range.start,
                        stop: range.stop,
                        step: range.step,
                    }
                }
                KIND_FUNCTION => ObjDataRef::Function(&*(payload as *const FunctionObj)),
                KIND_LOCK => ObjDataRef::Lock(&*(payload as *const LockState)),
                KIND_THREAD => ObjDataRef::Thread(&*(payload as *const ThreadState)),
                KIND_EVENT => ObjDataRef::Event(&*(payload as *const EventState)),
                KIND_QUEUE => ObjDataRef::Queue(&*(payload as *const QueueState)),
                KIND_PROCESS => ObjDataRef::Process(&*(payload as *const ProcessState)),
                KIND_MPQUEUE => ObjDataRef::MpQueue(&*(payload as *const MpQueueState)),
                KIND_POOL => ObjDataRef::Pool(&*(payload as *const PoolState)),
                KIND_FILE => ObjDataRef::File(&*(payload as *const FileState)),
                KIND_CSVREADER => ObjDataRef::CsvReader(&*(payload as *const CsvReaderState)),
                KIND_CSVWRITER => ObjDataRef::CsvWriter(&*(payload as *const CsvWriterState)),
                KIND_BYTES => ObjDataRef::Bytes(&*(payload as *const Vec<u8>)),
                KIND_RANDOM => ObjDataRef::Random(&*(payload as *const RandomState)),
                KIND_PATH => ObjDataRef::Path(&*(payload as *const PathState)),
                KIND_ARG_PARSER => ObjDataRef::ArgParser(
                    &*(payload as *const crate::modules::argparse::ArgParserState),
                ),
                KIND_NAMESPACE => ObjDataRef::Namespace(&*(payload as *const NamespaceState)),
                KIND_DEFAULTDICT => ObjDataRef::DefaultDict(
                    &*(payload as *const crate::modules::collections::DefaultDictState),
                ),
                KIND_PATTERN => {
                    ObjDataRef::Pattern(&*(payload as *const crate::modules::re::PatternState))
                }
                KIND_MATCH => {
                    ObjDataRef::Match(&*(payload as *const crate::modules::re::MatchState))
                }
                KIND_SOCKET => {
                    ObjDataRef::Socket(&*(payload as *const crate::modules::socket::SocketState))
                }
                KIND_CLASS => ObjDataRef::Class(&*(payload as *const ClassState)),
                KIND_INSTANCE => ObjDataRef::Instance(&*(payload as *const InstanceState)),
                KIND_FIBER => {
                    ObjDataRef::Fiber(&*(payload as *const Box<crate::fiber::FiberState>))
                }
                KIND_EXT_FUNCTION => {
                    ObjDataRef::ExtFunction(&*(payload as *const ExtFunctionState))
                }
                KIND_CAPSULE => {
                    let capsule = &*(payload as *const CapsuleData);
                    ObjDataRef::Capsule {
                        ptr: capsule.ptr,
                        name: capsule.name.as_ref(),
                    }
                }
                KIND_MODULE => ObjDataRef::Module(&*(payload as *const ModuleState)),
                KIND_STD_STREAM => ObjDataRef::StdStream(&*(payload as *const StdStreamState)),
                KIND_BIGINT => ObjDataRef::BigInt(&*(payload as *const BigInt)),
                KIND_CELL => ObjDataRef::Cell(*(payload as *const i64)),
                other => panic!("unknown object kind {other}"),
            }
        }
    }

    /// Exclusively borrows the payload as its concrete type.
    ///
    /// # Panics
    ///
    /// Panics on an unknown `kind`, like [`HydroObj::data_ref`].
    pub(crate) fn data_mut(&mut self) -> ObjDataMut<'_> {
        debug_assert!(!self.data.is_null(), "heap object without a payload");
        // SAFETY: as `data_ref`; callers only mutate reachable objects from
        // the runtime thread (containers enter through a graph lock).
        unsafe {
            let payload = self.data;
            match self.kind {
                KIND_STR => ObjDataMut::Str(&mut *(payload as *mut String)),
                KIND_FLOAT => ObjDataMut::Float(&mut *(payload as *mut f64)),
                KIND_EXCEPTION => ObjDataMut::Exception(&mut *(payload as *mut ExcData)),
                KIND_STR_BUILDER => ObjDataMut::StrBuilder(&mut *(payload as *mut StrBuilderState)),
                KIND_LIST => ObjDataMut::List(&mut *(payload as *mut Vec<i64>)),
                KIND_DICT => ObjDataMut::Dict(&mut *(payload as *mut DictData)),
                KIND_ITER => ObjDataMut::Iter(&mut *(payload as *mut Box<IterState>)),
                KIND_TUPLE => ObjDataMut::Tuple(&mut *(payload as *mut Vec<i64>)),
                KIND_SET => ObjDataMut::Set(&mut *(payload as *mut Vec<i64>)),
                KIND_RANGE => ObjDataMut::Range(&mut *(payload as *mut RangeData)),
                KIND_FUNCTION => ObjDataMut::Function(&mut *(payload as *mut FunctionObj)),
                KIND_LOCK => ObjDataMut::Lock(&mut *(payload as *mut LockState)),
                KIND_THREAD => ObjDataMut::Thread(&mut *(payload as *mut ThreadState)),
                KIND_EVENT => ObjDataMut::Event(&mut *(payload as *mut EventState)),
                KIND_QUEUE => ObjDataMut::Queue(&mut *(payload as *mut QueueState)),
                KIND_PROCESS => ObjDataMut::Process(&mut *(payload as *mut ProcessState)),
                KIND_MPQUEUE => ObjDataMut::MpQueue(&mut *(payload as *mut MpQueueState)),
                KIND_POOL => ObjDataMut::Pool(&mut *(payload as *mut PoolState)),
                KIND_FILE => ObjDataMut::File(&mut *(payload as *mut FileState)),
                KIND_CSVREADER => ObjDataMut::CsvReader(&mut *(payload as *mut CsvReaderState)),
                KIND_CSVWRITER => ObjDataMut::CsvWriter(&mut *(payload as *mut CsvWriterState)),
                KIND_BYTES => ObjDataMut::Bytes(&mut *(payload as *mut Vec<u8>)),
                KIND_RANDOM => ObjDataMut::Random(&mut *(payload as *mut RandomState)),
                KIND_PATH => ObjDataMut::Path(&mut *(payload as *mut PathState)),
                KIND_ARG_PARSER => ObjDataMut::ArgParser(
                    &mut *(payload as *mut crate::modules::argparse::ArgParserState),
                ),
                KIND_NAMESPACE => ObjDataMut::Namespace(&mut *(payload as *mut NamespaceState)),
                KIND_DEFAULTDICT => ObjDataMut::DefaultDict(
                    &mut *(payload as *mut crate::modules::collections::DefaultDictState),
                ),
                KIND_PATTERN => {
                    ObjDataMut::Pattern(&mut *(payload as *mut crate::modules::re::PatternState))
                }
                KIND_MATCH => {
                    ObjDataMut::Match(&mut *(payload as *mut crate::modules::re::MatchState))
                }
                KIND_SOCKET => {
                    ObjDataMut::Socket(&mut *(payload as *mut crate::modules::socket::SocketState))
                }
                KIND_CLASS => ObjDataMut::Class(&mut *(payload as *mut ClassState)),
                KIND_INSTANCE => ObjDataMut::Instance(&mut *(payload as *mut InstanceState)),
                KIND_FIBER => {
                    ObjDataMut::Fiber(&mut *(payload as *mut Box<crate::fiber::FiberState>))
                }
                KIND_EXT_FUNCTION => {
                    ObjDataMut::ExtFunction(&mut *(payload as *mut ExtFunctionState))
                }
                KIND_CAPSULE => ObjDataMut::Capsule(&mut *(payload as *mut CapsuleData)),
                KIND_MODULE => ObjDataMut::Module(&mut *(payload as *mut ModuleState)),
                KIND_STD_STREAM => ObjDataMut::StdStream(&mut *(payload as *mut StdStreamState)),
                KIND_BIGINT => ObjDataMut::BigInt(&mut *(payload as *mut BigInt)),
                KIND_CELL => ObjDataMut::Cell(&mut *(payload as *mut i64)),
                other => panic!("unknown object kind {other}"),
            }
        }
    }
}

/// Expands `$m!(KIND_*, VariantName, PayloadType)` for every object kind.
///
/// The table is the single source of truth shared by the payload layout, the
/// dropper and the allocator: every [`ObjData`] variant maps to exactly one
/// payload type and exactly one kind.
macro_rules! for_each_payload_type {
    ($m:ident) => {
        $m!(KIND_STR, Str, String);
        $m!(KIND_FLOAT, Float, f64);
        $m!(KIND_EXCEPTION, Exception, ExcData);
        $m!(KIND_STR_BUILDER, StrBuilder, StrBuilderState);
        $m!(KIND_LIST, List, Vec<i64>);
        $m!(KIND_DICT, Dict, DictData);
        $m!(KIND_ITER, Iter, Box<IterState>);
        $m!(KIND_TUPLE, Tuple, Vec<i64>);
        $m!(KIND_SET, Set, Vec<i64>);
        $m!(KIND_RANGE, Range, RangeData);
        $m!(KIND_FUNCTION, Function, FunctionObj);
        $m!(KIND_LOCK, Lock, LockState);
        $m!(KIND_THREAD, Thread, ThreadState);
        $m!(KIND_EVENT, Event, EventState);
        $m!(KIND_QUEUE, Queue, QueueState);
        $m!(KIND_PROCESS, Process, ProcessState);
        $m!(KIND_MPQUEUE, MpQueue, MpQueueState);
        $m!(KIND_POOL, Pool, PoolState);
        $m!(KIND_FILE, File, FileState);
        $m!(KIND_CSVREADER, CsvReader, CsvReaderState);
        $m!(KIND_CSVWRITER, CsvWriter, CsvWriterState);
        $m!(KIND_BYTES, Bytes, Vec<u8>);
        $m!(KIND_RANDOM, Random, RandomState);
        $m!(KIND_PATH, Path, PathState);
        $m!(
            KIND_ARG_PARSER,
            ArgParser,
            crate::modules::argparse::ArgParserState
        );
        $m!(KIND_NAMESPACE, Namespace, NamespaceState);
        $m!(
            KIND_DEFAULTDICT,
            DefaultDict,
            crate::modules::collections::DefaultDictState
        );
        $m!(KIND_PATTERN, Pattern, crate::modules::re::PatternState);
        $m!(KIND_MATCH, Match, crate::modules::re::MatchState);
        $m!(KIND_SOCKET, Socket, crate::modules::socket::SocketState);
        $m!(KIND_CLASS, Class, ClassState);
        $m!(KIND_INSTANCE, Instance, InstanceState);
        $m!(KIND_FIBER, Fiber, Box<crate::fiber::FiberState>);
        $m!(KIND_EXT_FUNCTION, ExtFunction, ExtFunctionState);
        $m!(KIND_CAPSULE, Capsule, CapsuleData);
        $m!(KIND_MODULE, Module, ModuleState);
        $m!(KIND_STD_STREAM, StdStream, StdStreamState);
        $m!(KIND_BIGINT, BigInt, BigInt);
        $m!(KIND_CELL, Cell, i64);
    };
}

/// Layout required by the payload type of `kind`.
///
/// The allocator rounds this up to a size class; the alignment is always at
/// least 8.
pub(crate) fn payload_layout(kind: u32) -> Layout {
    macro_rules! arm {
        ($kind:expr, $variant:ident, $ty:ty) => {
            if kind == $kind {
                return Layout::new::<$ty>();
            }
        };
    }
    for_each_payload_type!(arm);
    panic!("unknown object kind {kind}")
}

/// Writes `data` into the payload block `dst`.
///
/// # Safety
///
/// `dst` must point to at least [`payload_layout`] of the type selected by
/// `kind`, and uninitialised storage for that type.
pub(crate) unsafe fn write_payload(kind: u32, data: ObjData, dst: *mut u8) {
    // The macro must expand to the whole `match`: a macro invoked in match-arm
    // position may only expand to a pattern, never to `pattern => body`.
    macro_rules! payload_match {
        ($data:expr, $dst:expr; $( $kind_const:expr, $variant:ident, $ty:ty ),* $(,)?) => {
            match $data {
                $(
                    ObjData::$variant(value) => {
                        assert_eq!(
                            kind, $kind_const,
                            "object kind {kind} does not match its payload variant"
                        );
                        unsafe { ptr::write($dst as *mut $ty, value) };
                    }
                )*
                ObjData::Range { start, stop, step } => {
                    assert_eq!(kind, KIND_RANGE, "object kind {kind} is not a range");
                    unsafe { ptr::write($dst as *mut RangeData, RangeData { start, stop, step }) };
                }
                ObjData::Capsule { ptr: capsule_ptr, name } => {
                    assert_eq!(kind, KIND_CAPSULE, "object kind {kind} is not a capsule");
                    unsafe {
                        ptr::write($dst as *mut CapsuleData, CapsuleData { ptr: capsule_ptr, name })
                    };
                }
            }
        };
    }

    payload_match! { data, dst;
        KIND_STR, Str, String,
        KIND_FLOAT, Float, f64,
        KIND_EXCEPTION, Exception, ExcData,
        KIND_STR_BUILDER, StrBuilder, StrBuilderState,
        KIND_LIST, List, Vec<i64>,
        KIND_DICT, Dict, DictData,
        KIND_ITER, Iter, Box<IterState>,
        KIND_TUPLE, Tuple, Vec<i64>,
        KIND_SET, Set, Vec<i64>,
        KIND_FUNCTION, Function, FunctionObj,
        KIND_LOCK, Lock, LockState,
        KIND_THREAD, Thread, ThreadState,
        KIND_EVENT, Event, EventState,
        KIND_QUEUE, Queue, QueueState,
        KIND_PROCESS, Process, ProcessState,
        KIND_MPQUEUE, MpQueue, MpQueueState,
        KIND_POOL, Pool, PoolState,
        KIND_FILE, File, FileState,
        KIND_CSVREADER, CsvReader, CsvReaderState,
        KIND_CSVWRITER, CsvWriter, CsvWriterState,
        KIND_BYTES, Bytes, Vec<u8>,
        KIND_RANDOM, Random, RandomState,
        KIND_PATH, Path, PathState,
        KIND_ARG_PARSER, ArgParser, crate::modules::argparse::ArgParserState,
        KIND_NAMESPACE, Namespace, NamespaceState,
        KIND_DEFAULTDICT, DefaultDict, crate::modules::collections::DefaultDictState,
        KIND_PATTERN, Pattern, crate::modules::re::PatternState,
        KIND_MATCH, Match, crate::modules::re::MatchState,
        KIND_SOCKET, Socket, crate::modules::socket::SocketState,
        KIND_CLASS, Class, ClassState,
        KIND_INSTANCE, Instance, InstanceState,
        KIND_FIBER, Fiber, Box<crate::fiber::FiberState>,
        KIND_EXT_FUNCTION, ExtFunction, ExtFunctionState,
        KIND_MODULE, Module, ModuleState,
        KIND_STD_STREAM, StdStream, StdStreamState,
        KIND_BIGINT, BigInt, BigInt,
        KIND_CELL, Cell, i64,
    }
}

/// Drops the payload selected by `kind` in place.
///
/// # Safety
///
/// `dst` must point to an initialised payload of the type selected by `kind`;
/// the storage is logically uninitialised afterwards.
pub(crate) unsafe fn drop_payload(kind: u32, dst: *mut u8) {
    macro_rules! arm {
        ($kind:expr, $variant:ident, $ty:ty) => {
            if kind == $kind {
                unsafe { ptr::drop_in_place(dst as *mut $ty) };
                return;
            }
        };
    }
    for_each_payload_type!(arm);
    panic!("unknown object kind {kind}")
}

// Tag helpers

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
/// (Python's `bool` is numerically an `int`). A heap `BigInt` payload that
/// fits `i64` is decoded as well; larger BigInts yield `None` (use
/// [`as_bigint`] for the full value). `None` and other heap values yield
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
    as_bigint(v).and_then(|n| i64::try_from(n).ok())
}

/// True when `v` is a heap [`ObjData::BigInt`] payload.
pub(crate) fn is_bigint(v: i64) -> bool {
    if !is_heap(v) {
        return false;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    matches!(unsafe { as_obj(v) }, Some(obj) if obj.kind == KIND_BIGINT)
}

/// Borrows the payload of a heap `BigInt`. Tagged ints are *not* converted;
/// use [`to_bigint`] for a numeric operand that may be tagged.
pub(crate) fn as_bigint(v: i64) -> Option<&'static BigInt> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_BIGINT {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::BigInt(n) => Some(n),
        _ => None,
    }
}

/// Exact integer operand as a `BigInt`: tagged ints, booleans and heap
/// BigInts. Every other value (including floats) yields `None`.
pub(crate) fn to_bigint(v: i64) -> Option<BigInt> {
    if let Some(n) = as_integer(v) {
        return Some(BigInt::from(n));
    }
    as_bigint(v).cloned()
}

/// Exact three-way comparison of an integer with a float. `None` means
/// unordered (NaN). The float's integral part is converted exactly, so the
/// comparison never loses precision, no matter how large the integer is.
pub(crate) fn bigint_cmp_f64(n: &BigInt, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f == f64::INFINITY {
        return Some(Ordering::Less);
    }
    if f == f64::NEG_INFINITY {
        return Some(Ordering::Greater);
    }
    let truncated = BigInt::from_f64(f.trunc())?;
    match n.cmp(&truncated) {
        Ordering::Equal => {
            if f.fract() > 0.0 {
                Some(Ordering::Less)
            } else if f.fract() < 0.0 {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Equal)
            }
        }
        other => Some(other),
    }
}

/// `n` as an `f64`; `None` when its magnitude exceeds the `f64` range (the
/// caller decides between `f64::INFINITY` and an `OverflowError`).
pub(crate) fn bigint_to_f64(n: &BigInt) -> Option<f64> {
    match n.to_f64() {
        Some(f) if f.is_finite() => Some(f),
        _ => None,
    }
}

/// Truncates a finite float toward zero into an exact integer.
pub(crate) fn bigint_from_f64_trunc(f: f64) -> Option<BigInt> {
    BigInt::from_f64(f.trunc())
}

/// Parses a base-10 integer literal (`int(text)` grammar): an optional
/// leading sign followed by decimal digits.
pub(crate) fn parse_decimal_int(text: &str) -> Option<BigInt> {
    text.parse::<BigInt>().ok()
}

/// Parses `digits` (already validated against `radix`) with an optional sign.
pub(crate) fn bigint_from_radix(digits: &str, radix: u32, negative: bool) -> Option<BigInt> {
    let magnitude = BigInt::parse_bytes(digits.as_bytes(), radix)?;
    Some(if negative { -magnitude } else { magnitude })
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

/// `hydro_int` helper: identity for already-tagged values, otherwise encodes a
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

// Heap access

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
pub(crate) unsafe fn as_obj(v: i64) -> Option<&'static HydroObj> {
    if v & TAG_MASK != TAG_HEAP {
        return None;
    }
    let addr = (v & !TAG_MASK) as usize;
    if addr == 0 {
        // Malformed value: tag without a pointer.
        return None;
    }
    Some(&*(addr as *const HydroObj))
}

pub(crate) fn as_float(v: i64) -> Option<f64> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_FLOAT {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Float(f) => Some(f),
        _ => None,
    }
}

pub(crate) fn as_str(v: i64) -> Option<&'static str> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_STR {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Borrows the contents of a `bytes` object. `None` for any other value.
pub(crate) fn as_bytes(v: i64) -> Option<&'static [u8]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_BYTES {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Bytes(bytes) => Some(bytes.as_slice()),
        _ => None,
    }
}

/// Borrows the PRNG state of a `random.Random` object.
pub(crate) fn as_random(v: i64) -> Option<&'static RandomState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_RANDOM {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Random(state) => Some(state),
        _ => None,
    }
}

/// Borrows the payload of a `pathlib.Path` object.
pub(crate) fn as_path(v: i64) -> Option<&'static PathState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_PATH {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Path(state) => Some(state),
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
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_ARG_PARSER {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::ArgParser(state) => Some(state),
        _ => None,
    }
}

/// Borrows the attribute list of an `argparse.Namespace`.
pub(crate) fn as_namespace(v: i64) -> Option<&'static [(String, i64)]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_NAMESPACE {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Namespace(state) => Some(state.pairs.as_slice()),
        _ => None,
    }
}

/// Borrows a `collections.defaultdict` as its pairs plus the factory value.
pub(crate) fn as_defaultdict(v: i64) -> Option<(&'static [(i64, i64)], i64)> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_DEFAULTDICT {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::DefaultDict(state) => Some((state.pairs.as_slice(), state.factory)),
        _ => None,
    }
}

/// Borrows the state of a compiled `re.Pattern`.
pub(crate) fn as_pattern(v: i64) -> Option<&'static crate::modules::re::PatternState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_PATTERN {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Pattern(state) => Some(state),
        _ => None,
    }
}

/// Borrows the state of an `re.Match`.
pub(crate) fn as_match(v: i64) -> Option<&'static crate::modules::re::MatchState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_MATCH {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Match(state) => Some(state),
        _ => None,
    }
}

/// Borrows the native state of a `socket` object.
pub(crate) fn as_socket(v: i64) -> Option<&'static crate::modules::socket::SocketState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_SOCKET {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Socket(state) => Some(state),
        _ => None,
    }
}

pub(crate) fn exception_data(v: i64) -> Option<&'static ExcData> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_EXCEPTION {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Exception(e) => Some(e),
        _ => None,
    }
}

/// Borrows the state of a class object.
pub(crate) fn as_class(v: i64) -> Option<&'static ClassState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_CLASS {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Class(state) => Some(state),
        _ => None,
    }
}

/// Borrows the state of a module value.
pub(crate) fn as_module(v: i64) -> Option<&'static ModuleState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_MODULE {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Module(state) => Some(state),
        _ => None,
    }
}

/// Borrows the state of a native standard stream (`sys.stdin`/`sys.stdout`/
/// `sys.stderr`).
pub(crate) fn as_std_stream(v: i64) -> Option<&'static StdStreamState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted/immutable (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_STD_STREAM {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::StdStream(state) => Some(state),
        _ => None,
    }
}

/// Allocates a fresh standard stream over `fd` (0, 1 or 2).
///
/// A fresh object per resolution keeps the value GC-safe without a rooted
/// singleton; identity (`sys.stdout is sys.stdout`) is therefore not stable,
/// which no supported program observes.
pub(crate) fn std_stream_new(fd: i32) -> i64 {
    alloc(KIND_STD_STREAM, ObjData::StdStream(StdStreamState { fd }))
}

/// Field values of a `typing.NamedTuple` instance in attribute insertion
/// order. NamedTuple classes are empty native classes whose base is the
/// `NamedTuple` placeholder; only that class form is recognised.
pub(crate) fn namedtuple_values(instance: i64) -> Option<Vec<i64>> {
    let state = as_instance(instance)?;
    let class = as_class(state.class)?;
    let base = as_class(class.base)?;
    if base.name != "NamedTuple" {
        return None;
    }
    Some(
        state
            .attrs
            .snapshot()
            .into_iter()
            .map(|(_, value)| value)
            .collect(),
    )
}

pub(crate) fn as_instance(v: i64) -> Option<&'static InstanceState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_INSTANCE {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Instance(state) => Some(state),
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

/// One entry of a class namespace, as resolved through the base chain.
///
/// A class body cannot define the same name twice (the frontend diagnoses
/// it), but a subclass attribute may shadow a base-class method, so attribute
/// and method lookups share one namespace walk.
pub(crate) enum ClassEntry {
    /// Frozen class attribute value.
    Attr(i64),
    /// Method with its binding kind.
    Method(MethodKind, i64),
}

/// Looks up `name` on `class` and its bases.
///
/// The class namespace is immutable after construction, so bases form a finite
/// chain; the iteration cap is defensive against a malformed heap object.
pub(crate) fn class_lookup(class: i64, name: &str) -> Option<ClassEntry> {
    let mut current = class;
    for _ in 0..1024 {
        let state = as_class(current)?;
        // Attributes and methods live in separate vectors for tracing, but
        // they model one class dictionary; attributes are checked first
        // because duplicate names are rejected when the class is created.
        if let Some(value) = state.attr(name) {
            return Some(ClassEntry::Attr(value));
        }
        if let Some((kind, function)) = state.method(name) {
            return Some(ClassEntry::Method(kind, function));
        }
        if state.base == NONE {
            return None;
        }
        current = state.base;
    }
    None
}

/// Looks up `name` on `class` and its bases, returning its binding kind and
/// method value. Attributes of the same name are ignored.
pub(crate) fn class_member(class: i64, name: &str) -> Option<(MethodKind, i64)> {
    match class_lookup(class, name)? {
        ClassEntry::Method(kind, function) => Some((kind, function)),
        ClassEntry::Attr(_) => None,
    }
}

/// Looks up `name` on `class` and its bases, returning the method value
/// regardless of its binding kind.
pub(crate) fn class_method(class: i64, name: &str) -> Option<i64> {
    class_member(class, name).map(|(_, function)| function)
}

/// Snapshot of `instance.__dict__` in insertion order. Method attributes are
/// resolved by [`crate::methods::call`] instead.
pub(crate) fn instance_dict(instance: i64) -> Option<i64> {
    let state = as_instance(instance)?;
    let snapshot = state.attrs.snapshot();
    let result = dict_new(Vec::new());
    let root = crate::heap::root_values(&[result]);
    for (name, value) in snapshot {
        let key = str_alloc(name);
        crate::container::dict_set(result, key, value);
    }
    crate::heap::release_roots(root);
    Some(result)
}

/// Replaces `instance.__dict__` from a mapping of string keys.
pub(crate) fn instance_set_dict(instance: i64, mapping: i64) -> bool {
    let Some(state) = as_instance(instance) else {
        return false;
    };
    let Some(pairs) = as_dict(mapping) else {
        return false;
    };
    let copy = pairs.to_vec();
    state.attrs.clear();
    for (key, value) in copy {
        // `__dict__` keys must be strings; ignore foreign keys rather than
        // rejecting the whole mapping, so a partially malformed mapping still
        // restores the valid attribute set.
        if let Some(name) = as_str(key) {
            state.attrs.set(instance, name.to_string(), value);
        }
    }
    true
}

pub(crate) fn instance_attr(instance: i64, name: &str) -> Option<i64> {
    as_instance(instance)?.attrs.get(name)
}

/// Stores an instance data attribute, preserving its first insertion position.
pub(crate) fn instance_setattr(instance: i64, name: String, value: i64) {
    if let Some(state) = as_instance(instance) {
        state.attrs.set(instance, name, value);
    }
}

/// `dataclasses.replace(obj, **changes)`: shallow copy of `instance`'s
/// attribute table into a fresh instance of the same class.
///
/// Attributes keep their insertion order; each `(name, value)` in `changes`
/// overwrites the copied slot or is appended when the name is new. Names are
/// not validated against the class's declared dataclass fields: the frontend
/// expands `@dataclass` and the runtime keeps no field metadata, so an
/// unknown name is added as an ordinary attribute (CPython would raise
/// `TypeError`).
///
/// Every copied and replaced value is installed through
/// [`instance_setattr`], which publishes the insertion barrier for the new
/// instance, so a concurrent marker cannot miss a copied child.
///
/// Returns `None` when `instance` is not a class instance.
pub(crate) fn instance_replace(instance: i64, changes: Vec<(String, i64)>) -> Option<i64> {
    let state = as_instance(instance)?;
    let class = state.class;
    // Snapshot before allocating: `instance_new` can collect, and the pairs
    // are owned Rust data afterwards.
    let pairs = state.attrs.snapshot();
    let copy = instance_new(class);
    for (name, value) in pairs {
        instance_setattr(copy, name, value);
    }
    for (name, value) in changes {
        instance_setattr(copy, name, value);
    }
    Some(copy)
}

/// Guarded kind checks: `None` for every non-heap value and for heap objects
/// of a different kind, so a malformed or mistyped value is never
/// dereferenced as a container.
pub(crate) fn as_list(v: i64) -> Option<&'static [i64]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_LIST {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::List(items) => Some(items.as_slice()),
        _ => None,
    }
}

/// Borrows a dict's payload (entries plus key index).
pub(crate) fn as_dict_data(v: i64) -> Option<&'static DictData> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_DICT {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Dict(dict) => Some(dict),
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
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_TUPLE {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Tuple(items) => Some(items.as_slice()),
        _ => None,
    }
}

pub(crate) fn as_set(v: i64) -> Option<&'static [i64]> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_SET {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Set(items) => Some(items.as_slice()),
        _ => None,
    }
}

/// Returns `(start, stop, step)` for a range object.
pub(crate) fn as_range(v: i64) -> Option<(i64, i64, i64)> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_RANGE {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Range { start, stop, step } => Some((start, stop, step)),
        _ => None,
    }
}

pub(crate) fn as_function(v: i64) -> Option<&'static FunctionObj> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_FUNCTION {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Function(function) => Some(function),
        _ => None,
    }
}

/// Returns the shim handle of an extension-module function value.
pub(crate) fn as_ext_function(v: i64) -> Option<&'static ExtFunctionState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_EXT_FUNCTION {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::ExtFunction(state) => Some(state),
        _ => None,
    }
}

/// Allocates an opaque pointer capsule.
///
/// `ptr` is owned by the extension and is never dereferenced or freed by Hydro;
/// capsules carry no Hydro children and have no destructor.
pub(crate) fn capsule_new(ptr: usize, name: Option<String>) -> i64 {
    alloc(KIND_CAPSULE, ObjData::Capsule { ptr, name })
}

/// Returns a capsule's pointer and name, or `None` for other values.
pub(crate) fn as_capsule(v: i64) -> Option<(usize, Option<&'static str>)> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_CAPSULE {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Capsule { ptr, name } => Some((ptr, name.map(String::as_str))),
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
    // SAFETY: heap tag checked; object rooted, payload shared through a `Mutex`.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_LOCK {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Lock(lock) => Some(lock),
        _ => None,
    }
}

/// Returns the state of a `threading.Thread` object.
pub(crate) fn as_thread(v: i64) -> Option<&'static ThreadState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted and shared through atomics/mutexes.
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_THREAD {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Thread(thread) => Some(thread),
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
    match obj.data_ref() {
        ObjDataRef::Event(event) => Some(event),
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
    match obj.data_ref() {
        ObjDataRef::Queue(queue) => Some(queue),
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
    match obj.data_ref() {
        ObjDataRef::Process(process) => Some(process),
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
    match obj.data_ref() {
        ObjDataRef::MpQueue(queue) => Some(queue),
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
    match obj.data_ref() {
        ObjDataRef::Pool(pool) => Some(pool),
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
    match obj.data_ref() {
        ObjDataRef::File(file) => Some(file),
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
    match obj.data_ref() {
        ObjDataRef::CsvReader(reader) => Some(reader),
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
    match obj.data_ref() {
        ObjDataRef::CsvWriter(writer) => Some(writer),
        _ => None,
    }
}

pub(crate) fn as_iter(v: i64) -> Option<&'static IterState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_ITER {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Iter(state) => Some(state.as_ref()),
        _ => None,
    }
}

/// Borrows the fiber state behind a generator/coroutine object.
pub(crate) fn as_fiber(v: i64) -> Option<&'static crate::fiber::FiberState> {
    if !is_heap(v) {
        return None;
    }
    // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
    let obj = unsafe { as_obj(v) }?;
    if obj.kind != KIND_FIBER {
        return None;
    }
    match obj.data_ref() {
        ObjDataRef::Fiber(state) => Some(state.as_ref()),
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
    let obj = unsafe { &mut *((v & !TAG_MASK) as *mut HydroObj) };
    if obj.kind != KIND_FIBER {
        return None;
    }
    match obj.data_mut() {
        ObjDataMut::Fiber(state) => Some(state.as_mut()),
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
/// Rules for callers: no Hydro heap allocation, no safepoint and no user code
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
    // SAFETY: heap tag checked; object rooted for the borrow (collector frees only unreachable).
    let obj = unsafe { &*((v & !TAG_MASK) as *const HydroObj) };
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
    // SAFETY: heap tag/kind checked; value owned by the executing program (see contract).
    let obj: &'static mut HydroObj = unsafe { &mut *((v & !TAG_MASK) as *mut HydroObj) };
    match obj.data_mut() {
        ObjDataMut::List(items) => Some(ContainerGuard::new(items, graph)),
        _ => None,
    }
}

/// Returns exclusive access to a dict's payload. See [`list_items_mut`].
pub(crate) fn dict_pairs_mut(v: i64) -> Option<ContainerGuard<DictData>> {
    let graph = lock_container(v, KIND_DICT)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut HydroObj = unsafe { &mut *((v & !TAG_MASK) as *mut HydroObj) };
    match obj.data_mut() {
        ObjDataMut::Dict(dict) => Some(ContainerGuard::new(dict, graph)),
        _ => None,
    }
}

/// Returns exclusive access to a set's elements. See [`list_items_mut`].
pub(crate) fn set_items_mut(v: i64) -> Option<ContainerGuard<Vec<i64>>> {
    let graph = lock_container(v, KIND_SET)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut HydroObj = unsafe { &mut *((v & !TAG_MASK) as *mut HydroObj) };
    match obj.data_mut() {
        ObjDataMut::Set(items) => Some(ContainerGuard::new(items, graph)),
        _ => None,
    }
}

/// Returns exclusive access to an `argparse.ArgumentParser`'s definitions.
pub(crate) fn arg_parser_mut(
    v: i64,
) -> Option<ContainerGuard<crate::modules::argparse::ArgParserState>> {
    let graph = lock_container(v, KIND_ARG_PARSER)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut HydroObj = unsafe { &mut *((v & !TAG_MASK) as *mut HydroObj) };
    match obj.data_mut() {
        ObjDataMut::ArgParser(state) => Some(ContainerGuard::new(state, graph)),
        _ => None,
    }
}

/// Returns exclusive access to an `argparse.Namespace`'s attribute pairs.
pub(crate) fn namespace_pairs_mut(v: i64) -> Option<ContainerGuard<NamespaceState>> {
    let graph = lock_container(v, KIND_NAMESPACE)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut HydroObj = unsafe { &mut *((v & !TAG_MASK) as *mut HydroObj) };
    match obj.data_mut() {
        ObjDataMut::Namespace(state) => Some(ContainerGuard::new(state, graph)),
        _ => None,
    }
}

/// Returns exclusive access to a `collections.defaultdict`'s pairs/factory.
pub(crate) fn defaultdict_state_mut(
    v: i64,
) -> Option<ContainerGuard<crate::modules::collections::DefaultDictState>> {
    let graph = lock_container(v, KIND_DEFAULTDICT)?;
    // SAFETY: see `list_items_mut`.
    let obj: &'static mut HydroObj = unsafe { &mut *((v & !TAG_MASK) as *mut HydroObj) };
    match obj.data_mut() {
        ObjDataMut::DefaultDict(state) => Some(ContainerGuard::new(state, graph)),
        _ => None,
    }
}

/// Returns exclusive access to iterator state. See [`list_items_mut`].
pub(crate) fn iter_state_mut(v: i64) -> Option<&'static mut IterState> {
    if !is_heap(v) || (v & !TAG_MASK) == 0 {
        return None;
    }
    // SAFETY: see `list_items_mut`; iterators are per loop and advanced only by the runtime.
    let obj = unsafe { &mut *((v & !TAG_MASK) as *mut HydroObj) };
    if obj.kind != KIND_ITER {
        return None;
    }
    match obj.data_mut() {
        ObjDataMut::Iter(state) => Some(state.as_mut()),
        _ => None,
    }
}

/// Clones the exception payload behind a tagged value (used by tracebacks so
/// that the thread-local state can be cleared afterwards).
pub(crate) fn exception_clone(v: i64) -> Option<ExcData> {
    exception_data(v).cloned()
}

// Heap allocation (collector contract, ARCHITECTURE §7)

/// Allocates a heap object owned by the collector.
///
/// The allocation, registration and any policy-driven collection happen in
/// [`crate::heap::allocate`] (under the global-heap lock once `_thread` has
/// switched to the shared heap).
///
/// Audit note: an `&str`/payload borrow/`&mut` reference into *another* heap
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

/// Builds a Python `int` from an arbitrary-precision value: a tagged
/// immediate when it fits the 61-bit tag range, otherwise a `BigInt` heap
/// object. Callers that already hold an `i64` use
/// [`crate::ops::int_result`], which preserves the tagged fast path without
/// constructing a `BigInt` first.
pub(crate) fn bigint_new(n: BigInt) -> i64 {
    if let Ok(small) = i64::try_from(&n) {
        if let Some(tagged) = encode_int(small) {
            return tagged;
        }
    }
    alloc(KIND_BIGINT, ObjData::BigInt(n))
}

/// Parses a decimal integer literal from an ABI `(ptr, len)` buffer, the
/// `hydro_bigint_from_str` entry point. The result is a tagged `int` when it
/// fits and a heap `BigInt` otherwise; malformed text raises `ValueError`
/// and returns `None`.
pub(crate) fn bigint_from_ptr(ptr: *const u8, len: usize) -> i64 {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return invalid_int_literal("");
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    match std::str::from_utf8(bytes) {
        Ok(text) => match parse_decimal_int(text) {
            Some(n) => bigint_new(n),
            None => invalid_int_literal(text),
        },
        Err(_) => invalid_int_literal(&String::from_utf8_lossy(bytes)),
    }
}

/// Raises `ValueError` for a malformed integer literal and returns `None`.
fn invalid_int_literal(text: &str) -> i64 {
    crate::exc::raise_with_message(
        crate::exc::VALUE_ERROR,
        &format!("invalid literal for int() with base 10: '{text}'"),
    )
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

/// Allocates an `argparse.ArgumentParser` with a fully built state.
pub(crate) fn arg_parser_new(state: crate::modules::argparse::ArgParserState) -> i64 {
    alloc(KIND_ARG_PARSER, ObjData::ArgParser(state))
}

pub(crate) fn namespace_new(pairs: Vec<(String, i64)>) -> i64 {
    alloc(KIND_NAMESPACE, ObjData::Namespace(NamespaceState { pairs }))
}

/// Allocates a class object. `base` is `NONE` or an existing class; methods
/// are function values in declaration order (duplicates already collapsed by
/// the ABI entry point).
pub(crate) fn class_new(name: String, base: i64, methods: Vec<(String, i64)>) -> i64 {
    let methods = methods
        .into_iter()
        .map(|(name, function)| (name, function, MethodKind::Normal))
        .collect();
    class_new_full(name, base, methods, Vec::new())
}

/// Allocates a class object with method binding kinds and frozen class
/// attributes. See [`class_new`].
pub(crate) fn class_new_full(
    name: String,
    base: i64,
    methods: Vec<(String, i64, MethodKind)>,
    attrs: Vec<(String, i64)>,
) -> i64 {
    let class = alloc(
        KIND_CLASS,
        ObjData::Class(ClassState {
            name,
            base,
            methods,
            attrs,
        }),
    );
    // A subclass of the native unittest `TestCase` joins the test registry so
    // `unittest.main()` can discover and run it.
    if base != NONE && class_method(base, "__unittest_case__").is_some() {
        crate::modules::unittest::register(class);
    }
    // `Test*` classes with `test*` methods join the pytest registry; the
    // pytest runner instantiates them and calls each method.
    if as_class(class).is_some() {
        crate::modules::pytest::register_class(class);
    }
    class
}

/// Allocates a module value rendering as `<module 'name'>`.
pub(crate) fn module_value(name: String) -> i64 {
    alloc(KIND_MODULE, ObjData::Module(ModuleState { name }))
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
    let mut values = Vec::with_capacity(pairs.len() * 2 + 1);
    values.push(factory);
    for (key, value) in &pairs {
        values.push(*key);
        values.push(*value);
    }
    let roots = crate::gc::heap::root_values(&values);
    let result = alloc(
        KIND_DEFAULTDICT,
        ObjData::DefaultDict(crate::modules::collections::DefaultDictState { factory, pairs }),
    );
    crate::gc::heap::release_roots(roots);
    result
}

pub(crate) fn pattern_new(source: String, flags: i64) -> i64 {
    alloc(
        KIND_PATTERN,
        ObjData::Pattern(crate::modules::re::PatternState { source, flags }),
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
    let roots = crate::gc::heap::root_values(&items);
    let result = alloc(KIND_LIST, ObjData::List(items));
    crate::gc::heap::release_roots(roots);
    result
}

pub(crate) fn dict_new(pairs: Vec<(i64, i64)>) -> i64 {
    let mut values = Vec::with_capacity(pairs.len() * 2);
    for (key, value) in &pairs {
        values.push(*key);
        values.push(*value);
    }
    let roots = crate::gc::heap::root_values(&values);
    let result = dict_new_data(DictData::new(pairs));
    crate::gc::heap::release_roots(roots);
    result
}

/// Allocates a dict from an already indexed payload (the constructor path,
/// which deduplicates while building the index).
pub(crate) fn dict_new_data(data: DictData) -> i64 {
    alloc(KIND_DICT, ObjData::Dict(data))
}

pub(crate) fn tuple_new(items: Vec<i64>) -> i64 {
    let roots = crate::gc::heap::root_values(&items);
    let result = alloc(KIND_TUPLE, ObjData::Tuple(items));
    crate::gc::heap::release_roots(roots);
    result
}

pub(crate) fn set_new(items: Vec<i64>) -> i64 {
    let roots = crate::gc::heap::root_values(&items);
    let result = alloc(KIND_SET, ObjData::Set(items));
    crate::gc::heap::release_roots(roots);
    result
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

/// Allocates a closure cell holding `value`.
///
/// Generated code keeps the cell in the local slot of a variable that a nested
/// function captures and mutates; the closure captures the cell value itself,
/// so every reader observes the current binding.
pub(crate) fn cell_new(value: i64) -> i64 {
    alloc(KIND_CELL, ObjData::Cell(value))
}

/// Reads the value held by `cell`. Non-cell values yield [`NONE`]; generated
/// code only calls this for locals the frontend typed as cells.
pub(crate) fn cell_get(cell: i64) -> i64 {
    let Some(_graph) = lock_container(cell, KIND_CELL) else {
        return NONE;
    };
    // SAFETY: heap tag/kind checked under the graph lock.
    let obj = unsafe { &*((cell & !TAG_MASK) as *const HydroObj) };
    match obj.data_ref() {
        ObjDataRef::Cell(value) => value,
        _ => NONE,
    }
}

/// Stores `value` into `cell` and publishes the new GC edge.
pub(crate) fn cell_set(cell: i64, value: i64) {
    let Some(_graph) = lock_container(cell, KIND_CELL) else {
        return;
    };
    {
        // SAFETY: heap tag/kind checked under the graph lock; the collector
        // only frees unreachable objects.
        let obj = unsafe { &mut *((cell & !TAG_MASK) as *mut HydroObj) };
        let ObjDataMut::Cell(slot) = obj.data_mut() else {
            return;
        };
        *slot = value;
    }
    // Publish the new edge while the mutation is still serialised against the
    // marker.
    crate::heap::write_barrier_edge(cell, value);
}

// String accumulators (frontend string-accumulator pass)

/// Seeding of a builder. The static proof guarantees a `str`; a non-string
/// value is kept verbatim so the defensive path still ends with it.
pub(crate) fn str_builder_new(init: i64) -> i64 {
    let state = match as_str(init) {
        Some(text) => StrBuilderState::Building(text.to_string()),
        None => StrBuilderState::Materialized(init),
    };
    alloc(KIND_STR_BUILDER, ObjData::StrBuilder(state))
}

/// Runs `f` with exclusive access to a builder payload.
///
/// The caller keeps `builder` reachable (generated code holds it in a
/// GC-scanned local) and performs no allocation, user call or safepoint
/// inside `f` (container-guard contract).
fn with_str_builder<R>(builder: i64, f: impl FnOnce(&mut StrBuilderState) -> R) -> Option<R> {
    let _graph = lock_container(builder, KIND_STR_BUILDER)?;
    // SAFETY: heap tag and kind checked under the graph lock; the calling
    // ABI entry keeps the value rooted for the borrow.
    let obj = unsafe { &mut *((builder & !TAG_MASK) as *mut HydroObj) };
    match obj.data_mut() {
        ObjDataMut::StrBuilder(state) => Some(f(state)),
        _ => None,
    }
}

/// The accumulated value extracted for one append/finish step.
enum BuilderCurrent {
    /// Text moved out of a [`StrBuilderState::Building`] payload.
    Text(String),
    /// A materialised tagged value; the builder still traces it until the
    /// caller replaces the payload.
    Value(i64),
}

/// Moves the accumulated value out of the builder, leaving an empty buffer.
fn take_builder_value(builder: i64) -> Option<BuilderCurrent> {
    with_str_builder(builder, |state| match state {
        StrBuilderState::Building(buffer) => BuilderCurrent::Text(std::mem::take(buffer)),
        StrBuilderState::Materialized(value) => BuilderCurrent::Value(*value),
    })
}

/// Stores a generic concatenation result and publishes its GC edge.
fn store_builder_value(builder: i64, value: i64) {
    // A generic `+` that still produced a string keeps the fast append path
    // (one copy) instead of degrading to one copy per iteration.
    let state = match as_str(value) {
        Some(text) => StrBuilderState::Building(text.to_string()),
        None => StrBuilderState::Materialized(value),
    };
    let edge = match &state {
        StrBuilderState::Materialized(value) => Some(*value),
        StrBuilderState::Building(_) => None,
    };
    let Some(_graph) = lock_container(builder, KIND_STR_BUILDER) else {
        return;
    };
    {
        // SAFETY: heap tag/kind checked under the graph lock.
        let obj = unsafe { &mut *((builder & !TAG_MASK) as *mut HydroObj) };
        let ObjDataMut::StrBuilder(slot) = obj.data_mut() else {
            return;
        };
        *slot = state;
    }
    // Publish the new edge while the payload stays serialised against the
    // marker, exactly like [`cell_set`].
    if let Some(edge) = edge {
        crate::heap::write_barrier_edge(builder, edge);
    }
}

/// `accumulator += value` for a proved accumulator: appends strings in place
/// and keeps the exact `+` semantics (including the raised `TypeError`, the
/// overflow check and `__radd__`) for anything else. Always returns the
/// builder, which the generated code writes back to its slot.
pub(crate) fn str_builder_append(builder: i64, value: i64) -> i64 {
    // Fast path: no Hydro allocation (and therefore no collection) happens
    // while the buffer grows, matching `str + str` without the copy.
    if let Some(text) = as_str(value) {
        let appended = with_str_builder(builder, |state| match state {
            StrBuilderState::Building(buffer) => {
                buffer.push_str(text);
                true
            }
            StrBuilderState::Materialized(_) => false,
        });
        if matches!(appended, Some(true)) {
            return builder;
        }
    }
    // Generic path: `ops::add` may collect, raise or call user code, so both
    // operands and the materialised prefix are rooted for its duration.
    let start = crate::heap::root_values(&[builder, value]);
    let Some(current) = take_builder_value(builder) else {
        crate::heap::release_roots(start);
        return builder;
    };
    let current = match current {
        BuilderCurrent::Text(text) => str_alloc(text),
        BuilderCurrent::Value(value) => value,
    };
    let inner = crate::heap::root_values(&[current]);
    let sum = crate::ops::add(current, value);
    store_builder_value(builder, sum);
    crate::heap::release_roots(inner);
    crate::heap::release_roots(start);
    builder
}

/// Ends a builder and returns the accumulated text as a normal `str`.
pub(crate) fn str_builder_finish(builder: i64) -> i64 {
    let start = crate::heap::root_values(&[builder]);
    let Some(current) = take_builder_value(builder) else {
        crate::heap::release_roots(start);
        return NONE;
    };
    let result = match current {
        BuilderCurrent::Text(text) => str_alloc(text),
        BuilderCurrent::Value(value) => value,
    };
    crate::heap::release_roots(start);
    result
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
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
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
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
        }),
    )
}

/// Allocates a native module function value. `arity` is the upper bound from
/// `hydro_ir::stdlib`; the actual call arity is validated by the module
/// dispatcher against the central spec.
pub(crate) fn native_function_new(module: &'static str, name: &'static str, arity: usize) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Native { module, name },
            arity,
            name: name.to_string(),
            flags: 0,
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
        }),
    )
}

/// Allocates a builtin function value. Builtins validate their own arity, so
/// `arity` is `usize::MAX` (variadic); callers never rely on it.
pub(crate) fn builtin_function_new(builtin: hydro_ir::Builtin) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Builtin(builtin),
            arity: usize::MAX,
            name: builtin.name().to_string(),
            flags: 0,
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
        }),
    )
}

/// Allocates an interpreter-backed function value.
///
/// `token` identifies the IR function inside the interpreter's program
/// registry; `captures` are stored as traced GC edges. The parameter metadata
/// drives the runtime keyword binder and the generic call path; `full_entry`
/// carries the non-zero sentinel that enables keyword binding for host
/// functions even though no machine-code trampoline exists.
pub(crate) fn host_function_new(
    token: u64,
    captures: Vec<i64>,
    arity: usize,
    name: String,
    flags: u32,
    params: Vec<(String, ParamKind, Option<i64>)>,
) -> i64 {
    let signature: Vec<ParamMeta> = params
        .iter()
        .map(|(name, kind, default)| ParamMeta {
            name: name.clone(),
            kind: *kind,
            default: *default,
        })
        .collect();
    let positional = signature
        .iter()
        .filter(|param| param.kind == ParamKind::Positional)
        .map(|param| param.name.clone())
        .collect();
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Host { token, captures },
            arity,
            name,
            flags,
            // Non-zero sentinel: keyword/omitted-default binding must route
            // through the runtime binder for interpreter functions too.
            full_entry: 1,
            signature,
            params: positional,
        }),
    )
}

/// Allocates an `operator.attrgetter` key function. `arity` is `1`: the
/// getter takes the object it reads from.
pub(crate) fn attribute_getter_new(names: Vec<String>) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::AttributeGetter { names },
            arity: 1,
            name: "attrgetter".to_string(),
            flags: 0,
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
        }),
    )
}

/// Allocates an `operator.itemgetter` key function. Keys are moved into the
/// body, which traces them as GC edges.
pub(crate) fn item_getter_new(keys: Vec<i64>) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::ItemGetter { keys },
            arity: 1,
            name: "itemgetter".to_string(),
            flags: 0,
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
        }),
    )
}

/// Allocates a `functools.partial` value binding `args` as leading positional
/// arguments of `function`.
pub(crate) fn partial_new(function: i64, args: Vec<i64>) -> i64 {
    partial_new_with_keywords(function, args, Vec::new())
}

/// Allocates a `functools.partial` value binding `args` as leading positional
/// arguments and `keywords` by parameter name.
///
/// The keywords are applied when the partial is called: the target's parameter
/// names (attached by codegen) place each value in its slot, and every named
/// value is either consumed or reported with a `TypeError`. Nothing is
/// silently dropped.
///
/// `arity` mirrors the remaining parameter count of a generated wrapper so the
/// error paths that inspect it (`Thread`, `Process`, `Pool` targets) stay
/// meaningful; other callables validate their own arity and store
/// `usize::MAX`.
pub(crate) fn partial_new_with_keywords(
    function: i64,
    args: Vec<i64>,
    keywords: Vec<(String, i64)>,
) -> i64 {
    let bound = args.len().saturating_add(keywords.len());
    let remaining = match as_function(function) {
        Some(func) => match &func.body {
            FunctionBody::Native { .. }
            | FunctionBody::Builtin(_)
            | FunctionBody::BoundMethod { .. } => usize::MAX,
            // Generated wrappers count user-visible parameters; the derived
            // callables here take exactly one argument.
            FunctionBody::Wrapper { .. }
            | FunctionBody::Host { .. }
            | FunctionBody::Partial { .. }
            | FunctionBody::Identity
            | FunctionBody::AttributeGetter { .. }
            | FunctionBody::ItemGetter { .. } => func.arity.saturating_sub(bound),
        },
        None => usize::MAX,
    };
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Partial {
                function,
                args,
                keywords,
            },
            arity: remaining,
            name: "partial".to_string(),
            flags: 0,
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
        }),
    )
}

/// Attaches the user-visible positional parameter names to a function value.
///
/// Codegen calls this right after creating a generated wrapper. Values that
/// are not function objects are ignored, matching the ABI's tolerance for
/// best-effort metadata.
pub(crate) fn function_set_params(function: i64, params: Vec<String>) {
    if !is_heap(function) || (function & !TAG_MASK) == 0 {
        return;
    }
    // SAFETY: heap tag checked; function live for the call, parameter names own no Hydro values.
    let obj = unsafe { &mut *((function & !TAG_MASK) as *mut HydroObj) };
    if obj.kind != KIND_FUNCTION {
        return;
    }
    if let ObjDataMut::Function(func) = obj.data_mut() {
        func.params = params;
    }
}

/// Sets the uniform full-argv trampoline address for a function value.
pub(crate) fn function_set_full_entry(function: i64, entry: usize) {
    if !is_heap(function) || (function & !TAG_MASK) == 0 {
        return;
    }
    let obj = unsafe { &mut *((function & !TAG_MASK) as *mut HydroObj) };
    if obj.kind == KIND_FUNCTION {
        if let ObjDataMut::Function(func) = obj.data_mut() {
            func.full_entry = entry;
        }
    }
}

/// Stores full parameter metadata on a function value.
pub(crate) fn function_set_signature(function: i64, signature: Vec<ParamMeta>) {
    if !is_heap(function) || (function & !TAG_MASK) == 0 {
        return;
    }
    let obj = unsafe { &mut *((function & !TAG_MASK) as *mut HydroObj) };
    if obj.kind == KIND_FUNCTION {
        if let ObjDataMut::Function(func) = obj.data_mut() {
            func.params = signature
                .iter()
                .filter(|param| param.kind == ParamKind::Positional)
                .map(|param| param.name.clone())
                .collect();
            func.signature = signature;
        }
    }
}

/// Full parameter metadata of a function value.
pub(crate) fn function_signature(function: i64) -> &'static [ParamMeta] {
    match as_function(function) {
        Some(func) => &func.signature,
        None => &[],
    }
}

/// Uniform full-argv trampoline address, or `0` when unavailable.
pub(crate) fn function_full_entry(function: i64) -> usize {
    match as_function(function) {
        Some(func) => func.full_entry,
        None => 0,
    }
}

/// The user-visible positional parameter names of a function value, or an
/// empty slice for callables without them.
pub(crate) fn function_params(function: i64) -> &'static [String] {
    match as_function(function) {
        Some(func) => &func.params,
        None => &[],
    }
}

/// Allocates a native bound method value (`receiver.method`).
pub(crate) fn bound_method_new(receiver: i64, method: String) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::BoundMethod {
                receiver,
                method: method.clone(),
            },
            arity: usize::MAX,
            name: method,
            flags: 0,
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
        }),
    )
}

/// Allocates the identity callable returned by `functools.lru_cache` and
/// `functools.wraps` (the decorator) and used by `functools.cache`.
pub(crate) fn identity_function_new(name: &str) -> i64 {
    alloc(
        KIND_FUNCTION,
        ObjData::Function(FunctionObj {
            body: FunctionBody::Identity,
            arity: 1,
            name: name.to_string(),
            flags: 0,
            full_entry: 0,
            signature: Vec::new(),
            params: Vec::new(),
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
/// `hydro_iter_next`.
pub(crate) fn fiber_generator_new(entry: usize, name: String, args: Vec<i64>) -> Option<i64> {
    fiber_alloc(crate::fiber::FiberKind::Generator, entry, name, args)
}

/// Creates a coroutine object. No body code runs until the first `await`.
pub(crate) fn fiber_coroutine_new(entry: usize, name: String, args: Vec<i64>) -> Option<i64> {
    fiber_alloc(crate::fiber::FiberKind::Coroutine, entry, name, args)
}

/// Creates a lazy async generator object. No body code runs until the first
/// `__anext__`/`asend`.
pub(crate) fn fiber_async_generator_new(entry: usize, name: String, args: Vec<i64>) -> Option<i64> {
    fiber_alloc(crate::fiber::FiberKind::AsyncGenerator, entry, name, args)
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

// Python-visible semantics

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
            3 => "NotImplementedType",
            _ => "object",
        };
    }
    if is_heap(v) {
        // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
        return match unsafe { as_obj(v) }.map(|o| o.data_ref()) {
            Some(ObjDataRef::Str(_)) => "str",
            Some(ObjDataRef::Float(_)) => "float",
            Some(ObjDataRef::BigInt(_)) => "int",
            Some(ObjDataRef::Exception(e)) => e.name,
            Some(ObjDataRef::List(_)) => "list",
            Some(ObjDataRef::Dict(_)) => "dict",
            Some(ObjDataRef::Iter(_)) => "iterator",
            // Internal accumulator: unreachable from user code, but reported
            // as the value it will finish as.
            Some(ObjDataRef::StrBuilder(_)) => "str",
            Some(ObjDataRef::Tuple(_)) => "tuple",
            Some(ObjDataRef::Set(_)) => "set",
            Some(ObjDataRef::Range { .. }) => "range",
            Some(ObjDataRef::Function(_)) => "function",
            Some(ObjDataRef::Lock(_)) => "lock",
            Some(ObjDataRef::Thread(_)) => "Thread",
            Some(ObjDataRef::Event(_)) => "Event",
            Some(ObjDataRef::Queue(_)) => "Queue",
            Some(ObjDataRef::Process(_)) => "Process",
            Some(ObjDataRef::MpQueue(_)) => "Queue",
            Some(ObjDataRef::Pool(_)) => "Pool",
            Some(ObjDataRef::File(_)) => "file",
            Some(ObjDataRef::CsvReader(_)) => "csv.reader",
            Some(ObjDataRef::CsvWriter(_)) => "csv.writer",
            Some(ObjDataRef::Bytes(_)) => "bytes",
            Some(ObjDataRef::Random(_)) => "Random",
            Some(ObjDataRef::Path(_)) => "Path",
            Some(ObjDataRef::ArgParser(_)) => "ArgumentParser",
            Some(ObjDataRef::Namespace(_)) => "Namespace",
            Some(ObjDataRef::DefaultDict(_)) => "defaultdict",
            Some(ObjDataRef::Pattern(_)) => "Pattern",
            Some(ObjDataRef::Match(_)) => "Match",
            Some(ObjDataRef::Socket(_)) => "socket",
            Some(ObjDataRef::Class(_)) => "type",
            Some(ObjDataRef::Instance(_)) => "instance",
            Some(ObjDataRef::ExtFunction(_)) => "builtin_function_or_method",
            Some(ObjDataRef::Capsule { .. }) => "PyCapsule",
            Some(ObjDataRef::Module(_)) => "module",
            Some(ObjDataRef::StdStream(_)) => "TextIOWrapper",
            Some(ObjDataRef::Cell(_)) => "cell",
            Some(ObjDataRef::Fiber(state)) => match state.kind {
                crate::fiber::FiberKind::Generator => "generator",
                crate::fiber::FiberKind::Coroutine => "coroutine",
                crate::fiber::FiberKind::AsyncGenerator => "async_generator",
            },
            None => "object",
        };
    }
    "object"
}

thread_local! {
    /// Cached native classes returned by `type()` for primitive values. The
    /// classes are heap objects and are registered as GC roots; the cache is
    /// cleared by the runtime reset before the heap is freed.
    static TYPE_CLASS_CACHE: RefCell<HashMap<String, i64>> = RefCell::new(HashMap::new());
}

pub(crate) fn clear_type_class_cache() {
    TYPE_CLASS_CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Cached native class named `name`, created (and registered as a collector
/// root) on first use.
fn native_class(name: &str) -> i64 {
    if let Some(class) = TYPE_CLASS_CACHE.with(|cache| cache.borrow().get(name).copied()) {
        return class;
    }
    let class = class_new(name.to_string(), NONE, Vec::new());
    crate::heap::add_thread_root(class);
    TYPE_CLASS_CACHE.with(|cache| cache.borrow_mut().insert(name.to_string(), class));
    class
}

/// Cached native `object` class: the root of Hydro's value hierarchy.
///
/// `isinstance(x, object_class())` is true for every Hydro value, and every
/// class (including every built-in exception class) is a subclass of it.
pub(crate) fn object_class() -> i64 {
    native_class("object")
}

/// Whether `v` is the cached native `object` class.
///
/// Unlike [`object_class`] this never creates the class, so a side-effect-free
/// check is available on hot paths.
pub(crate) fn is_object_class(v: i64) -> bool {
    TYPE_CLASS_CACHE.with(|cache| cache.borrow().get("object").copied() == Some(v))
}

/// Whether `v` is the cached native `type` class, i.e. the metaclass of every
/// class value.
pub(crate) fn is_type_class(v: i64) -> bool {
    TYPE_CLASS_CACHE.with(|cache| cache.borrow().get("type").copied() == Some(v))
}

/// Whether `class` is one of the cached native classes created by `type()` for
/// primitives (or the cached `object`/`type` classes).
pub(crate) fn is_cached_type_class(class: i64) -> bool {
    TYPE_CLASS_CACHE.with(|cache| cache.borrow().values().any(|cached| *cached == class))
}

/// `type(value)`: the class object for instances, otherwise a cached native
/// class named after the value's runtime type. `type` itself maps to a cached
/// class object named `type`.
pub(crate) fn type_class(v: i64) -> i64 {
    if let Some(instance) = as_instance(v) {
        return instance.class;
    }
    // A built-in exception's type is the same cached class object the name
    // `ValueError` resolves to, so `type(e) is ValueError` holds.
    if let Some(tag) = exception_data(v).map(|data| data.tag) {
        return crate::exc::exception_class(tag);
    }
    let name = if as_class(v).is_some() {
        "type".to_string()
    } else {
        type_name_of(v)
    };
    // Opaque runtime kinds report `object`; use the cached root class rather
    // than creating a second class with the same name.
    if name == "object" {
        object_class()
    } else {
        native_class(&name)
    }
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
        // `True` and Python's `NotImplemented` singleton are truthy; `None`
        // and `False` are falsy.
        return v == TRUE || v == NOT_IMPLEMENTED;
    }
    if is_heap(v) {
        // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
        return match unsafe { as_obj(v) }.map(|o| o.data_ref()) {
            Some(ObjDataRef::Str(s)) => !s.is_empty(),
            // Internal accumulator: truthy exactly when its text is.
            Some(ObjDataRef::StrBuilder(state)) => match state {
                StrBuilderState::Building(text) => !text.is_empty(),
                StrBuilderState::Materialized(value) => truthy(*value),
            },
            Some(ObjDataRef::Float(f)) => f != 0.0,
            Some(ObjDataRef::BigInt(n)) => n.sign() != Sign::NoSign,
            Some(ObjDataRef::Exception(_)) => true,
            Some(ObjDataRef::List(items)) => !items.is_empty(),
            Some(ObjDataRef::Dict(pairs)) => !pairs.is_empty(),
            Some(ObjDataRef::Iter(_)) => true,
            Some(ObjDataRef::Tuple(items)) => !items.is_empty(),
            Some(ObjDataRef::Set(items)) => !items.is_empty(),
            Some(ObjDataRef::Bytes(bytes)) => !bytes.is_empty(),
            Some(ObjDataRef::Range { start, stop, step }) => range_len(start, stop, step) > 0,
            Some(ObjDataRef::Function(_)) => true,
            // Like CPython, a lock object is always truthy.
            Some(ObjDataRef::Lock(_)) => true,
            // Synchronisation objects are always truthy.
            Some(ObjDataRef::Thread(_))
            | Some(ObjDataRef::Event(_))
            | Some(ObjDataRef::Queue(_))
            | Some(ObjDataRef::Process(_))
            | Some(ObjDataRef::MpQueue(_))
            | Some(ObjDataRef::Pool(_))
            | Some(ObjDataRef::File(_))
            | Some(ObjDataRef::CsvReader(_))
            | Some(ObjDataRef::CsvWriter(_)) => true,
            // A generator object is always truthy.
            Some(ObjDataRef::Random(_)) => true,
            // Like CPython, a path is always truthy.
            Some(ObjDataRef::Path(_)) => true,
            // Parser objects and namespaces are always truthy.
            Some(ObjDataRef::ArgParser(_)) | Some(ObjDataRef::Namespace(_)) => true,
            // A defaultdict is truthy exactly when it has entries, like dict.
            Some(ObjDataRef::DefaultDict(state)) => !state.pairs.is_empty(),
            Some(ObjDataRef::Pattern(_)) | Some(ObjDataRef::Match(_)) => true,
            // A socket object is always truthy.
            Some(ObjDataRef::Socket(_)) => true,
            // Classes and instances are always truthy (there is no `__bool__`).
            Some(ObjDataRef::Class(_)) | Some(ObjDataRef::Instance(_)) => true,
            // Generator and coroutine objects are always truthy.
            Some(ObjDataRef::Fiber(_)) => true,
            // Function objects are always truthy.
            Some(ObjDataRef::ExtFunction(_)) => true,
            // Capsules and module values are always truthy, like CPython.
            Some(ObjDataRef::Capsule { .. }) | Some(ObjDataRef::Module(_)) => true,
            // A standard stream is always truthy.
            Some(ObjDataRef::StdStream(_)) => true,
            // Cells are internal; they are always truthy.
            Some(ObjDataRef::Cell(_)) => true,
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
        crate::fiber::FiberKind::AsyncGenerator => ("async_generator", "<async_generator object>"),
    };
    if state.name.is_empty() {
        fallback.to_string()
    } else {
        format!("<{} object {}>", kind, state.name)
    }
}

/// `str()` of any runtime value (used by `print` and `hydro_to_str`).
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
            3 => "NotImplemented".to_string(),
            // Malformed special payload: fall back to a stable rendering.
            _ => "None".to_string(),
        };
    }
    if is_heap(v) {
        // SAFETY: heap tag checked; object rooted (collector frees only unreachable objects).
        if let Some(obj) = unsafe { as_obj(v) } {
            return match obj.data_ref() {
                ObjDataRef::Str(s) => s.clone(),
                // Internal accumulator: renders the text it will finish as.
                ObjDataRef::StrBuilder(state) => match state {
                    StrBuilderState::Building(text) => text.clone(),
                    StrBuilderState::Materialized(value) => to_string(*value),
                },
                ObjDataRef::Float(f) => format_float(f),
                ObjDataRef::BigInt(n) => n.to_string(),
                // `str(exception)` is its message (Python behaviour).
                ObjDataRef::Exception(e) => e.msg.clone(),
                ObjDataRef::List(_)
                | ObjDataRef::Dict(_)
                | ObjDataRef::Tuple(_)
                | ObjDataRef::Set(_) => repr_depth(v, 0),
                ObjDataRef::Range { .. } => repr_depth(v, 0),
                ObjDataRef::Function(function) => format!("<function {}>", function.name),
                ObjDataRef::Lock(_) => "<lock object>".to_string(),
                ObjDataRef::Iter(_) => "<iterator>".to_string(),
                ObjDataRef::Thread(thread) => {
                    let state = if thread.started.load(std::sync::atomic::Ordering::Acquire) {
                        "started"
                    } else {
                        "initial"
                    };
                    format!("<Thread({}, {state})>", thread.name)
                }
                ObjDataRef::Event(event) => {
                    if event.is_set() {
                        "<Event set>".to_string()
                    } else {
                        "<Event unset>".to_string()
                    }
                }
                ObjDataRef::Queue(queue) => format!("<Queue size={}>", queue.qsize()),
                // Listing the peer's items would require reading the socket;
                // render the metadata only. `maxsize` is informational for a
                // cross-process queue (see `process`).
                ObjDataRef::MpQueue(queue) => format!("<Queue maxsize={}>", queue.maxsize),
                ObjDataRef::Pool(pool) => {
                    format!("<Pool({} processes)>", pool.size())
                }
                ObjDataRef::Process(process) => {
                    let state = if process.started.load(std::sync::atomic::Ordering::Acquire) {
                        "started"
                    } else {
                        "initial"
                    };
                    format!("<Process({}, {state})>", process.name)
                }
                ObjDataRef::File(file) => {
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
                ObjDataRef::CsvReader(_) => "<csv.reader>".to_string(),
                ObjDataRef::CsvWriter(_) => "<csv.writer>".to_string(),
                // `str(bytes)` is its `repr`, like CPython.
                ObjDataRef::Bytes(bytes) => quote_bytes(bytes),
                ObjDataRef::Random(_) => "<random.Random>".to_string(),
                // `str(Path)` is the path itself.
                ObjDataRef::Path(state) => state.path.clone(),
                ObjDataRef::ArgParser(state) => match &state.prog {
                    Some(prog) => format!("ArgumentParser(prog='{prog}')"),
                    None => "ArgumentParser(prog='program')".to_string(),
                },
                ObjDataRef::Namespace(_) => repr_depth(v, 0),
                ObjDataRef::DefaultDict(_) => repr_depth(v, 0),
                ObjDataRef::Pattern(state) => format!("re.compile({})", quote_str(&state.source)),
                ObjDataRef::Match(state) => {
                    format!("<re.Match object; pattern={}>", quote_str(&state.pattern))
                }
                ObjDataRef::Socket(state) => state.render(),
                ObjDataRef::Class(state) => format!("<class '{}'>", state.name),
                ObjDataRef::Instance(state) => {
                    let class = state.class;
                    if let Some(ClassEntry::Method(hydro_ir::MethodKind::Normal, function)) =
                        class_lookup(class, "__str__")
                    {
                        let result = crate::core::builtins::call(function, 1, [v].as_ptr());
                        if !crate::exc::pending() {
                            if let Some(text) = as_str(result) {
                                return text.to_string();
                            }
                        }
                    }
                    instance_default_repr(state)
                }
                ObjDataRef::Fiber(state) => fiber_repr(state),
                ObjDataRef::ExtFunction(state) => {
                    format!("<built-in function {}>", state.name)
                }
                ObjDataRef::Capsule { name, .. } => match name {
                    Some(name) => format!("<capsule object \"{name}\">"),
                    None => "<capsule object NULL>".to_string(),
                },
                ObjDataRef::Module(state) => format!("<module '{}'>", state.name),
                ObjDataRef::StdStream(stream) => format!("<tea.{}>", stream.name()),
                ObjDataRef::Cell(value) => to_string(value),
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

// Heap objects currently being rendered by `repr_depth` on this thread.
//
// A depth limit alone is insufficient for the cyclic graphs Python permits
// through item assignment: a 24-node graph rendered to depth 64 can expand
// exponentially. The active set turns a back-edge into the conventional
// `...` marker immediately.
thread_local! {
    static REPR_ACTIVE: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Marks a heap object active for the duration of its repr. Returns `false`
/// when the object is already active (a cycle).
struct ReprGuard {
    address: usize,
    entered: bool,
}

impl ReprGuard {
    fn enter(value: i64) -> (ReprGuard, bool) {
        let address = if is_heap(value) {
            (value & !TAG_MASK) as usize
        } else {
            0
        };
        if address == 0 {
            return (
                ReprGuard {
                    address: 0,
                    entered: false,
                },
                true,
            );
        }
        let fresh = REPR_ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&address) {
                false
            } else {
                active.push(address);
                true
            }
        });
        (
            ReprGuard {
                address,
                entered: fresh,
            },
            fresh,
        )
    }
}

impl Drop for ReprGuard {
    fn drop(&mut self) {
        if self.entered {
            REPR_ACTIVE.with(|active| {
                let mut active = active.borrow_mut();
                if let Some(index) = active.iter().rposition(|&value| value == self.address) {
                    active.remove(index);
                }
            });
        }
    }
}

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
    if let Some(state) = as_module(v) {
        return format!("<module '{}'>", state.name);
    }
    if let Some(state) = as_instance(v) {
        let class = state.class;
        if let Some(ClassEntry::Method(hydro_ir::MethodKind::Normal, function)) =
            class_lookup(class, "__repr__")
        {
            let result = crate::core::builtins::call(function, 1, [v].as_ptr());
            if !crate::exc::pending() {
                if let Some(text) = as_str(result) {
                    return text.to_string();
                }
            }
        }
        return instance_default_repr(state);
    }
    if let Some(state) = as_fiber(v) {
        return fiber_repr(state);
    }
    if depth >= MAX_REPR_DEPTH {
        return "...".to_string();
    }
    let (repr_guard, fresh) = ReprGuard::enter(v);
    if !fresh {
        return "...".to_string();
    }
    let _repr_guard = repr_guard;
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
        let named = capsule_new(0x1234, Some("tea.demo".to_string()));
        assert_eq!(type_name(named), "PyCapsule");
        assert!(truthy(named));
        assert_eq!(to_string(named), "<capsule object \"tea.demo\">");
        assert_eq!(repr(named), "<capsule object \"tea.demo\">");
        assert_eq!(as_capsule(named), Some((0x1234, Some("tea.demo"))));

        let unnamed = capsule_new(0x5678, None);
        assert_eq!(repr(unnamed), "<capsule object NULL>");
        assert_eq!(as_capsule(unnamed), Some((0x5678, None)));
        assert!(as_capsule(NONE).is_none());
        assert!(as_capsule(str_from_bytes(b"x")).is_none());
    }

    #[test]
    fn module_semantics() {
        let module = module_value("errno".to_string());
        assert_eq!(type_name(module), "module");
        assert!(truthy(module));
        assert_eq!(to_string(module), "<module 'errno'>");
        assert_eq!(repr(module), "<module 'errno'>");
        assert_eq!(as_module(module).map(|state| state.name()), Some("errno"));

        // A module is not a class, instance, string or capsule.
        assert!(as_module(NONE).is_none());
        assert!(as_module(str_from_bytes(b"errno")).is_none());
        assert!(as_module(module_value("unittest".to_string())).is_some());
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

    /// Allocates a BigInt payload directly, bypassing the tagged
    /// normalization that [`bigint_new`] performs for in-range values.
    fn heap_big(text: &str) -> i64 {
        let n: BigInt = text.parse().expect("big literal");
        alloc(KIND_BIGINT, ObjData::BigInt(n))
    }

    #[test]
    fn bigint_promotion_and_accessors() {
        // Values inside the tagged range keep the immediate representation.
        assert_eq!(bigint_new(BigInt::from(5)), encode_int(5).unwrap());
        assert_eq!(
            bigint_new(BigInt::from(MAX_INT)),
            encode_int(MAX_INT).unwrap()
        );

        // One past the tagged range becomes a heap payload.
        let promoted = bigint_new(BigInt::from(MAX_INT) + BigInt::from(1));
        assert!(is_bigint(promoted));
        assert!(!is_int(promoted));
        assert_eq!(as_integer(promoted), Some(MAX_INT + 1));
        assert_eq!(
            as_bigint(promoted),
            Some(&(BigInt::from(MAX_INT) + BigInt::from(1)))
        );
        assert_eq!(type_name(promoted), "int");
        assert_eq!(type_name_of(promoted), "int");
        assert_eq!(to_string(promoted), "1152921504606846976");
        assert_eq!(repr(promoted), "1152921504606846976");
        assert!(truthy(promoted));

        // BigInts beyond `i64` only expose the arbitrary-precision accessor.
        let huge = heap_big("123456789012345678901234567890");
        assert_eq!(as_integer(huge), None);
        assert!(as_bigint(huge).is_some());
        assert_eq!(as_bigint(encode_int(1).unwrap()), None);
        assert_eq!(
            to_bigint(huge),
            Some(
                "123456789012345678901234567890"
                    .parse::<BigInt>()
                    .expect("literal")
            )
        );
        assert_eq!(to_bigint(encode_int(7).unwrap()), Some(BigInt::from(7)));
        assert_eq!(to_bigint(float_new(1.5)), None);
    }

    #[test]
    fn bigint_parsing_repr_and_truthiness() {
        let huge = heap_big("123456789012345678901234567890");
        assert_eq!(to_string(huge), "123456789012345678901234567890");
        assert_eq!(repr(huge), "123456789012345678901234567890");

        let negative = heap_big("-123456789012345678901234567890");
        assert_eq!(to_string(negative), "-123456789012345678901234567890");
        assert_eq!(repr(negative), "-123456789012345678901234567890");
        assert!(truthy(negative));

        // Zero is falsy even as a heap payload; small payloads still decode.
        assert!(!truthy(heap_big("0")));
        assert_eq!(as_integer(heap_big("0")), Some(0));
        assert_eq!(as_integer(heap_big("-42")), Some(-42));

        // Printing a BigInt through the generic renderer stays decimal.
        assert_eq!(
            to_string(list_new(vec![huge])),
            "[123456789012345678901234567890]"
        );
    }

    #[test]
    fn bigint_hashing_matches_equal_tagged_ints() {
        // Equal values hash equally whether the payload is tagged or heap.
        for text in ["0", "7", "-42", "1152921504606846976"] {
            let heap = heap_big(text);
            let normalized = bigint_new(text.parse::<BigInt>().expect("literal"));
            assert_eq!(
                crate::container::key_hash(heap),
                crate::container::key_hash(normalized),
                "hash mismatch for {text}"
            );
        }
        // Independently allocated equal big values share one hash.
        let a = heap_big("123456789012345678901234567890");
        let b = heap_big("123456789012345678901234567890");
        assert_eq!(crate::container::key_hash(a), crate::container::key_hash(b));
    }

    #[test]
    fn bigint_exact_float_comparison() {
        let huge = heap_big("1267650600228229401496703205376"); // 2^100
        let n = as_bigint(huge).expect("bigint payload");
        assert_eq!(bigint_cmp_f64(n, 2f64.powi(100)), Some(Ordering::Equal));
        assert_eq!(bigint_cmp_f64(n, 2f64.powi(99)), Some(Ordering::Greater));
        assert_eq!(bigint_cmp_f64(n, 2f64.powi(101)), Some(Ordering::Less));
        assert_eq!(bigint_cmp_f64(n, f64::NAN), None);
        assert_eq!(bigint_cmp_f64(n, f64::INFINITY), Some(Ordering::Less));
        assert_eq!(
            bigint_cmp_f64(n, f64::NEG_INFINITY),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn bigint_literal_parsing_from_abi_buffer() {
        let text = b"987654321098765432109876543210";
        let parsed = bigint_from_ptr(text.as_ptr(), text.len());
        assert!(is_bigint(parsed));
        assert_eq!(to_string(parsed), "987654321098765432109876543210");

        // Tagged-range literals normalize to immediates.
        assert_eq!(bigint_from_ptr(b"42".as_ptr(), 2), encode_int(42).unwrap());
        let min = b"-1152921504606846976";
        assert_eq!(bigint_from_ptr(min.as_ptr(), min.len()), MIN_INT << 3);

        // Malformed text raises ValueError through the pending exception.
        crate::exc::init();
        let bad = b"12x";
        assert_eq!(bigint_from_ptr(bad.as_ptr(), bad.len()), NONE);
        assert_eq!(crate::exc::pending_tag(), crate::exc::VALUE_ERROR as i64);
        assert_eq!(
            to_string(crate::exc::pending_value()),
            "invalid literal for int() with base 10: '12x'"
        );
        crate::exc::clear();
        assert_eq!(bigint_from_ptr(std::ptr::null(), 0), NONE);
        assert_eq!(crate::exc::pending_tag(), crate::exc::VALUE_ERROR as i64);
    }
}
