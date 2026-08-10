//! Thread-local cached shard hint for producer→shard routing.
//!
//! Producers call [`current_shard_hint`] to pick an injector shard. The
//! hint is derived from the producer thread's ID, hashed once and cached
//! for the lifetime of the thread — a thread's ID never changes, so there
//! is nothing to refresh. The goal is purely that a given producer thread
//! routes consistently to the same shard, which keeps that producer's
//! traffic isolated to one injector and minimises cross-shard contention
//! when many producers run concurrently (the dominant workload).
//!
//! This previously queried the running CPU (`sched_getcpu` on Linux,
//! `GetCurrentProcessorNumber` on Windows) for geographic locality, but
//! that bought little over thread-ID stickiness — workers steal across
//! shards regardless — while costing a syscall/vDSO call and a platform
//! dependency (`libc`/`winapi`). Hashing the thread ID is stable,
//! allocation-free, identical on every target, and works under miri
//! (which does not support `sched_getcpu`).

use std::cell::Cell;

thread_local! {
	/// Cached shard hint for this thread, computed lazily on first use.
	/// `None` until the first call; a thread's ID is immutable, so the
	/// value never needs refreshing once set. Initialised lazily so
	/// threads that never produce work pay nothing.
	static SHARD_HINT: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Return a stable shard hint for the calling thread. Derived once from
/// the thread ID and cached for the thread's lifetime.
#[inline]
pub(crate) fn current_shard_hint() -> usize {
	SHARD_HINT.with(|c| match c.get() {
		Some(h) => h,
		None => {
			let h = hash_thread_id();
			c.set(Some(h));
			h
		}
	})
}

/// Hash the current thread's `ThreadId` into a `usize`. Stable per
/// thread, so a given producer always routes to the same shard.
#[inline]
fn hash_thread_id() -> usize {
	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};
	let mut h = DefaultHasher::new();
	std::thread::current().id().hash(&mut h);
	h.finish() as usize
}
