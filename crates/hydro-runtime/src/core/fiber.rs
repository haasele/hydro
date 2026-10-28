//! Stackful fibers backing lazy generators and coroutines.
//!
//! A [`FiberState`] owns a private 128 KiB stack and a register-switch
//! context. On x86_64 the switch is a minimal callee-saved-register context
//! switch (`ret`-based, no syscalls); other architectures fall back to libc
//! `ucontext`. `resume` switches from the caller to the fiber; `hydro_yield`
//! and every other suspension point switch back. Nothing runs when a fiber is
//! created: the generated `__hydro_wrap_N` trampoline is entered on the first
//! resume, which makes generators lazy and makes calling an `async def`
//! function create a coroutine object without running its body.
//!
//! # GC interaction
//!
//! While a fiber runs, the thread's stack pointer lives on the fiber stack, so
//! [`crate::heap::push_stack_bounds`] redirects the collector's stack bounds
//! to the fiber range.
//!
//! The fiber stack, `ctx` and `caller_ctx` live for the whole object
//! lifetime and are registered as conservative root ranges exactly once in
//! [`FiberState::new`]; `Drop` unregisters them. The contexts contain the
//! callee-saved register words, which may hold tagged values while a fiber is
//! suspended. Scanning a running fiber's stack is conservative but safe, so a
//! resume performs no registry work at all.
//!
//! The *native* caller frames of a resume (the only suspended side that is
//! not on a fiber stack) are published through
//! [`crate::heap::set_aux_stack`] while the fiber runs.
//!
//! Precise shadow-stack roots follow the switch: [`resume_raw`] detaches the
//! resumer's root set into [`FiberState::caller_roots`] and installs the
//! fiber's own set ([`FiberState::saved_roots`]); [`suspend_switch`] and
//! [`finish_switch`] do the reverse. Both sets stay inside the fiber object,
//! so the collector can still scan them (`FiberState::trace` marks them) while
//! the other context runs.

use std::alloc::{self, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::exc::{self, ATTRIBUTE_ERROR, RUNTIME_ERROR, STOP_ITERATION, TYPE_ERROR};
use crate::gc::roots::{self, SavedRoots};
use crate::value::{self, NONE};

// -------------------------------------------------------------------------
// Context switch
// -------------------------------------------------------------------------

/// Callee-saved register frame used by the x86_64 context switch.
///
/// Offsets are fixed by the assembly below: `rsp` at 0, then `rbx`, `rbp`,
/// `r12`, `r13`, `r14`, `r15` at 8, 16, 24, 32, 40 and 48.
#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[allow(dead_code)] // fields are loaded/stored by the assembly only
struct Context {
    rsp: usize,
    rbx: usize,
    rbp: usize,
    r12: usize,
    r13: usize,
    r14: usize,
    r15: usize,
}

#[cfg(target_arch = "x86_64")]
const _: () = {
    assert!(std::mem::size_of::<Context>() == 56);
    assert!(std::mem::offset_of!(Context, rsp) == 0);
    assert!(std::mem::offset_of!(Context, rbx) == 8);
    assert!(std::mem::offset_of!(Context, rbp) == 16);
    assert!(std::mem::offset_of!(Context, r12) == 24);
    assert!(std::mem::offset_of!(Context, r13) == 32);
    assert!(std::mem::offset_of!(Context, r14) == 40);
    assert!(std::mem::offset_of!(Context, r15) == 48);
};

// `hydro_context_switch(from, to)` saves the SysV callee-saved registers and
// the stack pointer into `from` and resumes `to`.
//
// `call hydro_context_switch` pushes the continuation address, so the saved
// `rsp` points at it and `ret` in the target context resumes exactly where
// the corresponding call happened. Only callee-saved registers are touched:
// caller-saved registers are already spilled by the compiler around the
// call, and `mxcsr`/the x87 control word are never modified.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl hydro_context_switch",
    "hydro_context_switch:",
    "mov [rdi + 0], rsp",
    "mov [rdi + 8], rbx",
    "mov [rdi + 16], rbp",
    "mov [rdi + 24], r12",
    "mov [rdi + 32], r13",
    "mov [rdi + 40], r14",
    "mov [rdi + 48], r15",
    "mov rsp, [rsi + 0]",
    "mov rbx, [rsi + 8]",
    "mov rbp, [rsi + 16]",
    "mov r12, [rsi + 24]",
    "mov r13, [rsi + 32]",
    "mov r14, [rsi + 40]",
    "mov r15, [rsi + 48]",
    "ret",
);

#[cfg(target_arch = "x86_64")]
extern "C" {
    fn hydro_context_switch(from: *mut Context, to: *const Context);
}

/// Per-fiber context storage. On x86_64 an inline register frame; elsewhere
/// the boxed libc context (boxed because `getcontext` stores internal
/// pointers, e.g. the FPU state, into the structure itself).
#[cfg(target_arch = "x86_64")]
type ArchContext = Context;
#[cfg(not(target_arch = "x86_64"))]
type ArchContext = Box<libc::ucontext_t>;

/// Builds the initial and caller contexts for a fresh fiber stack.
#[cfg(target_arch = "x86_64")]
fn make_arch_contexts(stack: &AlignedStack) -> Option<(ArchContext, ArchContext)> {
    // A 16-byte-aligned word holds the entry address; `ret` pops it, leaving
    // `rsp % 16 == 8` at the trampoline, exactly as after a normal `call`.
    let top = stack.high() & !15;
    let slot = top - 16;
    let entry: extern "C" fn() = fiber_trampoline;
    // SAFETY: `slot` lies inside the freshly allocated fiber stack.
    unsafe { (slot as *mut usize).write(entry as usize) };
    Some((
        Context {
            rsp: slot,
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        },
        Context {
            rsp: 0,
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        },
    ))
}

#[cfg(not(target_arch = "x86_64"))]
fn make_arch_contexts(stack: &AlignedStack) -> Option<(ArchContext, ArchContext)> {
    // SAFETY: a zeroed `ucontext_t` is a valid starting point for
    // `getcontext`.
    let mut ctx = Box::new(unsafe { std::mem::zeroed::<libc::ucontext_t>() });
    // SAFETY: `ctx` is a valid, writable context at a stable address.
    if unsafe { libc::getcontext(&mut *ctx) } != 0 {
        return None;
    }
    ctx.uc_stack.ss_sp = stack.low() as *mut libc::c_void;
    ctx.uc_stack.ss_size = stack.len;
    ctx.uc_stack.ss_flags = 0;
    ctx.uc_link = std::ptr::null_mut();
    // SAFETY: the stack is installed and the trampoline reads the active
    // fiber from the thread-local set by `resume`.
    unsafe { libc::makecontext(&mut *ctx, fiber_trampoline, 0) };
    Some((
        ctx,
        Box::new(unsafe { std::mem::zeroed::<libc::ucontext_t>() }),
    ))
}

/// Saves the caller's continuation into `fiber.caller_ctx` and enters
/// `fiber.ctx`.
#[cfg(target_arch = "x86_64")]
unsafe fn switch_to_fiber(fiber: *mut FiberState) {
    // SAFETY: both contexts are live fields of `fiber`.
    unsafe {
        hydro_context_switch(
            std::ptr::addr_of_mut!((*fiber).caller_ctx),
            std::ptr::addr_of!((*fiber).ctx),
        );
    }
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn switch_to_fiber(fiber: *mut FiberState) {
    // SAFETY: both contexts are live fields of `fiber`.
    unsafe {
        libc::swapcontext(
            &mut *(*fiber).caller_ctx as *mut libc::ucontext_t,
            &*(*fiber).ctx as *const libc::ucontext_t,
        );
    }
}

/// Saves the fiber's continuation into `fiber.ctx` and resumes
/// `fiber.caller_ctx`.
#[cfg(target_arch = "x86_64")]
unsafe fn switch_from_fiber(fiber: *mut FiberState) {
    // SAFETY: both contexts are live fields of `fiber`.
    unsafe {
        hydro_context_switch(
            std::ptr::addr_of_mut!((*fiber).ctx),
            std::ptr::addr_of!((*fiber).caller_ctx),
        );
    }
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn switch_from_fiber(fiber: *mut FiberState) {
    // SAFETY: both contexts are live fields of `fiber`.
    unsafe {
        libc::swapcontext(
            &mut *(*fiber).ctx as *mut libc::ucontext_t,
            &*(*fiber).caller_ctx as *const libc::ucontext_t,
        );
    }
}

/// Byte ranges of the two contexts. The callee-saved register words may hold
/// tagged values while a fiber is suspended, so both ranges are conservative
/// GC roots for the fiber's whole lifetime.
#[cfg(target_arch = "x86_64")]
fn context_root_ranges(state: &FiberState) -> [(usize, usize); 2] {
    let ctx_lo = std::ptr::addr_of!(state.ctx) as usize;
    let caller_lo = std::ptr::addr_of!(state.caller_ctx) as usize;
    let size = std::mem::size_of::<Context>();
    [(ctx_lo, ctx_lo + size), (caller_lo, caller_lo + size)]
}

#[cfg(not(target_arch = "x86_64"))]
fn context_root_ranges(state: &FiberState) -> [(usize, usize); 2] {
    let ctx_lo = (&*state.ctx) as *const libc::ucontext_t as usize;
    let caller_lo = (&*state.caller_ctx) as *const libc::ucontext_t as usize;
    let size = std::mem::size_of::<libc::ucontext_t>();
    [(ctx_lo, ctx_lo + size), (caller_lo, caller_lo + size)]
}

/// Conservative lower bound for the native caller's live stack while a fiber
/// runs.
///
/// Reading `rsp` inside this (tiny) function yields an address below every
/// live frame of the resumer, so scanning `[sp, native_high)` cannot miss a
/// spill slot in the resume frame itself.
#[cfg(target_arch = "x86_64")]
fn caller_stack_low() -> usize {
    let sp: usize;
    // SAFETY: reads the stack pointer; touches no memory and preserves flags.
    unsafe {
        core::arch::asm!(
            "mov {}, rsp",
            out(reg) sp,
            options(nomem, nostack, preserves_flags)
        );
    }
    sp
}

#[cfg(target_arch = "aarch64")]
fn caller_stack_low() -> usize {
    let sp: usize;
    // SAFETY: reads the stack pointer; touches no memory and preserves flags.
    unsafe {
        core::arch::asm!(
            "mov {}, sp",
            out(reg) sp,
            options(nomem, nostack, preserves_flags)
        );
    }
    sp
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn caller_stack_low() -> usize {
    let marker: usize = 0;
    &marker as *const usize as usize
}

/// Size of every fiber stack. Deep Python recursion inside a fiber is bounded
/// by this; it matches the runtime's remaining recursion headroom assumption.
pub(crate) const FIBER_STACK_SIZE: usize = 1024 * 1024;

/// What a fiber object represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FiberKind {
    /// `def` with `yield`: iterated with `hydro_iter_new`/`hydro_iter_next`.
    Generator,
    /// `async def`: driven by `hydro_await` or `asyncio.run`.
    Coroutine,
    /// `async def` with `yield`: driven by `__anext__`/`asend`.
    AsyncGenerator,
}

/// A suspension request that the driver has to service before the fiber can
/// make progress. Requests are runtime-internal: they are never exposed as
/// Python values.
pub(crate) enum WaitRequest {
    /// `asyncio.sleep(seconds)` inside a fiber.
    Sleep(Duration),
    /// `asyncio.gather(...)` inside a fiber: wait until every listed task is
    /// done (or fails).
    Join(Vec<i64>),
}

/// Private fiber stack. Freed when the fiber object is collected; a running
/// fiber is always reachable from its caller's registered stack, so it is
/// never freed underneath itself.
struct AlignedStack {
    ptr: *mut u8,
    len: usize,
}

impl AlignedStack {
    fn new(size: usize) -> Option<AlignedStack> {
        let layout = Layout::from_size_align(size, 16).ok()?;
        // SAFETY: the layout has a non-zero size.
        let ptr = unsafe { alloc::alloc(layout) };
        if ptr.is_null() {
            return None;
        }
        Some(AlignedStack { ptr, len: size })
    }

    fn low(&self) -> usize {
        self.ptr as usize
    }

    fn high(&self) -> usize {
        self.ptr as usize + self.len
    }
}

impl Drop for AlignedStack {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            let Ok(layout) = Layout::from_size_align(self.len, 16) else {
                return;
            };
            // SAFETY: the pointer/layout pair is exactly what `new` allocated
            // and `Drop` runs once.
            unsafe { alloc::dealloc(self.ptr, layout) };
        }
    }
}

/// Mutable state of one generator or coroutine object.
pub(crate) struct FiberState {
    pub(crate) kind: FiberKind,
    /// Python-visible function name used by `repr`.
    pub(crate) name: String,
    stack: AlignedStack,
    /// Register-switch context. Its address is stable because `FiberState`
    /// lives in a `Box`; the collector scans its callee-saved words.
    ctx: ArchContext,
    /// Continuation of the resumer; scanned for the same reason.
    caller_ctx: ArchContext,
    /// Address of the generated `__hydro_wrap_N` trampoline.
    entry: usize,
    /// Captures followed by the call arguments (the wrapper's `argv`).
    pub(crate) args: Vec<i64>,
    pub(crate) done: bool,
    pub(crate) running: bool,
    /// Value passed to `hydro_yield` at the last suspension.
    pub(crate) yielded: i64,
    /// Return value recorded when the body finished.
    pub(crate) result: i64,
    /// Pending exception captured from the body (`NONE` if none).
    pub(crate) error: i64,
    /// The body has been resumed at least once.
    pub(crate) started: bool,
    /// Value passed to the current (or last) resume; `hydro_yield` returns it
    /// to the generator body.
    pub(crate) sent: i64,
    /// Exception injected by `throw()`/`close()` and raised at the suspended
    /// `yield` when the fiber is resumed.
    pub(crate) injected: Option<i64>,
    /// Exception queued by task cancellation and raised when the fiber next
    /// resumes from a suspension point.
    pub(crate) pending_raise: Option<i64>,
    /// Fiber this one is currently awaiting (`NONE` when none). Keeps the
    /// awaited object reachable while the awning fiber is suspended.
    pub(crate) awaiting: i64,
    /// The task has been handed to the asyncio driver at least once.
    pub(crate) scheduled: bool,
    /// The fiber is a task owned by the asyncio loop (`create_task`), so
    /// awaiting it must go through a join instead of resuming it here.
    pub(crate) owned: bool,
    /// `Task.cancel()` was requested; the CancelledError is delivered at the
    /// next resume.
    pub(crate) cancel_requested: bool,
    /// The task finished by propagating `CancelledError`.
    pub(crate) cancelled: bool,
    /// Number of active `await` frames on this fiber's stack, saved across
    /// context switches so nested awaits are bounded per chain.
    pub(crate) await_depth: usize,
    /// `close()` is driving the fiber; a `yield` that resumes afterwards is
    /// an ignored `GeneratorExit`.
    pub(crate) closing: bool,
    /// Outstanding suspension request, if any.
    pub(crate) request: Option<WaitRequest>,
    /// Lifetime conservative root range: the private stack.
    stack_root_lo: usize,
    stack_root_hi: usize,
    /// Lifetime conservative root ranges: the two register contexts.
    ctx_root_lo: usize,
    ctx_root_hi: usize,
    caller_ctx_root_lo: usize,
    caller_ctx_root_hi: usize,
    /// Monotonic completion order, used to report the first failing gather
    /// task.
    pub(crate) finish_seq: u64,
    /// Precise shadow-stack roots of this fiber while it is suspended.
    ///
    /// Filled by [`suspend_switch`] and installed by [`resume_raw`]; the
    /// collector can scan them through [`SavedRoots::frames`]/
    /// [`SavedRoots::ranges`] while another context runs, and
    /// [`FiberState::trace`]
    /// marks them for the conservative collector.
    pub(crate) saved_roots: SavedRoots,
    /// Precise roots of the resumer while this fiber runs.
    ///
    /// Captured by [`resume_raw`] before the switch and installed again by
    /// [`suspend_switch`]/[`finish_switch`], so the caller's frames stay
    /// scannable while its native stack is inactive.
    pub(crate) caller_roots: SavedRoots,
    /// Frame-introspection records of this fiber while it is suspended.
    ///
    /// Python frames are thread state, not process state; a suspended fiber's
    /// activations must not appear on the resumer's stack. The records are
    /// swapped exactly like [`FiberState::saved_roots`], so `f_back` chains and
    /// `sys._getframe` depths stay within the context that is running.
    pub(crate) saved_frames: Vec<Arc<exc::FrameData>>,
    /// Frame records of the resumer while this fiber runs.
    pub(crate) caller_frames: Vec<Arc<exc::FrameData>>,
}

impl FiberState {
    /// Creates a suspended fiber. The body runs on the first resume.
    pub(crate) fn new(
        kind: FiberKind,
        entry: usize,
        name: String,
        args: Vec<i64>,
    ) -> Option<Box<FiberState>> {
        let stack = AlignedStack::new(FIBER_STACK_SIZE)?;
        let (ctx, caller_ctx) = make_arch_contexts(&stack)?;
        let stack_lo = stack.low();
        let stack_hi = stack.high();
        let mut state = Box::new(FiberState {
            kind,
            name,
            stack,
            ctx,
            caller_ctx,
            entry,
            args,
            done: false,
            running: false,
            yielded: NONE,
            result: NONE,
            error: NONE,
            started: false,
            sent: NONE,
            injected: None,
            pending_raise: None,
            awaiting: NONE,
            scheduled: false,
            owned: false,
            cancel_requested: false,
            cancelled: false,
            await_depth: 0,
            closing: false,
            request: None,
            stack_root_lo: stack_lo,
            stack_root_hi: stack_hi,
            ctx_root_lo: 0,
            ctx_root_hi: 0,
            caller_ctx_root_lo: 0,
            caller_ctx_root_hi: 0,
            finish_seq: 0,
            saved_roots: SavedRoots::default(),
            caller_roots: SavedRoots::default(),
            saved_frames: Vec::new(),
            caller_frames: Vec::new(),
        });
        // The three ranges stay valid until `Drop`; registering them once
        // keeps the per-resume path free of global-registry work.
        let [(ctx_lo, ctx_hi), (caller_lo, caller_hi)] = context_root_ranges(&state);
        state.ctx_root_lo = ctx_lo;
        state.ctx_root_hi = ctx_hi;
        state.caller_ctx_root_lo = caller_lo;
        state.caller_ctx_root_hi = caller_hi;
        crate::heap::register_suspended_stack(stack_lo, stack_hi);
        crate::heap::register_suspended_stack(ctx_lo, ctx_hi);
        crate::heap::register_suspended_stack(caller_lo, caller_hi);
        Some(state)
    }

    /// Marks the values owned by this object. The fiber stack and the two
    /// register contexts are scanned separately by the collector.
    ///
    /// The precise shadow roots are marked too: while a fiber is suspended
    /// they are its only record of the values on its stack, and while it runs
    /// `caller_roots` pins the resumer's frames.
    pub(crate) fn trace(&self, mark: fn(i64)) {
        for &arg in &self.args {
            mark(arg);
        }
        mark(self.yielded);
        mark(self.result);
        mark(self.error);
        mark(self.sent);
        mark(self.awaiting);
        if let Some(injected) = self.injected {
            mark(injected);
        }
        if let Some(pending) = self.pending_raise {
            mark(pending);
        }
        if let Some(WaitRequest::Join(tasks)) = &self.request {
            for &task in tasks {
                mark(task);
            }
        }
        roots::mark_precise(self.saved_roots.frames(), self.saved_roots.ranges(), mark);
        roots::mark_precise(self.caller_roots.frames(), self.caller_roots.ranges(), mark);
    }
}

impl Drop for FiberState {
    fn drop(&mut self) {
        // Drop the precise root descriptors before anything they point at is
        // freed. `clear` only drops the vectors: it never dereferences a slot,
        // so a stale frame on a dead fiber stack cannot be scanned here.
        self.saved_roots.clear();
        self.caller_roots.clear();
        // The lifetime root ranges must be gone before the stack and the
        // contexts are freed.
        crate::heap::unregister_suspended_stack(self.stack_root_lo, self.stack_root_hi);
        crate::heap::unregister_suspended_stack(self.ctx_root_lo, self.ctx_root_hi);
        crate::heap::unregister_suspended_stack(self.caller_ctx_root_lo, self.caller_ctx_root_hi);
        self.stack_root_lo = 0;
        self.stack_root_hi = 0;
        self.ctx_root_lo = 0;
        self.ctx_root_hi = 0;
        self.caller_ctx_root_lo = 0;
        self.caller_ctx_root_hi = 0;
    }
}

/// The fiber that is currently running on this thread.
#[derive(Clone, Copy)]
struct CurrentFiber {
    ptr: *mut FiberState,
    value: i64,
}

thread_local! {
    static CURRENT: Cell<Option<CurrentFiber>> = const { Cell::new(None) };
    /// Chain depth of active `await` frames on the running fiber. Saved into
    /// [`FiberState::await_depth`] across a context switch, so unrelated
    /// tasks never accumulate each other's depth.
    static AWAIT_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Upper bound for nested `await` chains. Each level keeps a Rust frame plus
/// the awaited fiber's trampoline alive; a chain this deep is already far
/// beyond useful programs and a `RuntimeError` is better than a stack probe.
const MAX_AWAIT_DEPTH: usize = 512;

/// Decrements [`AWAIT_DEPTH`] when an `await` frame exits (including when
/// the exception path unwinds through it).
struct AwaitDepthGuard;

impl AwaitDepthGuard {
    fn enter() -> Option<AwaitDepthGuard> {
        let depth = AWAIT_DEPTH.with(|cell| {
            let depth = cell.get() + 1;
            cell.set(depth);
            depth
        });
        if depth > MAX_AWAIT_DEPTH {
            AWAIT_DEPTH.with(|cell| cell.set(depth - 1));
            None
        } else {
            Some(AwaitDepthGuard)
        }
    }
}

impl Drop for AwaitDepthGuard {
    fn drop(&mut self) {
        AWAIT_DEPTH.with(|cell| cell.set(cell.get().saturating_sub(1)));
    }
}

/// Completion counter shared by every thread.
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_finish_seq() -> u64 {
    NEXT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Entry point of every fiber stack. Reads the active fiber from the
/// thread-local installed by [`resume_raw`] (never through varargs), calls the
/// generated trampoline, records the result or pending exception, and swaps
/// back for good.
extern "C" fn fiber_trampoline() {
    let Some(current) = CURRENT.with(Cell::get) else {
        // Nothing to run: never return into uninitialised stack memory.
        std::process::abort();
    };
    let ptr = current.ptr;
    // SAFETY: `resume_raw` keeps this fiber alive and installed it here.
    let (entry, args) = unsafe { ((*ptr).entry, &(*ptr).args as *const Vec<i64>) };
    // SAFETY: the generated wrapper has exactly this signature. The argument
    // buffer lives in the fiber object, which outlives the call.
    let body: extern "C" fn(*const i64) -> i64 = unsafe { std::mem::transmute(entry) };
    let result = body(unsafe { (*args).as_ptr() });
    // SAFETY: as above; no other reference to the fiber is live on this stack.
    unsafe {
        (*ptr).result = result;
        if exc::pending() {
            let error = exc::pending_value();
            (*ptr).error = error;
            if value::exception_data(error).map(|data| data.tag) == Some(exc::CANCELLED_ERROR) {
                (*ptr).cancelled = true;
            }
            let _ = exc::clear();
        }
        (*ptr).finish_seq = next_finish_seq();
        (*ptr).done = true;
    }
    // SAFETY: the fiber is done; swap back to the resumer and never return.
    unsafe { finish_switch(ptr) };
}

/// Restores the caller's stack bounds, records the fiber as finished and
/// swaps back. Never returns.
///
/// # Safety
///
/// `ptr` must point at the currently running fiber.
unsafe fn finish_switch(ptr: *mut FiberState) {
    // SAFETY: caller contract; this fiber owns its stack and is running on it.
    unsafe {
        (*ptr).running = false;
        crate::heap::pop_stack_bounds();
        // The body returned: its precise roots die with this frame. Drop the
        // active set (without scanning it) and make the resumer's roots
        // current again before its native context becomes reachable.
        (*ptr).await_depth = AWAIT_DEPTH.with(Cell::get);
        let _ = roots::take_current();
        let caller = std::mem::take(&mut (*ptr).caller_roots);
        let _ = roots::install(caller);
        // The fiber's activations are dead with its stack; restore the
        // resumer's frame stack before its native context runs again.
        let _ = exc::take_frames();
        exc::install_frames(std::mem::take(&mut (*ptr).caller_frames));
        // The context must never be resumed once the body returned. Save the
        // fiber's (dead) continuation into its own slot and switch to the
        // resumer (`caller_ctx`).
        switch_from_fiber(ptr);
    }
    std::process::abort();
}

/// Restores the caller's stack bounds and switches back to the resumer.
///
/// The fiber stack and both contexts are registered as GC roots for the whole
/// fiber lifetime ([`FiberState::new`]), and the context switch itself saves
/// the callee-saved register words into `ctx`, so no registry or snapshot
/// work happens here.
///
/// # Safety
///
/// `ptr` must point at the currently running fiber.
unsafe fn suspend_switch(ptr: *mut FiberState) {
    // SAFETY: caller contract; this fiber owns its stack and is running on it.
    unsafe {
        (*ptr).running = false;
        crate::heap::pop_stack_bounds();
        // Park this fiber's precise roots in the fiber object, where the
        // collector can still find them, and restore the resumer's set. Both
        // operations only move vectors; no snapshot or lock is taken.
        (*ptr).await_depth = AWAIT_DEPTH.with(Cell::get);
        (*ptr).saved_roots = roots::take_current();
        let caller = std::mem::take(&mut (*ptr).caller_roots);
        let _ = roots::install(caller);
        // Park this fiber's frame records where a retained frame object can
        // still reach them, and make the resumer's stack current again.
        (*ptr).saved_frames = exc::take_frames();
        exc::install_frames(std::mem::take(&mut (*ptr).caller_frames));
        // Save the fiber's continuation into `ctx` and switch back to the
        // resumer (`caller_ctx`).
        switch_from_fiber(ptr);
    }
    // Resumed: the switch restored the fiber's callee-saved registers.
}

/// Resumes `ptr` and reports whether it finished.
///
/// # Safety
///
/// `ptr` must point at a live fiber object.
pub(crate) unsafe fn resume_raw(ptr: *mut FiberState, value: i64) -> bool {
    // SAFETY: caller contract.
    unsafe {
        if (*ptr).done {
            return true;
        }
        if (*ptr).running {
            // Defensive: a fiber cannot be resumed while it runs.
            return false;
        }
        // The switch itself saves the caller's callee-saved registers into
        // `caller_ctx` and its stack frames stay on the native stack. Both
        // are published for the outermost resume; a nested caller is a fiber
        // whose lifetime stack and context ranges are already registered.
        let outermost = CURRENT.with(Cell::get).is_none();
        let mut aux_published = false;
        if outermost {
            let caller_lo = caller_stack_low();
            let caller_hi = crate::heap::stack_bounds()
                .map(|(_, high)| high)
                .unwrap_or(caller_lo);
            if caller_hi > caller_lo {
                // Cover the native caller frames and, as a second range, the
                // resumer context the upcoming switch is about to fill with
                // the caller's callee-saved registers.
                crate::heap::set_aux_stack(
                    caller_lo,
                    caller_hi,
                    (*ptr).caller_ctx_root_lo,
                    (*ptr).caller_ctx_root_hi,
                );
                aux_published = true;
            }
        }
        (*ptr).started = true;
        (*ptr).sent = value;
        (*ptr).running = true;
        crate::heap::push_stack_bounds((*ptr).stack.low(), (*ptr).stack.high());
        // Save the resumer's await-chain depth and install the callee's. A
        // fresh fiber starts at the resumer's depth (it is being awaited),
        // while a scheduler-resumed task uses its own saved depth.
        let resume_depth = AWAIT_DEPTH.with(Cell::get);
        let callee_depth = (*ptr).await_depth.max(resume_depth);
        AWAIT_DEPTH.with(|cell| cell.set(callee_depth));
        // Save the resumer's precise roots in the fiber object and install
        // this fiber's set (empty on the first resume, restored after a
        // suspension). The resumer's roots stay scannable there while its
        // native stack is inactive.
        (*ptr).caller_roots = roots::take_current();
        let _ = roots::install(std::mem::take(&mut (*ptr).saved_roots));
        // Frame records follow the same split: `sys._getframe` inside the
        // fiber must not observe the resumer's activations.
        (*ptr).caller_frames = exc::take_frames();
        exc::install_frames(std::mem::take(&mut (*ptr).saved_frames));
        let outer = CURRENT.with(|cell| cell.replace(Some(CurrentFiber { ptr, value })));
        // Resume: save the caller's continuation into `caller_ctx` and switch
        // to the fiber's `ctx` (the trampoline entry on first resume, the
        // suspension point afterwards).
        switch_to_fiber(ptr);
        CURRENT.with(|cell| cell.set(outer));
        AWAIT_DEPTH.with(|cell| cell.set(resume_depth));
        (*ptr).running = false;
        // The suspension/finish path restored the caller's stack bounds
        // before switching back.
        if aux_published {
            crate::heap::clear_aux_stack();
        }
        (*ptr).done
    }
}

/// Whether a tagged value is a fiber object.
pub(crate) fn is_fiber(value: i64) -> bool {
    value::as_fiber(value).is_some()
}

pub(crate) fn kind(value: i64) -> Option<FiberKind> {
    value::as_fiber(value).map(|fiber| fiber.kind)
}

pub(crate) fn is_done(value: i64) -> bool {
    value::as_fiber(value)
        .map(|fiber| fiber.done)
        .unwrap_or(true)
}

pub(crate) fn has_error(value: i64) -> bool {
    value::as_fiber(value)
        .map(|fiber| fiber.error != NONE)
        .unwrap_or(false)
}

pub(crate) fn finish_sequence(value: i64) -> u64 {
    value::as_fiber(value)
        .map(|fiber| fiber.finish_seq)
        .unwrap_or(0)
}

/// The return value of a finished fiber (ignores a pending error).
pub(crate) fn result_value(value: i64) -> i64 {
    value::as_fiber(value)
        .map(|fiber| fiber.result)
        .unwrap_or(NONE)
}

/// Re-raises the exception a fiber captured and returns `None`.
pub(crate) fn raise_error(value: i64) -> i64 {
    let error = value::as_fiber(value)
        .map(|fiber| fiber.error)
        .unwrap_or(NONE);
    if error != NONE {
        exc::raise_value(error);
    }
    NONE
}

/// Finishes an accomplished fiber: its result, or the captured exception
/// re-raised into the caller.
pub(crate) fn finish_value(value: i64) -> i64 {
    if has_error(value) {
        return raise_error(value);
    }
    result_value(value)
}

/// The value passed to the last `hydro_yield`.
pub(crate) fn yielded_value(value: i64) -> i64 {
    value::as_fiber(value)
        .map(|fiber| fiber.yielded)
        .unwrap_or(NONE)
}

/// Tagged value of the fiber running on this thread, if any.
pub(crate) fn current_value() -> Option<i64> {
    CURRENT.with(Cell::get).map(|current| current.value)
}

fn current_ptr() -> Option<*mut FiberState> {
    CURRENT.with(Cell::get).map(|current| current.ptr)
}

/// Re-raises a detached exception value. The value is rooted while the
/// runtime clones it into the pending slot (the clone allocates).
fn raise_detached(error: i64) -> i64 {
    let token = crate::heap::root_values(&[error]);
    exc::raise_value(error);
    crate::heap::release_roots(token);
    NONE
}

/// Takes the exception queued by `throw()`/`close()` or by cancellation.
/// `injected` wins: an explicit `throw`/`close` is delivered before a
/// cancellation request queued while the fiber was suspended.
fn take_suspension_raise(ptr: *mut FiberState) -> Option<i64> {
    // SAFETY: `ptr` is a live fiber.
    unsafe {
        if let Some(injected) = (*ptr).injected.take() {
            return Some(injected);
        }
        (*ptr).pending_raise.take()
    }
}

/// Suspends the running fiber with an optional wait request. Returns `None`
/// once the fiber is resumed.
pub(crate) fn suspend_current(request: Option<WaitRequest>) -> i64 {
    let Some(ptr) = current_ptr() else {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            "cannot suspend outside of a coroutine or generator",
        );
    };
    // SAFETY: `ptr` is the running fiber installed by `resume_raw`.
    unsafe {
        (*ptr).request = request;
        suspend_switch(ptr);
        if let Some(error) = take_suspension_raise(ptr) {
            return raise_detached(error);
        }
    }
    NONE
}

/// `hydro_yield`: stores `value` as the yielded value and suspends the running
/// generator. Outside a running generator this raises `RuntimeError`.
pub(crate) fn yield_value(value: i64) -> i64 {
    let Some(ptr) = current_ptr() else {
        return exc::raise_with_message(RUNTIME_ERROR, "'yield' outside of a generator");
    };
    // SAFETY: `ptr` is the running fiber installed by `resume_raw`.
    let kind = unsafe { (*ptr).kind };
    if !matches!(kind, FiberKind::Generator | FiberKind::AsyncGenerator) {
        return exc::raise_with_message(RUNTIME_ERROR, "'yield' outside of a generator");
    }
    unsafe {
        (*ptr).yielded = value;
        (*ptr).request = None;
        suspend_switch(ptr);
        // An exception thrown, injected by `close()` or queued by task
        // cancellation is raised at the suspension point, so codegen routes
        // it through the generator's handlers and `finally` blocks.
        if let Some(injected) = take_suspension_raise(ptr) {
            return raise_detached(injected);
        }
        // `close()` resumed this generator after injecting `GeneratorExit`;
        // a further yield means the body ignored it.
        if (*ptr).closing {
            return exc::raise_with_message(RUNTIME_ERROR, "generator ignored GeneratorExit");
        }
        // The value passed to the resume that continued this generator.
        (*ptr).sent
    }
}

/// Whether a generator has been resumed at least once.
pub(crate) fn is_started(value: i64) -> bool {
    value::as_fiber(value)
        .map(|fiber| fiber.started)
        .unwrap_or(false)
}

/// `generator.send(value)`.
///
/// A just-started generator only accepts `None`; any other value is a
/// `TypeError`, mirroring CPython. Resuming returns the next yielded value or
/// raises `StopIteration` when the body finishes. An exception raised by the
/// body propagates unchanged.
pub(crate) fn send_value(generator: i64, value: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(generator) else {
        return exc::raise_with_message(TYPE_ERROR, "send() requires a generator object");
    };
    // SAFETY: `fiber_raw` returned the address of a live fiber payload that
    // stays reachable through `generator` for this call.
    unsafe {
        if (*ptr).done {
            return exc::raise_with_message(STOP_ITERATION, "");
        }
        if !is_started(generator) && value != NONE {
            return exc::raise_with_message(
                TYPE_ERROR,
                "can't send non-None value to a just-started generator",
            );
        }
        resume_raw(ptr, value);
        if (*ptr).error != NONE {
            return raise_error(generator);
        }
        if (*ptr).done {
            return exc::raise_with_message(STOP_ITERATION, "");
        }
        (*ptr).yielded
    }
}

/// `generator.throw(exception)`: raises `exception` at the suspension point.
///
/// Only exception *instances* are accepted (Hydro has no exception class
/// objects), and only the single-argument form is supported.
pub(crate) fn throw_value(generator: i64, exception: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(generator) else {
        return exc::raise_with_message(TYPE_ERROR, "throw() requires a generator object");
    };
    if value::exception_data(exception).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "throw() argument must be an exception instance",
        );
    }
    // SAFETY: `fiber_raw` returned a live fiber payload.
    unsafe {
        if (*ptr).done {
            return exc::raise_with_message(STOP_ITERATION, "");
        }
        if !(*ptr).started {
            // Throwing into a just-started generator raises the exception
            // without running the body and closes the generator.
            (*ptr).done = true;
            (*ptr).finish_seq = next_finish_seq();
            return exc::raise_value(exception);
        }
        (*ptr).injected = Some(exception);
        resume_raw(ptr, NONE);
        if (*ptr).error != NONE {
            return raise_error(generator);
        }
        if (*ptr).done {
            return exc::raise_with_message(STOP_ITERATION, "");
        }
        (*ptr).yielded
    }
}

/// `generator.close()`: runs the generator's `finally` blocks by raising
/// `GeneratorExit` at its suspension point.
pub(crate) fn close_value(generator: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(generator) else {
        return exc::raise_with_message(TYPE_ERROR, "close() requires a generator object");
    };
    // SAFETY: `fiber_raw` returned a live fiber payload.
    unsafe {
        if (*ptr).done {
            return NONE;
        }
        if !(*ptr).started {
            // A generator whose body never ran has no `finally` to run.
            (*ptr).done = true;
            (*ptr).finish_seq = next_finish_seq();
            return NONE;
        }
        exc::raise_with_message(exc::GENERATOR_EXIT, "");
        let exit = exc::pending_value();
        let _ = exc::clear();
        (*ptr).injected = Some(exit);
        (*ptr).closing = true;
        resume_raw(ptr, NONE);
        let error = std::mem::replace(&mut (*ptr).error, NONE);
        if error != NONE {
            let tag = value::exception_data(error).map(|data| data.tag);
            if tag == Some(exc::GENERATOR_EXIT) {
                // The injected GeneratorExit propagated out of the body:
                // this is the normal `close()` outcome.
                (*ptr).closing = false;
                return NONE;
            }
            exc::raise_value(error);
            return NONE;
        }
        if (*ptr).done {
            (*ptr).closing = false;
            return NONE;
        }
        // The generator yielded while closing: CPython treats it as closed
        // and reports the ignored GeneratorExit.
        (*ptr).done = true;
        (*ptr).closing = false;
        (*ptr).finish_seq = next_finish_seq();
        exc::raise_with_message(RUNTIME_ERROR, "generator ignored GeneratorExit")
    }
}

/// Builds a detached exception value and clears the pending slot.
fn detached_exception(tag: u32, message: &str) -> i64 {
    exc::raise_with_message(tag, message);
    let value = exc::pending_value();
    let _ = exc::clear();
    value
}

/// `__aiter__` of an async generator: the object is its own iterator.
pub(crate) fn async_generator_aiter(value: i64) -> i64 {
    if value::as_fiber(value)
        .map(|fiber| fiber.kind == FiberKind::AsyncGenerator)
        .unwrap_or(false)
    {
        value
    } else {
        exc::raise_with_message(TYPE_ERROR, "object is not an async iterable")
    }
}

/// Resumes an async generator and drives its wait requests, returning the
/// yielded item. The caller has already validated `ptr` and the resume value.
fn drive_async_generator(agen: i64, ptr: *mut FiberState, first_value: i64) -> i64 {
    let Some(_depth) = AwaitDepthGuard::enter() else {
        return exc::raise_with_message(RUNTIME_ERROR, "await chain is too deep");
    };
    let mut resume_value = first_value;
    loop {
        // SAFETY: `ptr` is a live fiber that is not running.
        unsafe {
            resume_raw(ptr, resume_value);
        }
        resume_value = NONE;
        if unsafe { (*ptr).error != NONE } {
            return raise_error(agen);
        }
        if unsafe { (*ptr).done } {
            return exc::raise_with_message(exc::STOP_ASYNC_ITERATION, "");
        }
        let has_request = unsafe { (*ptr).request.is_some() };
        if has_request {
            if current_ptr().is_none() {
                service_pending_request(ptr);
                continue;
            }
            // Propagate the suspension outwards; the enclosing task's driver
            // resumes us and this loop continues the async generator.
            let request = unsafe { (*ptr).request.take() };
            let _ = suspend_current(request);
            if exc::pending() {
                return NONE;
            }
            continue;
        }
        // No request: the body executed `yield`, so `yielded` holds the item.
        return unsafe { (*ptr).yielded };
    }
}

/// `async_generator.__anext__()`: returns the next item directly (await
/// passes non-fibers through), or raises `StopAsyncIteration` at completion.
pub(crate) fn async_generator_next(agen: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(agen) else {
        return exc::raise_with_message(TYPE_ERROR, "__anext__() requires an async generator");
    };
    // SAFETY: `fiber_raw` returned a live fiber payload.
    unsafe {
        if (*ptr).kind != FiberKind::AsyncGenerator {
            return exc::raise_with_message(TYPE_ERROR, "__anext__() requires an async generator");
        }
        if current_ptr() == Some(ptr) {
            return exc::raise_with_message(RUNTIME_ERROR, "async generator already executing");
        }
        if (*ptr).done {
            return exc::raise_with_message(exc::STOP_ASYNC_ITERATION, "");
        }
    }
    drive_async_generator(agen, ptr, NONE)
}

/// `async_generator.asend(value)`: resumes the generator delivering `value`.
pub(crate) fn asend_value(agen: i64, value: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(agen) else {
        return exc::raise_with_message(TYPE_ERROR, "asend() requires an async generator");
    };
    // SAFETY: `fiber_raw` returned a live fiber payload.
    unsafe {
        if (*ptr).kind != FiberKind::AsyncGenerator {
            return exc::raise_with_message(TYPE_ERROR, "asend() requires an async generator");
        }
        if current_ptr() == Some(ptr) {
            return exc::raise_with_message(RUNTIME_ERROR, "async generator already executing");
        }
        if (*ptr).done {
            return exc::raise_with_message(exc::STOP_ASYNC_ITERATION, "");
        }
        if !(*ptr).started && value != NONE {
            return exc::raise_with_message(
                TYPE_ERROR,
                "can't send non-None value to a just-started async generator",
            );
        }
    }
    drive_async_generator(agen, ptr, value)
}

/// `async_generator.athrow(exception)`: raises `exception` at the suspension
/// point and returns the item the handler yields, or raises
/// `StopAsyncIteration` when the body finishes.
pub(crate) fn athrow_value(agen: i64, exception: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(agen) else {
        return exc::raise_with_message(TYPE_ERROR, "athrow() requires an async generator");
    };
    if value::exception_data(exception).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "athrow() argument must be an exception instance",
        );
    }
    // SAFETY: `fiber_raw` returned a live fiber payload.
    unsafe {
        if (*ptr).kind != FiberKind::AsyncGenerator {
            return exc::raise_with_message(TYPE_ERROR, "athrow() requires an async generator");
        }
        if current_ptr() == Some(ptr) {
            return exc::raise_with_message(RUNTIME_ERROR, "async generator already executing");
        }
        if (*ptr).done {
            return exc::raise_with_message(exc::STOP_ASYNC_ITERATION, "");
        }
        if !(*ptr).started {
            (*ptr).done = true;
            (*ptr).finish_seq = next_finish_seq();
            return exc::raise_value(exception);
        }
        (*ptr).injected = Some(exception);
    }
    drive_async_generator(agen, ptr, NONE)
}

/// `async_generator.aclose()`: runs the generator's `finally` blocks by
/// raising `GeneratorExit` at its suspension point.
pub(crate) fn aclose_value(agen: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(agen) else {
        return exc::raise_with_message(TYPE_ERROR, "aclose() requires an async generator");
    };
    // SAFETY: `fiber_raw` returned a live fiber payload.
    unsafe {
        if (*ptr).kind != FiberKind::AsyncGenerator {
            return exc::raise_with_message(TYPE_ERROR, "aclose() requires an async generator");
        }
        if current_ptr() == Some(ptr) {
            return exc::raise_with_message(RUNTIME_ERROR, "async generator already executing");
        }
        if (*ptr).done {
            return NONE;
        }
        if !(*ptr).started {
            (*ptr).done = true;
            (*ptr).finish_seq = next_finish_seq();
            return NONE;
        }
        (*ptr).injected = Some(detached_exception(exc::GENERATOR_EXIT, ""));
        (*ptr).closing = true;
    }
    loop {
        // SAFETY: `ptr` is a live, suspended async generator.
        unsafe { resume_raw(ptr, NONE) };
        let error = unsafe { std::mem::replace(&mut (*ptr).error, NONE) };
        if error != NONE {
            let tag = value::exception_data(error).map(|data| data.tag);
            if tag == Some(exc::GENERATOR_EXIT) {
                unsafe { (*ptr).closing = false };
                return NONE;
            }
            // `error` was detached from the fiber, so root it while
            // `raise_value` allocates the pending copy.
            let token = crate::heap::root_values(&[error]);
            exc::raise_value(error);
            crate::heap::release_roots(token);
            return NONE;
        }
        if unsafe { (*ptr).done } {
            unsafe { (*ptr).closing = false };
            return NONE;
        }
        if unsafe { (*ptr).request.is_some() } {
            let request = unsafe { (*ptr).request.take() };
            let _ = suspend_current(request);
            if exc::pending() {
                return NONE;
            }
            continue;
        }
        // A yield while closing is an ignored GeneratorExit.
        unsafe {
            (*ptr).done = true;
            (*ptr).closing = false;
            (*ptr).finish_seq = next_finish_seq();
        }
        return exc::raise_with_message(RUNTIME_ERROR, "async generator ignored GeneratorExit");
    }
}

/// Queues `CancelledError` for `task`. A task that has not started is
/// finished immediately, mirroring CPython's "cancelled before first step".
pub(crate) fn cancel_value(task: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(task) else {
        return exc::raise_with_message(TYPE_ERROR, "cancel() requires a task");
    };
    // SAFETY: `fiber_raw` returned a live fiber payload.
    unsafe {
        if (*ptr).kind != FiberKind::Coroutine {
            return exc::raise_with_message(TYPE_ERROR, "cancel() requires a task");
        }
        if (*ptr).done || (*ptr).cancel_requested {
            return NONE;
        }
        (*ptr).cancel_requested = true;
        if !(*ptr).started {
            (*ptr).error = detached_exception(exc::CANCELLED_ERROR, "");
            (*ptr).cancelled = true;
            (*ptr).done = true;
            (*ptr).finish_seq = next_finish_seq();
            return NONE;
        }
        (*ptr).pending_raise = Some(detached_exception(exc::CANCELLED_ERROR, ""));
    }
    if current_ptr() != Some(ptr) {
        crate::modules::asyncio::wake_task(task);
    }
    NONE
}

/// Whether a task finished by propagating `CancelledError`.
pub(crate) fn is_cancelled(value: i64) -> bool {
    value::as_fiber(value)
        .map(|fiber| fiber.cancelled)
        .unwrap_or(false)
}

/// Marks a task as queued on (or removed from) the scheduler.
pub(crate) fn set_scheduled(value: i64, scheduled: bool) {
    if let Some(ptr) = value::fiber_raw(value) {
        // SAFETY: `fiber_raw` returned a live fiber payload.
        unsafe { (*ptr).scheduled = scheduled };
    }
}

/// Whether a task is currently queued on the scheduler's ready/timer lists.
pub(crate) fn is_scheduled(value: i64) -> bool {
    value::as_fiber(value)
        .map(|fiber| fiber.scheduled)
        .unwrap_or(false)
}

/// Marks a coroutine as a task owned by the asyncio loop.
pub(crate) fn mark_owned(value: i64) {
    if let Some(ptr) = value::fiber_raw(value) {
        // SAFETY: `fiber_raw` returned a live fiber payload.
        unsafe { (*ptr).owned = true };
    }
}

/// The exception captured by a finished task (`NONE` when it returned).
pub(crate) fn exception_value(value: i64) -> i64 {
    value::as_fiber(value)
        .map(|fiber| fiber.error)
        .unwrap_or(NONE)
}

/// One advance of a `yield from` delegate.
enum DelegateStep {
    /// The delegate produced a value for the outer generator to yield.
    Item(i64),
    /// The delegate finished normally with this result value.
    Done(i64),
    /// An exception is pending and must propagate through the outer generator.
    Failed,
}

/// Maps a pending delegate completion to [`DelegateStep::Done`]. A pending
/// `StopIteration` is the normal end of iteration; every other exception
/// propagates unchanged.
fn finish_delegate(delegate: i64) -> DelegateStep {
    if exc::pending_tag() == STOP_ITERATION as i64 {
        let _ = exc::clear();
        DelegateStep::Done(result_value(delegate))
    } else {
        DelegateStep::Failed
    }
}

/// Advances a `yield from` delegate by one step, forwarding `sent`.
///
/// A delegated generator uses the same `send` semantics as a Python call
/// (including `StopIteration` on completion and exception propagation); a
/// plain iterator only accepts `None`, mirroring CPython (`send` on an
/// iterator without a `send` method).
fn advance_delegate(delegate: i64, is_generator: bool, sent: i64) -> DelegateStep {
    if is_generator {
        let item = send_value(delegate, sent);
        if exc::pending() {
            return finish_delegate(delegate);
        }
        return DelegateStep::Item(item);
    }
    if sent != NONE {
        exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!(
                "'{}' object has no attribute 'send'",
                value::type_name(delegate)
            ),
        );
        return DelegateStep::Failed;
    }
    let mut out = NONE;
    if crate::iter::iter_next(delegate, &mut out) == 1 {
        DelegateStep::Item(out)
    } else if exc::pending() {
        DelegateStep::Failed
    } else {
        DelegateStep::Done(NONE)
    }
}

/// Forwards an injected exception into a delegated generator.
fn throw_delegate(delegate: i64, exception: i64) -> DelegateStep {
    let item = throw_value(delegate, exception);
    if exc::pending() {
        return finish_delegate(delegate);
    }
    DelegateStep::Item(item)
}

/// `yield from iterable`: drives `iterable` from the running generator.
///
/// Each delegated item is yielded through the outer generator. `send` values
/// are forwarded to a delegated generator; `throw` exceptions are forwarded
/// as well, and `close` closes the delegate first so its `finally` blocks run.
/// The expression evaluates to the delegate's return value (`None` for a plain
/// iterator). Outside a running generator this raises `RuntimeError`.
pub(crate) fn yield_from_value(iterable: i64) -> i64 {
    let Some(outer) = current_ptr() else {
        return exc::raise_with_message(RUNTIME_ERROR, "'yield from' outside of a generator");
    };
    // SAFETY: `outer` is the running fiber installed by `resume_raw`.
    if unsafe { (*outer).kind } != FiberKind::Generator {
        return exc::raise_with_message(RUNTIME_ERROR, "'yield from' outside of a generator");
    }
    let delegate = crate::iter::iter_of(iterable);
    if exc::pending() {
        return NONE;
    }
    let delegate_is_generator = value::as_fiber(delegate)
        .map(|fiber| fiber.kind == FiberKind::Generator)
        .unwrap_or(false);
    // Delegating to a generator that is still on a stack (the outer one or an
    // ancestor in a cycle) would resume a running fiber.
    if value::as_fiber(delegate)
        .map(|fiber| fiber.running)
        .unwrap_or(false)
    {
        return exc::raise_with_message(RUNTIME_ERROR, "generator already executing");
    }

    let mut to_send = NONE;
    let mut pending_item: Option<i64> = None;
    loop {
        let item = match pending_item.take() {
            Some(item) => item,
            None => match advance_delegate(delegate, delegate_is_generator, to_send) {
                DelegateStep::Item(item) => item,
                DelegateStep::Done(value) => return value,
                DelegateStep::Failed => return NONE,
            },
        };
        // Suspend the outer generator with the delegate's item. The result is
        // the next `send` value; a pending exception means `throw`/`close`
        // interrupted the delegation.
        let sent = yield_value(item);
        if !exc::pending() {
            to_send = sent;
            continue;
        }
        // Take the injected exception before `close_value`/`throw_value` can
        // overwrite the pending slot; the native local keeps it rooted.
        let exception = exc::pending_value();
        let tag = exc::pending_tag();
        let _ = exc::clear();
        if !delegate_is_generator {
            // A plain iterator has no `throw`/`close`: the exception
            // propagates in the outer generator.
            exc::raise_value(exception);
            return NONE;
        }
        if tag == exc::GENERATOR_EXIT as i64 {
            // `close()`: close the delegate first so its `finally` blocks
            // run, then re-raise GeneratorExit through the outer generator.
            close_value(delegate);
            if exc::pending() {
                return NONE;
            }
            exc::raise_value(exception);
            return NONE;
        }
        // `throw()`: forward the exception; a value yielded by the delegate's
        // handler is yielded again by the outer generator.
        pending_item = match throw_delegate(delegate, exception) {
            DelegateStep::Item(item) => Some(item),
            DelegateStep::Done(value) => return value,
            DelegateStep::Failed => return NONE,
        };
    }
}

/// Resume a fiber by tagged value; `None` if `value` is not a fiber.
pub(crate) fn resume_value(value: i64) -> Option<bool> {
    let ptr = value::fiber_raw(value)?;
    // Iteration and `await` deliver `None`; only `send()` passes a value.
    // SAFETY: `fiber_raw` returned the address of a live fiber payload.
    Some(unsafe { resume_raw(ptr, NONE) })
}

/// Takes the outstanding suspension request (if any).
pub(crate) fn take_request(value: i64) -> Option<WaitRequest> {
    let ptr = value::fiber_raw(value)?;
    // SAFETY: `fiber_raw` returned the address of a live fiber payload.
    unsafe { (*ptr).request.take() }
}

/// Waits for a task the scheduler owns without resuming it here.
fn await_scheduled_task(value: i64, ptr: *mut FiberState) -> i64 {
    if unsafe { (*ptr).done } {
        return finish_value(value);
    }
    if current_ptr().is_none() {
        crate::modules::asyncio::drive_join_tasks(&[value]);
        let token = crate::heap::root_values(&[value]);
        let result = finish_value(value);
        crate::heap::release_roots(token);
        return result;
    }
    loop {
        if unsafe { (*ptr).done } {
            return finish_value(value);
        }
        let _ = suspend_current(Some(WaitRequest::Join(vec![value])));
        if exc::pending() {
            return NONE;
        }
    }
}

/// `hydro_await`: drives an awaitable value.
///
/// Non-fiber values are returned unchanged (Hydro has no other awaitables). A
/// coroutine/generator fiber is resumed until it finishes. A suspension with
/// a wait request either suspends the calling fiber with the same request
/// (nested `await`) or is serviced synchronously at the top level.
pub(crate) fn await_value(value: i64) -> i64 {
    let Some(ptr) = value::fiber_raw(value) else {
        return value;
    };
    if current_ptr() == Some(ptr) {
        return exc::raise_with_message(RUNTIME_ERROR, "cannot await the running coroutine");
    }
    // Only coroutines are awaitable. Generators and async generators are
    // driven by iteration (and their `__anext__` returns the item directly).
    match unsafe { (*ptr).kind } {
        FiberKind::Coroutine => {}
        FiberKind::Generator => {
            return exc::raise_with_message(TYPE_ERROR, "'generator' object is not awaitable")
        }
        FiberKind::AsyncGenerator => {
            return exc::raise_with_message(TYPE_ERROR, "'async_generator' object is not awaitable")
        }
    }
    // A task owned by the driver must not be resumed from the awaiting
    // fiber's stack: the scheduler will run it. Wait through a join request
    // instead, so `await task` observes the task's completion without
    // executing it twice.
    if unsafe { (*ptr).owned } {
        return await_scheduled_task(value, ptr);
    }
    let Some(_depth) = AwaitDepthGuard::enter() else {
        return exc::raise_with_message(RUNTIME_ERROR, "await chain is too deep");
    };
    // SAFETY: `fiber_raw` returned a live fiber payload. The awaited value
    // stays reachable through the awaiting fiber's field while it runs.
    let caller = current_ptr();
    if let Some(caller) = caller {
        unsafe { (*caller).awaiting = value };
    }
    let clear_awaiting = |ptr: *mut FiberState| {
        // SAFETY: `ptr` is the live calling fiber installed by `resume_raw`.
        unsafe { (*ptr).awaiting = NONE };
    };
    loop {
        if unsafe { (*ptr).done } {
            if let Some(caller) = caller {
                clear_awaiting(caller);
            }
            return finish_value(value);
        }
        // SAFETY: `ptr` is live and not done.
        unsafe { resume_raw(ptr, NONE) };
        if unsafe { (*ptr).done } {
            if let Some(caller) = caller {
                clear_awaiting(caller);
            }
            return finish_value(value);
        }
        let has_request = unsafe { (*ptr).request.is_some() };
        if !has_request {
            // A bare coroutine suspension: resume it again, delivering
            // `None`, mirroring a bare `yield` inside a coroutine.
            continue;
        }
        if current_ptr().is_none() {
            service_pending_request(ptr);
        } else {
            // Propagate the request outwards: the caller's driver decides how
            // to service it. `suspend_current` delivers a cancellation queued
            // for this fiber.
            let request = unsafe { (*ptr).request.take() };
            let _ = suspend_current(request);
            if exc::pending() {
                if let Some(caller) = caller {
                    clear_awaiting(caller);
                }
                return NONE;
            }
        }
    }
}

/// Services the request stored in `ptr` at the top level (no enclosing fiber).
/// The request stays in the fiber object while it runs, so its tagged task
/// values stay reachable.
fn service_pending_request(ptr: *mut FiberState) {
    // SAFETY: `ptr` is a live, suspended fiber.
    let request = unsafe {
        match &(*ptr).request {
            Some(WaitRequest::Sleep(duration)) => Some(WaitRequest::Sleep(*duration)),
            Some(WaitRequest::Join(tasks)) => Some(WaitRequest::Join(tasks.clone())),
            None => None,
        }
    };
    match request {
        Some(WaitRequest::Sleep(duration)) => blocking_sleep(duration),
        Some(WaitRequest::Join(tasks)) => crate::modules::asyncio::drive_join_tasks(&tasks),
        None => {}
    }
    // SAFETY: the fiber is still suspended and owned by this thread.
    unsafe { (*ptr).request = None };
}

/// Parks for the collector while sleeping.
pub(crate) fn blocking_sleep(duration: Duration) {
    if duration.is_zero() {
        return;
    }
    crate::heap::blocking_enter();
    std::thread::sleep(duration);
    crate::heap::blocking_leave();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable")
    }

    fn generator(entry: extern "C" fn(*const i64) -> i64) -> i64 {
        exc::init();
        value::fiber_generator_new(entry as usize, "gen".to_string(), Vec::new())
            .expect("generator stack")
    }

    /// Builds an exception object and leaves no pending exception behind.
    fn exception(tag: u32, message: &str) -> i64 {
        exc::raise_with_message(tag, message);
        let value = exc::pending_value();
        exc::clear();
        value
    }

    extern "C" fn echoes_sent(_argv: *const i64) -> i64 {
        let received = yield_value(value::encode_int(1).expect("representable"));
        yield_value(received);
        NONE
    }

    extern "C" fn yields_once(_argv: *const i64) -> i64 {
        yield_value(value::encode_int(1).expect("representable"));
        NONE
    }

    /// Simulates a body that does not catch the exception injected at its
    /// suspension point.
    extern "C" fn leaves_exception_pending(_argv: *const i64) -> i64 {
        yield_value(value::encode_int(1).expect("representable"));
        NONE
    }

    /// Simulates `except ValueError: yield 2`.
    extern "C" fn catches_and_yields(_argv: *const i64) -> i64 {
        yield_value(value::encode_int(1).expect("representable"));
        if exc::pending() {
            exc::clear();
        }
        yield_value(value::encode_int(2).expect("representable"));
        NONE
    }

    /// Simulates a `finally` block that yields while the generator is closing.
    extern "C" fn ignores_close(_argv: *const i64) -> i64 {
        yield_value(value::encode_int(1).expect("representable"));
        exc::clear();
        yield_value(value::encode_int(2).expect("representable"));
        NONE
    }

    #[test]
    fn send_delivers_values_and_reports_stop_iteration() {
        let generator = generator(echoes_sent);
        assert!(!is_started(generator));

        assert_eq!(send_value(generator, NONE), int(1));
        assert!(is_started(generator));
        assert!(!exc::pending());

        assert_eq!(send_value(generator, int(5)), int(5));
        assert!(!exc::pending());

        assert_eq!(send_value(generator, NONE), NONE);
        assert_eq!(exc::pending_tag(), STOP_ITERATION as i64);

        exc::init();
        assert_eq!(send_value(generator, int(7)), NONE);
        assert_eq!(exc::pending_tag(), STOP_ITERATION as i64);
    }

    #[test]
    fn send_rejects_a_value_to_a_just_started_generator() {
        let generator = generator(echoes_sent);
        assert_eq!(send_value(generator, int(5)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "can't send non-None value to a just-started generator"
        );
        // The rejected send must not start the body.
        assert!(!is_started(generator));
        exc::init();
        assert_eq!(send_value(generator, NONE), int(1));
    }

    #[test]
    fn throw_into_a_just_started_generator_raises_without_running_the_body() {
        let generator = generator(yields_once);
        let thrown = exception(exc::VALUE_ERROR, "thrown");
        assert_eq!(throw_value(generator, thrown), NONE);
        assert_eq!(exc::pending_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(exc::pending_value(), thrown);
        assert!(is_done(generator));
        assert!(!is_started(generator));
    }

    #[test]
    fn throw_propagates_an_uncaught_exception() {
        let generator = generator(leaves_exception_pending);
        assert_eq!(send_value(generator, NONE), int(1));

        let thrown = exception(exc::VALUE_ERROR, "thrown");
        assert_eq!(throw_value(generator, thrown), NONE);
        assert_eq!(exc::pending_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(exc::pending_value(), thrown);
        assert!(is_done(generator));
    }

    #[test]
    fn throw_returns_the_value_yielded_by_the_handler() {
        let generator = generator(catches_and_yields);
        assert_eq!(send_value(generator, NONE), int(1));

        let thrown = exception(exc::VALUE_ERROR, "thrown");
        assert_eq!(throw_value(generator, thrown), int(2));
        assert!(!exc::pending());
        assert_eq!(send_value(generator, NONE), NONE);
        assert_eq!(exc::pending_tag(), STOP_ITERATION as i64);
    }

    #[test]
    fn throw_rejects_non_exception_values() {
        let generator = generator(yields_once);
        assert_eq!(throw_value(generator, int(3)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "throw() argument must be an exception instance"
        );
        // The generator is untouched and still suspended.
        assert!(!is_done(generator));
        exc::init();
        assert_eq!(send_value(generator, NONE), int(1));
    }

    #[test]
    fn close_swallows_the_injected_generator_exit() {
        let generator = generator(yields_once);
        assert_eq!(send_value(generator, NONE), int(1));
        assert_eq!(close_value(generator), NONE);
        assert!(!exc::pending());
        assert!(is_done(generator));
        assert!(!has_error(generator));

        assert_eq!(send_value(generator, NONE), NONE);
        assert_eq!(exc::pending_tag(), STOP_ITERATION as i64);
    }

    #[test]
    fn close_before_the_first_resume_just_marks_the_generator_done() {
        let generator = generator(yields_once);
        assert!(!is_started(generator));
        assert_eq!(close_value(generator), NONE);
        assert!(is_done(generator));
        assert!(!is_started(generator));
        assert_eq!(send_value(generator, NONE), NONE);
        assert_eq!(exc::pending_tag(), STOP_ITERATION as i64);
    }

    #[test]
    fn yielding_while_closing_is_a_runtime_error() {
        let generator = generator(ignores_close);
        assert_eq!(send_value(generator, NONE), int(1));
        assert_eq!(close_value(generator), NONE);
        assert_eq!(exc::pending_tag(), RUNTIME_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "generator ignored GeneratorExit"
        );
        assert!(is_done(generator));
    }

    extern "C" fn echoes_two_values(_argv: *const i64) -> i64 {
        let received = yield_value(value::encode_int(1).expect("representable"));
        yield_value(received);
        value::encode_int(7).expect("representable")
    }

    extern "C" fn delegates_to_sub(_argv: *const i64) -> i64 {
        let sub = value::fiber_generator_new(
            echoes_two_values as *const () as usize,
            "sub".to_string(),
            Vec::new(),
        )
        .expect("sub generator stack");
        yield_from_value(sub)
    }

    extern "C" fn delegates_to_list(_argv: *const i64) -> i64 {
        let items = value::list_new(vec![
            value::encode_int(1).expect("representable"),
            value::encode_int(2).expect("representable"),
        ]);
        yield_from_value(items)
    }

    #[test]
    fn yield_from_delegates_a_plain_iterator() {
        let generator = generator(delegates_to_list);
        assert_eq!(send_value(generator, NONE), int(1));
        assert_eq!(send_value(generator, NONE), int(2));
        // A plain iterator has no return value: `yield from` evaluates to
        // `None` and the outer body then finishes.
        assert_eq!(send_value(generator, NONE), NONE);
        assert_eq!(exc::pending_tag(), STOP_ITERATION as i64);
    }

    #[test]
    fn yield_from_forwards_send_and_returns_the_delegate_result() {
        let generator = generator(delegates_to_sub);
        assert_eq!(send_value(generator, NONE), int(1));
        assert_eq!(send_value(generator, int(5)), int(5));
        assert_eq!(send_value(generator, NONE), NONE);
        assert_eq!(exc::pending_tag(), STOP_ITERATION as i64);
        assert_eq!(result_value(generator), int(7));
    }

    #[test]
    fn yield_from_rejects_send_to_a_plain_iterator() {
        let generator = generator(delegates_to_list);
        assert_eq!(send_value(generator, NONE), int(1));
        assert_eq!(send_value(generator, int(5)), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'iterator' object has no attribute 'send'"
        );
    }
}
