//! Scoped parallelism for the two cache-local per-frame passes.
//!
//! Long capture is a burst of work on one core: at 4K a push and its preview
//! cost ~13 ms inside a 33 ms frame budget. A single core held at its boost
//! clock is what makes that burst expensive in package power on parts that must
//! raise every core to serve one. Both passes here split the work into
//! independent bands — rows of a frame, positions in a scoring window — so the
//! helpers read their own slice of an input that is already in memory and write
//! only their own output slice. There is no shared mutable state anywhere
//! between them, which is what keeps the cache traffic to the read-only input
//! and avoids the coherence cost that makes naive threading slower than the
//! single-threaded loop.
//!
//! The thread budget is deliberately small: four helpers is enough to leave the
//! boost range, and keeping the fan-out low keeps every helper on its own L1/L2
//! working set instead of competing for one core's cache.

use std::sync::OnceLock;

/// Upper bound on helper threads for one per-frame pass.
///
/// More helpers would not shorten the memory-bound part (a 4K frame is 25 MB of
/// RGB traffic) and would spread the shared last-level cache across more
/// cores. Four is the smallest fan-out that removes the single-core burst.
pub(super) const MAX_HELPER_THREADS: usize = 4;

fn available_threads() -> usize {
    static AVAILABLE: OnceLock<usize> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
    })
}

/// How many helpers should split `items` units of work.
///
/// `min_items_per_thread` is the smallest amount of work that pays for a
/// thread wake-up; below it the pass stays serial. The result is also bounded
/// by the machine and by [`MAX_HELPER_THREADS`].
pub(super) fn helper_threads(items: u64, min_items_per_thread: u64) -> usize {
    let per_thread = min_items_per_thread.max(1);
    if items < per_thread {
        return 1;
    }
    cap_threads(items / per_thread)
}

/// [`helper_threads`] for a caller that already knows how many ways it wants
/// to split the work.
pub(super) fn cap_threads(desired: u64) -> usize {
    available_threads()
        .min(MAX_HELPER_THREADS)
        .min(desired.max(1) as usize)
        .max(1)
}
