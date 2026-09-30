//! Returning memory to the OS once threads go idle.
//!
//! Kiki's binary uses mimalloc as its global allocator (see `src/main.rs`).
//! mimalloc gives every thread a heap of its own, and a page freed in it —
//! including by *another* thread, as happens whenever a tokio task moves
//! between workers — only leaves that heap when the owning thread next
//! calls into the allocator. Even then, freed memory stays committed until
//! mimalloc's purge delay (a second, by default) has passed *and* some
//! thread calls in again. A runtime that goes idle after a burst of work,
//! such as a feed refresh, therefore holds on to everything the burst
//! freed for as long as it stays idle: in the feed fetcher that was around
//! 100 MiB after every refresh.
//!
//! [`release_on_park`] fixes both halves: each worker thread hands its
//! empty pages back as it parks, and a background thread purges them — for
//! the whole process — a few seconds later.

use std::sync::Once;
use std::time::Duration;

/// How often the background thread purges freed memory.
///
/// Well past mimalloc's purge delay, so memory freed in a burst is
/// returned within this long of the process going idle, while waking an
/// otherwise idle process only rarely.
const PURGE_INTERVAL: Duration = Duration::from_secs(5);

/// Hand the calling thread's empty pages back to mimalloc's shared arenas.
///
/// This is cheap — a few microseconds — so it is safe to call on every
/// park. It does not decommit anything by itself: mimalloc schedules the
/// freed pages to be purged after its purge delay, which the thread
/// started by [`release_on_park`] carries out.
///
/// It is harmless when mimalloc is not the global allocator, as in the
/// library's tests: mimalloc's heaps are then empty, and there is nothing
/// to collect.
///
/// # Examples
///
/// ```
/// // After a burst of work, before the thread sleeps:
/// kiki_rss::memory::release_thread_memory();
/// ```
pub fn release_thread_memory() {
    // SAFETY: `mi_collect` takes no pointers and may be called from any
    // thread at any time; it only operates on the calling thread's heap
    // and mimalloc's own, internally synchronized, arenas.
    unsafe { libmimalloc_sys::mi_collect(false) };
}

/// Start, once per process, a thread that returns memory freed by any
/// thread to the OS every [`PURGE_INTERVAL`].
///
/// A forced collection purges every page mimalloc has scheduled for
/// purging, in all of its arenas, not just those of the calling thread.
/// Doing that here rather than as workers park keeps the much more
/// expensive forced collection (around 100µs, against a few for
/// [`release_thread_memory`]) off the runtime's threads, and makes sure it
/// happens even if nothing else in the process runs again.
fn start_purger() {
    static PURGER: Once = Once::new();
    PURGER.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("memory-purger".into())
            .spawn(|| loop {
                std::thread::sleep(PURGE_INTERVAL);
                // SAFETY: as in `release_thread_memory`.
                unsafe { libmimalloc_sys::mi_collect(true) };
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the memory purger thread");
        }
    });
}

/// Configure a tokio runtime to return memory to the OS when it goes idle.
///
/// Each of the runtime's worker threads calls [`release_thread_memory`] as
/// it parks, and a background thread, started with the first runtime
/// configured this way, purges what they released. Use this for any
/// long-lived runtime whose workers allocate in bursts.
///
/// # Examples
///
/// ```
/// let mut builder = tokio::runtime::Builder::new_multi_thread();
/// let rt = kiki_rss::memory::release_on_park(&mut builder)
///     .enable_all()
///     .build()?;
/// rt.block_on(async { /* ... */ });
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn release_on_park(builder: &mut tokio::runtime::Builder) -> &mut tokio::runtime::Builder {
    start_purger();
    builder.on_thread_park(release_thread_memory)
}
