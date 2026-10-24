//! Stackful fibers backing lazy generators and coroutines.
//!
//! A [`FiberState`] owns a private 128 KiB stack and a register-switch
//! context. On x86_64 the switch is a minimal callee-saved-register context
//! switch (`ret`-based, no syscalls); other architectures fall back to libc
//! `ucontext`. `resume` switches from the caller to the fiber; `pyx_yield`
//! and every other suspension point switch back. Nothing runs when a fiber is
//! created: the generated `__pyx_wrap_N` trampoline is entered on the first
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

use std::alloc::{self, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::exc::{self, RUNTIME_ERROR, TYPE_ERROR};
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

// `pyx_context_switch(from, to)` saves the SysV callee-saved registers and
// the stack pointer into `from` and resumes `to`.
//
// `call pyx_context_switch` pushes the continuation address, so the saved
// `rsp` points at it and `ret` in the target context resumes exactly where
// the corresponding call happened. Only callee-saved registers are touched:
// caller-saved registers are already spilled by the compiler around the
// call, and `mxcsr`/the x87 control word are never modified.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl pyx_context_switch",
    "pyx_context_switch:",
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
    fn pyx_context_switch(from: *mut Context, to: *const Context);
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
        pyx_context_switch(
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
        pyx_context_switch(
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
pub(crate) const FIBER_STACK_SIZE: usize = 128 * 1024;

/// What a fiber object represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FiberKind {
    /// `def` with `yield`: iterated with `pyx_iter_new`/`pyx_iter_next`.
    Generator,
    /// `async def`: driven by `pyx_await` or `asyncio.run`.
    Coroutine,
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
    /// Address of the generated `__pyx_wrap_N` trampoline.
    entry: usize,
    /// Captures followed by the call arguments (the wrapper's `argv`).
    pub(crate) args: Vec<i64>,
    pub(crate) done: bool,
    pub(crate) running: bool,
    /// Value passed to `pyx_yield` at the last suspension.
    pub(crate) yielded: i64,
    /// Return value recorded when the body finished.
    pub(crate) result: i64,
    /// Pending exception captured from the body (`NONE` if none).
    pub(crate) error: i64,
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
            request: None,
            stack_root_lo: stack_lo,
            stack_root_hi: stack_hi,
            ctx_root_lo: 0,
            ctx_root_hi: 0,
            caller_ctx_root_lo: 0,
            caller_ctx_root_hi: 0,
            finish_seq: 0,
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
    pub(crate) fn trace(&self, mark: fn(i64)) {
        for &arg in &self.args {
            mark(arg);
        }
        mark(self.yielded);
        mark(self.result);
        mark(self.error);
        if let Some(WaitRequest::Join(tasks)) = &self.request {
            for &task in tasks {
                mark(task);
            }
        }
    }
}

impl Drop for FiberState {
    fn drop(&mut self) {
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
            (*ptr).error = exc::pending_value();
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
        (*ptr).running = true;
        crate::heap::push_stack_bounds((*ptr).stack.low(), (*ptr).stack.high());
        let outer = CURRENT.with(|cell| cell.replace(Some(CurrentFiber { ptr, value })));
        // Resume: save the caller's continuation into `caller_ctx` and switch
        // to the fiber's `ctx` (the trampoline entry on first resume, the
        // suspension point afterwards).
        switch_to_fiber(ptr);
        CURRENT.with(|cell| cell.set(outer));
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

/// The value passed to the last `pyx_yield`.
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
    }
    NONE
}

/// `pyx_yield`: stores `value` as the yielded value and suspends the running
/// generator. Outside a running generator this raises `RuntimeError`.
pub(crate) fn yield_value(value: i64) -> i64 {
    let Some(ptr) = current_ptr() else {
        return exc::raise_with_message(RUNTIME_ERROR, "'yield' outside of a generator");
    };
    // SAFETY: `ptr` is the running fiber installed by `resume_raw`.
    if unsafe { (*ptr).kind } != FiberKind::Generator {
        return exc::raise_with_message(RUNTIME_ERROR, "'yield' outside of a generator");
    }
    unsafe {
        (*ptr).yielded = value;
        (*ptr).request = None;
        suspend_switch(ptr);
    }
    NONE
}

/// Resume a fiber by tagged value; `None` if `value` is not a fiber.
pub(crate) fn resume_value(value: i64) -> Option<bool> {
    let ptr = value::fiber_raw(value)?;
    // SAFETY: `fiber_raw` returned the address of a live fiber payload.
    Some(unsafe { resume_raw(ptr, value) })
}

/// Takes the outstanding suspension request (if any).
pub(crate) fn take_request(value: i64) -> Option<WaitRequest> {
    let ptr = value::fiber_raw(value)?;
    // SAFETY: `fiber_raw` returned the address of a live fiber payload.
    unsafe { (*ptr).request.take() }
}

/// `pyx_await`: drives an awaitable value.
///
/// Non-fiber values are returned unchanged (PyX has no other awaitables). A
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
    loop {
        if unsafe { (*ptr).done } {
            return finish_value(value);
        }
        // SAFETY: `ptr` is live and not done.
        unsafe { resume_raw(ptr, value) };
        if unsafe { (*ptr).done } {
            return finish_value(value);
        }
        let has_request = unsafe { (*ptr).request.is_some() };
        if !has_request {
            let generator = unsafe { (*ptr).kind } == FiberKind::Generator;
            if generator {
                return exc::raise_with_message(TYPE_ERROR, "'generator' object is not awaitable");
            }
            // A plain coroutine suspension: resume it again, delivering
            // `None`, mirroring a bare `yield` inside a coroutine.
            continue;
        }
        if current_ptr().is_none() {
            service_pending_request(ptr);
        } else {
            // Propagate the request outwards: the caller's driver decides how
            // to service it.
            let request = unsafe { (*ptr).request.take() };
            if let Some(request) = request {
                suspend_current(Some(request));
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
