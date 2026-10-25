//! Unit tests for the precise shadow-stack root registry (P2-06/P2-07).
//!
//! Every test runs on its own OS thread, so the thread-local frame stacks and
//! the per-thread snapshot keys are isolated. The ABI entry points are
//! exercised directly; the snapshot/save/restore API is exercised through the
//! hidden `precise_roots` re-export that the collector integration shares.

use pyx_runtime::abi::{
    pyx_gc_frame_pop, pyx_gc_frame_push, pyx_gc_frame_used, pyx_gc_scope_pop, pyx_gc_scope_push,
    pyx_int, pyx_none,
};
use pyx_runtime::precise_roots::{
    clear_current, clear_thread, current_thread_token, install, publish_current, snapshot_all,
    snapshot_current, snapshot_thread, take_current, PublishedRoots, SavedRoots,
};

/// Drops any roots this test left registered (they may point at test frames
/// that are already gone). The clear never dereferences a slot.
struct CleanRoots;

impl Drop for CleanRoots {
    fn drop(&mut self) {
        clear_current();
    }
}

fn frames_of(snapshot: &PublishedRoots) -> Vec<(usize, usize, usize, usize, usize)> {
    snapshot
        .frames()
        .iter()
        .map(|frame| {
            (
                frame.base as usize,
                frame.capacity,
                frame.used,
                frame.low,
                frame.high,
            )
        })
        .collect()
}

#[test]
fn frame_push_none_fills_and_used_narrows_the_prefix() {
    let _clean = CleanRoots;
    let mut slots = [0i64; 4];
    let base = slots.as_ptr() as usize;

    let token = pyx_gc_frame_push(slots.as_mut_ptr(), slots.len());
    assert_ne!(token, 0, "a frame token is never 0");
    assert_eq!(slots, [pyx_none(); 4], "every slot is filled with None");

    let snapshot = snapshot_current();
    assert_eq!(snapshot.frames().len(), 1);
    let frame = snapshot.frames()[0];
    assert_eq!(frame.base as usize, base);
    assert_eq!(frame.capacity, 4);
    assert_eq!(frame.used, 4);
    assert_eq!(frame.low, base);
    assert_eq!(frame.high, base + 4 * std::mem::size_of::<i64>());
    assert!(snapshot.ranges().is_empty());

    // The frame may narrow the scanned prefix at any point before the pop.
    pyx_gc_frame_used(token, 2);
    let snapshot = snapshot_current();
    assert_eq!(snapshot.frames()[0].used, 2);

    // It may also widen it again within the reservation.
    pyx_gc_frame_used(token, 4);
    assert_eq!(snapshot_current().frames()[0].used, 4);

    pyx_gc_frame_pop(token);
    assert!(snapshot_current().is_empty());
}

#[test]
fn nested_frames_pop_lifo_and_tokens_are_monotonic() {
    let _clean = CleanRoots;
    let mut outer = [0i64; 2];
    let mut inner = [0i64; 1];

    let outer_token = pyx_gc_frame_push(outer.as_mut_ptr(), outer.len());
    let inner_token = pyx_gc_frame_push(inner.as_mut_ptr(), inner.len());
    assert!(inner_token > outer_token, "tokens are monotonic per thread");

    let snapshot = snapshot_current();
    assert_eq!(snapshot.frames().len(), 2);
    assert_eq!(snapshot.frames()[0].base, outer.as_mut_ptr());
    assert_eq!(snapshot.frames()[1].base, inner.as_mut_ptr());

    pyx_gc_frame_pop(inner_token);
    let snapshot = snapshot_current();
    assert_eq!(snapshot.frames().len(), 1);
    assert_eq!(snapshot.frames()[0].base, outer.as_mut_ptr());

    pyx_gc_frame_pop(outer_token);
    assert!(snapshot_current().is_empty());
}

#[test]
fn scope_ranges_are_independent_of_frames() {
    let _clean = CleanRoots;
    let mut frame_slots = [0i64; 1];
    let args = [pyx_int(1), pyx_int(2), pyx_int(3)];

    let frame_token = pyx_gc_frame_push(frame_slots.as_mut_ptr(), frame_slots.len());
    let scope_token = pyx_gc_scope_push(args.as_ptr(), args.len());
    assert_ne!(scope_token, 0);
    assert!(scope_token > frame_token);

    let snapshot = snapshot_current();
    assert_eq!(snapshot.frames().len(), 1);
    assert_eq!(snapshot.ranges().len(), 1);
    let range = snapshot.ranges()[0];
    assert_eq!(range.base, args.as_ptr());
    assert_eq!(range.count, 3);

    // Popping the frame leaves the scope alone, and vice versa: the two
    // stacks are independent even though their tokens share a counter.
    pyx_gc_frame_pop(frame_token);
    let snapshot = snapshot_current();
    assert!(snapshot.frames().is_empty());
    assert_eq!(snapshot.ranges().len(), 1);

    pyx_gc_scope_pop(scope_token);
    assert!(snapshot_current().is_empty());
}

#[test]
fn mark_precise_scans_frames_used_prefix_and_ranges() {
    let _clean = CleanRoots;
    let mut slots = [0i64; 4];
    let args = [pyx_int(20), pyx_int(21)];

    // Push overwrites, so write the values afterwards (as generated code
    // does) and shrink the prefix.
    let token = pyx_gc_frame_push(slots.as_mut_ptr(), slots.len());
    slots[0] = pyx_int(10);
    slots[1] = pyx_int(11);
    assert_eq!(slots[1], pyx_int(11));
    pyx_gc_frame_used(token, 2);
    let scope = pyx_gc_scope_push(args.as_ptr(), args.len());

    let mut marked = Vec::new();
    snapshot_current().mark(|value| marked.push(value));
    assert_eq!(
        marked,
        vec![pyx_int(10), pyx_int(11), pyx_int(20), pyx_int(21)]
    );

    pyx_gc_frame_pop(token);
    pyx_gc_scope_pop(scope);
}

#[test]
fn published_snapshot_is_isolated_from_later_mutation() {
    let _clean = CleanRoots;
    let mut first = [0i64; 2];
    let token = pyx_gc_frame_push(first.as_mut_ptr(), first.len());
    publish_current();

    let thread = current_thread_token();
    let published = snapshot_thread(thread).expect("this thread published its roots");
    assert_eq!(frames_of(&published), frames_of(&snapshot_current()));

    // Mutate the live registry without re-publishing: the collector's copy
    // must stay exactly as published.
    pyx_gc_frame_used(token, 1);
    let mut second = [0i64; 1];
    let second_token = pyx_gc_frame_push(second.as_mut_ptr(), second.len());
    assert_eq!(snapshot_current().frames().len(), 2);

    let published = snapshot_thread(thread).expect("snapshot is still there");
    assert_eq!(published.frames().len(), 1);
    assert_eq!(published.frames()[0].used, 2);

    // Publishing again refreshes the registry with the current roots.
    publish_current();
    let refreshed = snapshot_thread(thread).unwrap();
    assert_eq!(refreshed.frames().len(), 2);
    assert_eq!(refreshed.frames()[0].used, 1);
    assert_eq!(refreshed.frames()[1].base, second.as_mut_ptr());

    pyx_gc_frame_pop(second_token);
    pyx_gc_frame_pop(token);
}

#[test]
fn snapshot_all_is_keyed_by_thread_token() {
    let _clean = CleanRoots;
    let mut slots = [0i64; 2];
    let _token = pyx_gc_frame_push(slots.as_mut_ptr(), slots.len());
    publish_current();

    let thread = current_thread_token();
    let all = snapshot_all();
    let (_, snapshot) = all
        .iter()
        .find(|(token, _)| *token == thread)
        .expect("snapshot_all contains this thread");
    assert_eq!(snapshot.frames().len(), 1);
    assert_eq!(snapshot.frames()[0].base, slots.as_mut_ptr());

    // Tokens are unique and the result is sorted.
    let tokens: Vec<u64> = all.iter().map(|(token, _)| *token).collect();
    assert!(tokens.contains(&thread));
    assert!(tokens.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn clear_current_removes_both_tls_roots_and_publication() {
    let _clean = CleanRoots;
    let mut slots = [0i64; 1];
    let _token = pyx_gc_frame_push(slots.as_mut_ptr(), slots.len());
    publish_current();

    let thread = current_thread_token();
    assert!(snapshot_thread(thread).is_some());

    clear_current();
    assert!(snapshot_current().is_empty());
    assert!(snapshot_thread(thread).is_none());
    assert!(!snapshot_all().iter().any(|(token, _)| *token == thread));

    // The thread token is stable across clears; only the roots are gone.
    assert_eq!(current_thread_token(), thread);
}

#[test]
fn take_and_install_round_trip_preserves_order_and_tokens() {
    let _clean = CleanRoots;
    let mut outer_slots = [0i64; 2];
    let outer_token = pyx_gc_frame_push(outer_slots.as_mut_ptr(), outer_slots.len());
    let outer_args = [pyx_int(1)];
    let outer_scope = pyx_gc_scope_push(outer_args.as_ptr(), outer_args.len());
    let saved_outer: SavedRoots = take_current();
    assert!(
        snapshot_current().is_empty(),
        "take detaches the active set"
    );
    assert_eq!(saved_outer.frames().len(), 1);
    assert_eq!(saved_outer.ranges().len(), 1);

    // A different context (the fiber side) registers its own roots.
    let mut fiber_slots = [0i64; 1];
    let fiber_token = pyx_gc_frame_push(fiber_slots.as_mut_ptr(), fiber_slots.len());
    let displaced = install(saved_outer);
    let active = snapshot_current();
    assert_eq!(active.frames().len(), 1);
    assert_eq!(active.frames()[0].base, outer_slots.as_mut_ptr());
    assert_eq!(active.ranges().len(), 1);

    // Restore the fiber context; its token stack came back with it.
    let saved_outer = install(displaced);
    let active = snapshot_current();
    assert_eq!(active.frames().len(), 1);
    assert_eq!(active.frames()[0].base, fiber_slots.as_mut_ptr());
    pyx_gc_frame_pop(fiber_token);

    // Back to the outer context: the original tokens still pop correctly.
    let _ = install(saved_outer);
    pyx_gc_scope_pop(outer_scope);
    pyx_gc_frame_pop(outer_token);
    assert!(snapshot_current().is_empty());
}

#[test]
fn publication_is_shared_across_threads_and_clear_thread_drops_it() {
    let _clean = CleanRoots;
    let (send_token, receive_token) = std::sync::mpsc::channel::<u64>();
    let (send_ready, receive_ready) = std::sync::mpsc::channel::<u64>();
    let (send_done, receive_done) = std::sync::mpsc::channel::<()>();

    let worker = std::thread::spawn(move || {
        let mut slots = [0i64; 3];
        let _token = pyx_gc_frame_push(slots.as_mut_ptr(), slots.len());
        pyx_gc_frame_used(_token, 1);
        publish_current();
        send_token.send(current_thread_token()).unwrap();
        // Keep the slots alive until the observing thread is done.
        assert_eq!(receive_ready.recv().unwrap(), 1);
        pyx_gc_frame_pop(_token);
        receive_done.recv().unwrap();
        clear_current();
    });

    let worker_token = receive_token.recv().unwrap();
    let published = snapshot_thread(worker_token).expect("worker roots are published");
    assert_eq!(published.frames().len(), 1);
    assert_eq!(published.frames()[0].capacity, 3);
    assert_eq!(published.frames()[0].used, 1);
    assert!(snapshot_all()
        .iter()
        .any(|(token, _)| *token == worker_token));

    send_ready.send(1).unwrap();
    clear_thread(worker_token);
    assert!(snapshot_thread(worker_token).is_none());

    send_done.send(()).unwrap();
    worker.join().unwrap();
}
