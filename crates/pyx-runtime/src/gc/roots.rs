//! Precise shadow-stack roots (P2-06/P2-07).
//!
//! Generated code no longer relies on conservative stack scanning for its
//! locals. Before a function touches the heap it registers the slots that hold
//! its live tagged values:
//!
//! * [`frame_push`]/[`frame_pop`] bracket one function frame. The whole slot
//!   array is filled with tagged `None` first, so a collection between the
//!   push and the first store can never see an uninitialised word. A frame
//!   can shrink the scanned prefix with [`frame_used`].
//! * [`scope_push`]/[`scope_pop`] bracket a caller-owned array (`argv`,
//!   packed call arguments, a stack buffer) whose values the ABI passes in.
//!
//! The mutator fast path is a pair of thread-local vectors; it allocates no
//! PyX objects and never takes the heap lock. A frame or range is therefore
//! always registered on the thread that owns the referenced slots, and is
//! only ever popped on that thread.
//!
//! # Collecting
//!
//! A collection cannot walk another thread's thread-local vectors, so the
//! mutator copies them into a process-wide registry at a safepoint with
//! [`publish_current`]. [`snapshot_all`] hands the collector one consistent
//! copy, keyed by a per-thread token; [`snapshot_current`] serves the
//! single-threaded (thread-local heap) collector. Publishing allocates only
//! Rust-side vectors and is safe while the publishing thread is parked: the
//! collector must still only dereference a snapshot's slots while the owning
//! thread is stopped (the module cannot enforce that; the collector's
//! stop-the-world protocol does).
//!
//! # Fiber switches
//!
//! A fiber maintains two independent root sets: its own and the resumer's
//! (see the fiber module). [`take_current`] removes the active set from the
//! thread-local registry and [`install`] swaps another set in, returning the
//! previous one. This is a pure vector move: no snapshot or registry work
//! happens on the context-switch path. The saved roots stay in the fiber
//! object (`FiberState`) and are exposed through [`SavedRoots::frames`]/
//! [`SavedRoots::ranges`] so the collector can mark them while the other
//! context runs.
//!
//! # Token discipline
//!
//! Every push returns a token from a per-thread monotonic counter (`0` is
//! never a valid token). Pops and updates must be strictly LIFO; a mismatch
//! is a `debug_assert!` failure, and in release the call is ignored so a
//! malformed token can never corrupt the registry or the collector's view.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// One registered shadow frame.
///
/// * `base .. base + capacity` is the caller's slot array; [`frame_push`]
///   initialises every slot to tagged `None`.
/// * `used` is the scanned prefix (`used <= capacity`); [`frame_used`] may
///   narrow it while the frame is live and widen it again within the
///   reservation.
/// * `low`/`high` are the half-open byte range of the slot array
///   (`low = base as usize`, `high = low + capacity * 8`). They let the
///   collector and debuggers validate a frame without recomputing the extent.
#[derive(Clone, Copy, Debug)]
pub struct RootFrame {
    /// First slot. Valid for `capacity` `i64`s until the matching pop.
    pub base: *mut i64,
    /// Number of slots reserved (and `None`-filled) by the push.
    pub capacity: usize,
    /// Number of slots the collector must scan; always `<= capacity`.
    pub used: usize,
    /// `base` as a byte address.
    pub low: usize,
    /// One past the last slot byte (`low + capacity * 8`).
    pub high: usize,
}

/// One registered array of tagged values owned by the caller (`argv`, a
/// packed argument buffer, a stack array).
///
/// Unlike a [`RootFrame`], the values are already initialised when the range
/// is pushed, so they are neither filled nor truncated.
#[derive(Clone, Copy, Debug)]
pub struct RootRange {
    /// First live value; valid for `count` `i64`s until the matching pop.
    pub base: *const i64,
    /// Number of values in the array.
    pub count: usize,
}

/// The active root set of one thread, in push order.
///
/// Produced by [`snapshot_current`]/[`publish_current`] and owned by
/// [`SavedRoots`]; a snapshot is a descriptor copy, not a copy of the values.
#[derive(Clone, Debug, Default)]
pub struct PublishedRoots {
    frames: Vec<RootFrame>,
    ranges: Vec<RootRange>,
}

// SAFETY: a snapshot contains descriptors — raw addresses of shadow slots
// owned by one mutator thread — plus plain lengths. Moving the descriptors to
// the collector's thread does not move the slots. The collector contract
// guarantees that the referenced memory stays live and is only dereferenced
// while the owning thread is parked, exactly like the existing conservative
// root snapshot.
unsafe impl Send for PublishedRoots {}

/// A thread's frame/range set detached for a context switch.
///
/// Unlike [`PublishedRoots`], a `SavedRoots` also carries the internal token
/// stacks: installing it restores the exact LIFO order, so the frames of the
/// suspended context can be popped with their original tokens when it
/// resumes. `Drop`/`clear` only drop the descriptors; no slot is ever
/// dereferenced or scanned.
#[derive(Debug, Default)]
pub struct SavedRoots {
    frames: Vec<RootFrame>,
    frame_tokens: Vec<usize>,
    ranges: Vec<RootRange>,
    range_tokens: Vec<usize>,
}

/// Thread-local mutator-side registry.
struct ThreadRoots {
    frames: Vec<RootFrame>,
    /// Token of the matching entry in `frames`.
    frame_tokens: Vec<usize>,
    ranges: Vec<RootRange>,
    /// Token of the matching entry in `ranges`.
    range_tokens: Vec<usize>,
    /// Per-thread monotonic push serial; `0` means "never pushed".
    next_token: usize,
}

impl ThreadRoots {
    const fn new() -> ThreadRoots {
        ThreadRoots {
            frames: Vec::new(),
            frame_tokens: Vec::new(),
            ranges: Vec::new(),
            range_tokens: Vec::new(),
            next_token: 0,
        }
    }

    fn fresh_token(&mut self) -> usize {
        // `usize` wrap is unreachable in practice; skipping `0` keeps the
        // "invalid token" sentinel unambiguous.
        self.next_token = self.next_token.wrapping_add(1);
        if self.next_token == 0 {
            self.next_token = 1;
        }
        self.next_token
    }
}

thread_local! {
    /// Fast path: frames and ranges registered by the running context.
    static ROOTS: RefCell<ThreadRoots> = const { RefCell::new(ThreadRoots::new()) };
}

/// Source of the per-thread registry keys.
///
/// A thread token is unique for the lifetime of the OS thread and independent
/// of the Python-visible thread id (which every never-spawned thread claims as
/// `1`).
static NEXT_THREAD_TOKEN: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static THREAD_TOKEN: Cell<u64> = const { Cell::new(0) };
}

/// Returns the calling thread's registry token, allocating it on first use.
pub fn current_thread_token() -> u64 {
    THREAD_TOKEN.with(|cell| {
        let mut token = cell.get();
        if token == 0 {
            token = NEXT_THREAD_TOKEN.fetch_add(1, Ordering::Relaxed);
            cell.set(token);
        }
        token
    })
}

/// Published copy of every thread's active root set.
static PUBLISHED: LazyLock<Mutex<HashMap<u64, PublishedRoots>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_published() -> MutexGuard<'static, HashMap<u64, PublishedRoots>> {
    PUBLISHED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Registers a shadow frame covering `capacity` slots at `base` and returns
/// its token.
///
/// Every slot is first overwritten with tagged `None`, so the frame is safe
/// to scan from the moment it is visible to the collector. The capacity must
/// cover all locals, temporaries and return-value slots of the frame; the
/// scanned prefix can later be narrowed with [`frame_used`].
///
/// Allocates no PyX objects and takes no heap lock.
#[allow(clippy::not_unsafe_ptr_arg_deref)] // ABI contract: generated code owns `base..base+capacity` slots.
pub fn frame_push(base: *mut i64, capacity: usize) -> usize {
    debug_assert!(
        !base.is_null() || capacity == 0,
        "pyx_gc_frame_push: NULL slot array for a non-empty frame"
    );
    if base.is_null() && capacity != 0 {
        // Compiled-code invariant violated: never expose a dangling frame.
        return 0;
    }
    if capacity != 0 {
        // SAFETY: the codegen contract passes a live writable array of
        // exactly `capacity` tagged slots; it is filled before registration.
        unsafe {
            for index in 0..capacity {
                base.add(index).write(crate::value::NONE);
            }
        }
    }
    ROOTS.with(|cell| {
        let mut roots = cell.borrow_mut();
        let token = roots.fresh_token();
        roots.frames.push(RootFrame {
            base,
            capacity,
            used: capacity,
            low: base as usize,
            high: base as usize + capacity * std::mem::size_of::<i64>(),
        });
        roots.frame_tokens.push(token);
        debug_assert_eq!(roots.frames.len(), roots.frame_tokens.len());
        token
    })
}

/// Unregisters the innermost frame.
///
/// `token` must be the token returned by the matching [`frame_push`]; any
/// other value trips a debug assertion and is ignored in release builds.
pub fn frame_pop(token: usize) {
    ROOTS.with(|cell| {
        let mut roots = cell.borrow_mut();
        match roots.frame_tokens.last().copied() {
            Some(top) if top == token => {
                roots.frames.pop();
                roots.frame_tokens.pop();
            }
            _ => debug_assert!(
                false,
                "pyx_gc_frame_pop: token {token} is not the innermost frame"
            ),
        }
    });
}

/// Sets the scanned prefix of the frame identified by `token`.
///
/// The prefix may be narrowed so a temporary that is dead for the rest of the
/// frame stops retaining garbage, and widened again within the reservation.
/// `used` is clamped to the frame's capacity; the frame must still be the
/// innermost one.
pub fn frame_used(token: usize, used: usize) {
    ROOTS.with(|cell| {
        let mut roots = cell.borrow_mut();
        let top = roots.frame_tokens.last().copied();
        match (roots.frames.last_mut(), top) {
            (Some(frame), Some(top)) if top == token => {
                debug_assert!(
                    used <= frame.capacity,
                    "pyx_gc_frame_used: {used} exceeds capacity {}",
                    frame.capacity
                );
                frame.used = used.min(frame.capacity);
            }
            _ => debug_assert!(
                false,
                "pyx_gc_frame_used: token {token} is not the innermost frame"
            ),
        }
    });
}

/// Registers an already initialised array of `count` tagged values and
/// returns its token.
///
/// Used for `argv`/argument buffers, where the ABI receives a pointer to a
/// caller-owned array instead of a compiled frame. Allocates no PyX objects
/// and takes no heap lock.
pub fn scope_push(base: *const i64, count: usize) -> usize {
    debug_assert!(
        !base.is_null() || count == 0,
        "pyx_gc_scope_push: NULL value array for a non-empty range"
    );
    if base.is_null() && count != 0 {
        return 0;
    }
    ROOTS.with(|cell| {
        let mut roots = cell.borrow_mut();
        let token = roots.fresh_token();
        roots.ranges.push(RootRange { base, count });
        roots.range_tokens.push(token);
        debug_assert_eq!(roots.ranges.len(), roots.range_tokens.len());
        token
    })
}

/// Unregisters the innermost scope range; LIFO, like [`frame_pop`].
pub fn scope_pop(token: usize) {
    ROOTS.with(|cell| {
        let mut roots = cell.borrow_mut();
        match roots.range_tokens.last().copied() {
            Some(top) if top == token => {
                roots.ranges.pop();
                roots.range_tokens.pop();
            }
            _ => debug_assert!(
                false,
                "pyx_gc_scope_pop: token {token} is not the innermost scope"
            ),
        }
    });
}

/// Copies the calling thread's active roots into the process-wide registry.
///
/// Called at a safepoint before parking, so the collector sees the exact
/// roots of the stopped thread. Replaces any earlier publication of this
/// thread. Allocates only Rust vectors; takes no PyX heap lock and no fiber
/// lock, and is safe while the thread is parked.
pub fn publish_current() {
    let (frames, ranges) = ROOTS.with(|cell| {
        let roots = cell.borrow();
        (roots.frames.clone(), roots.ranges.clone())
    });
    let token = current_thread_token();
    lock_published().insert(token, PublishedRoots { frames, ranges });
}

/// Returns a consistent copy of every published root set, keyed by thread
/// token and sorted by token.
///
/// The collector takes this once per collection after stopping the world.
pub fn snapshot_all() -> Vec<(u64, PublishedRoots)> {
    let registry = lock_published();
    let mut snapshots: Vec<(u64, PublishedRoots)> = registry
        .iter()
        .map(|(&token, roots)| (token, roots.clone()))
        .collect();
    snapshots.sort_unstable_by_key(|(token, _)| *token);
    snapshots
}

/// Returns the last published snapshot of `token`, if any.
pub fn snapshot_thread(token: u64) -> Option<PublishedRoots> {
    lock_published().get(&token).cloned()
}

/// Returns a copy of the calling thread's currently active roots.
///
/// Used by the local-heap collector, which runs on the mutator thread and
/// therefore needs no publication.
pub fn snapshot_current() -> PublishedRoots {
    ROOTS.with(|cell| {
        let roots = cell.borrow();
        PublishedRoots {
            frames: roots.frames.clone(),
            ranges: roots.ranges.clone(),
        }
    })
}

/// Drops this thread's active roots and its published snapshot.
///
/// Called when a thread exits or after a `fork`, where the child no longer
/// matches the forked address space.
pub fn clear_current() {
    ROOTS.with(|cell| {
        let mut roots = cell.borrow_mut();
        roots.frames.clear();
        roots.frame_tokens.clear();
        roots.ranges.clear();
        roots.range_tokens.clear();
    });
    let token = current_thread_token();
    lock_published().remove(&token);
}

/// Removes the published snapshot of another (exited) thread.
pub fn clear_thread(token: u64) {
    lock_published().remove(&token);
}

/// Detaches the calling thread's active roots for a fiber context switch.
///
/// The returned set can be installed again with [`install`]; the registry
/// keeps the caller's publication untouched (the collector publishes anew
/// when the context reaches a safepoint).
pub fn take_current() -> SavedRoots {
    ROOTS.with(|cell| {
        let mut roots = cell.borrow_mut();
        SavedRoots {
            frames: std::mem::take(&mut roots.frames),
            frame_tokens: std::mem::take(&mut roots.frame_tokens),
            ranges: std::mem::take(&mut roots.ranges),
            range_tokens: std::mem::take(&mut roots.range_tokens),
        }
    })
}

/// Installs `saved` as the calling thread's active root set and returns the
/// previously active set.
///
/// The tokens saved with `saved` are restored verbatim, so the context can
/// keep popping its frames with the tokens it already holds (the thread's
/// token counter stays monotonic and is *not* rolled back).
pub fn install(saved: SavedRoots) -> SavedRoots {
    let SavedRoots {
        frames,
        frame_tokens,
        ranges,
        range_tokens,
    } = saved;
    ROOTS.with(|cell| {
        let mut roots = cell.borrow_mut();
        let previous = SavedRoots {
            frames: std::mem::replace(&mut roots.frames, frames),
            frame_tokens: std::mem::replace(&mut roots.frame_tokens, frame_tokens),
            ranges: std::mem::replace(&mut roots.ranges, ranges),
            range_tokens: std::mem::replace(&mut roots.range_tokens, range_tokens),
        };
        debug_assert_eq!(roots.frames.len(), roots.frame_tokens.len());
        debug_assert_eq!(roots.ranges.len(), roots.range_tokens.len());
        previous
    })
}

/// Marks every live value of `frames` and `ranges` through `mark`.
///
/// Only the `used` prefix of each frame is scanned. The caller guarantees
/// that the referenced slots are still live (the owning thread is parked or
/// the context is suspended); `mark` performs the tagged-value validation.
pub fn mark_precise(frames: &[RootFrame], ranges: &[RootRange], mut mark: impl FnMut(i64)) {
    for frame in frames {
        let used = frame.used.min(frame.capacity);
        for index in 0..used {
            // SAFETY: `frame_push` registered `capacity` initialised slots at
            // `base`; `used <= capacity` and a frame is popped only after it
            // stops being scanned.
            mark(unsafe { *frame.base.add(index) });
        }
    }
    for range in ranges {
        for index in 0..range.count {
            // SAFETY: the caller registered a live array of `count` values.
            mark(unsafe { *range.base.add(index) });
        }
    }
}

impl PublishedRoots {
    /// Registered frames, outermost first.
    pub fn frames(&self) -> &[RootFrame] {
        &self.frames
    }

    /// Registered scope ranges, in push order.
    pub fn ranges(&self) -> &[RootRange] {
        &self.ranges
    }

    /// Whether the owning thread has no roots at all.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.ranges.is_empty()
    }

    /// Marks all live values through `mark`; see [`mark_precise`].
    pub fn mark(&self, mark: impl FnMut(i64)) {
        mark_precise(&self.frames, &self.ranges, mark);
    }
}

impl SavedRoots {
    /// Registered frames, outermost first.
    pub fn frames(&self) -> &[RootFrame] {
        &self.frames
    }

    /// Registered scope ranges, in push order.
    pub fn ranges(&self) -> &[RootRange] {
        &self.ranges
    }

    /// Whether the saved context has no roots at all.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.ranges.is_empty()
    }

    /// Marks all live values through `mark`; see [`mark_precise`].
    pub fn mark(&self, mark: impl FnMut(i64)) {
        mark_precise(&self.frames, &self.ranges, mark);
    }

    /// Drops the descriptors without touching any referenced slot.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.frame_tokens.clear();
        self.ranges.clear();
        self.range_tokens.clear();
        // Keep the vectors' capacity: a fiber reuses them across resumes.
    }
}
