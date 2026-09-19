//! Allocator memory-release hook.
//!
//! jemalloc keeps freed pages attached to the process, so the multi-hundred-MB transients a large session stages linger after they drop.
//! The pager library cannot reference jemalloc, so the composition-root binary installs an arena-purge hook here.
//! The library calls [`release_retained_memory`] after heavy transient drops to return the pages to the OS.
//! Absent a hook (tests, non-jemalloc builds) everything is inert.
//!
//! Purges are edge-triggered on an actual drop, never per frame; draw/tick-path cliffs defer to the post-flush gap so a purge never stalls the frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static RELEASE_HOOK: OnceLock<fn()> = OnceLock::new();

/// Install the allocator release hook. Idempotent; first caller wins.
/// Called once by the composition-root binary before the app runs.
pub fn install_release_hook(hook: fn()) {
    let _ = RELEASE_HOOK.set(hook);
}

/// Ask the allocator to return freed-but-retained pages to the OS. From draw/tick paths use
/// [`request_release_after_draw`] instead.
pub(crate) fn release_retained_memory(reason: &'static str) {
    let hook = RELEASE_HOOK.get();
    // Skip gauge sampling entirely when tracing is off (`GROK_MEMTRACE=0` or no sink): a disabled trace must add zero syscalls to purges
    let trace = crate::memory_trace::is_active();
    let before = if trace {
        // Same gauge precedence as the trace's threshold logic: physical footprint where available (macOS), else RSS (Linux)
        // Purge deltas are then computable on every platform
        let mem = crate::memory_trace::sample_process_memory();
        mem.footprint_bytes.or(mem.rss_bytes)
    } else {
        None
    };
    let started = std::time::Instant::now();
    if let Some(hook) = hook {
        hook();
    }
    if trace {
        crate::memory_trace::record_purge(reason, hook.is_some(), before, started.elapsed());
    }
}

/// Deferred-release request flag, drained after the frame flush.
/// `AtomicBool` rather than a thread-local: both sides are main-thread today, but the flag must not silently drop a request if that ever changes.
static RELEASE_AFTER_DRAW: AtomicBool = AtomicBool::new(false);

/// Memory-cliff tag for the pending deferred request. the `"post-draw"` default only shows up if a drain ever races
/// a set without one.
static DEFER_REASON: Mutex<&'static str> = Mutex::new("post-draw");

/// Request a purge to run right after the current frame flushes, drained by [`run_deferred_release`] at the end of `AppView::draw`.
/// The purge is tagged with `reason` for the trace (see [`release_retained_memory`]).
/// Use it for memory cliffs hit *inside* the draw/tick path, e.g. inline video stopped because it scrolled off screen.
pub(crate) fn request_release_after_draw(reason: &'static str) {
    if let Ok(mut r) = DEFER_REASON.lock() {
        *r = reason;
    }
    RELEASE_AFTER_DRAW.store(true, Ordering::Relaxed);
}

/// Drain a pending [`request_release_after_draw`], if any.
/// Called once at the end of `AppView::draw`, after the terminal buffer flush.
/// The purge cost then lands in the idle gap between frames instead of inside one.
pub(crate) fn run_deferred_release() {
    if RELEASE_AFTER_DRAW.swap(false, Ordering::Relaxed) {
        // Recover the stored reason even if the lock was poisoned; a request always writes a real tag before setting the flag
        let reason = match DEFER_REASON.lock() {
            Ok(r) => *r,
            Err(poison) => *poison.into_inner(),
        };
        release_retained_memory(reason);
    }
}

/// Test support: a counting release hook with a **per-thread** counter.
/// The real `RELEASE_HOOK` is a process-global `OnceLock`, but dispatch/view code always calls [`release_retained_memory`] on the calling thread.
/// A thread-local count lets parallel `cargo test` threads assert both "released" and "must not release" deltas without cross-test interference.
#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::Cell;

    thread_local! {
        static CALLS: Cell<usize> = const { Cell::new(0) };
    }

    fn counting_hook() {
        CALLS.with(|c| c.set(c.get() + 1));
    }

    /// Install the counting hook (idempotent; first caller wins, and every test installs the same `fn`, so ordering does not matter).
    pub(crate) fn install_counting_hook() {
        super::install_release_hook(counting_hook);
    }

    /// Number of releases observed **on this thread**.
    pub(crate) fn calls() -> usize {
        CALLS.with(|c| c.get())
    }
}

#[cfg(test)]
#[path = "memory_release_tests.rs"]
mod tests;
