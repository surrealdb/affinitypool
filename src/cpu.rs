//! Thread-local cached "current CPU" lookup for shard routing.
//!
//! Producers call [`current_cpu`] to pick a shard. The value is cached
//! in thread-local state and only refreshed every [`REFRESH_EVERY`]
//! calls — [`libc::sched_getcpu`] is a vDSO entry on modern Linux
//! (≈5 ns) and `GetCurrentProcessorNumber` on Windows is similar, but
//! the cached lookup is still a few cycles faster and avoids any
//! syscall on platforms without a direct API.
//!
//! Platforms without a direct "what CPU am I on" call fall back to
//! hashing the thread ID. That loses geographical accuracy but
//! preserves the goal: a given producer thread consistently routes
//! to the same shard, which is what gets you cache locality for
//! long-lived producers.

use std::cell::Cell;

/// Refresh cadence. Picked empirically: small enough that producer
/// migrations don't strand a sender on a stale shard for long, large
/// enough that the per-call overhead is amortised into the noise.
const REFRESH_EVERY: u32 = 64;

thread_local! {
	/// `(cached_cpu, age)`. `age == REFRESH_EVERY` (or above) signals
	/// "refresh on next call". Initialised so the very first call
	/// always queries — that way we don't pay the cost of a query on
	/// threads that never reach `current_cpu`.
	static CACHED: Cell<(usize, u32)> = const { Cell::new((0, REFRESH_EVERY)) };
}

/// Return a stable hint for this thread's current CPU. The value is
/// refreshed every [`REFRESH_EVERY`] calls; in between, callers see
/// the cached result.
#[inline]
pub(crate) fn current_cpu() -> usize {
	CACHED.with(|c| {
		let (cpu, age) = c.get();
		if age >= REFRESH_EVERY {
			let fresh = query_cpu();
			c.set((fresh, 0));
			fresh
		} else {
			c.set((cpu, age + 1));
			cpu
		}
	})
}

#[cfg(target_os = "linux")]
#[inline]
fn query_cpu() -> usize {
	// vDSO on modern Linux; ~5 ns.
	let cpu = unsafe { libc::sched_getcpu() };
	if cpu < 0 {
		0
	} else {
		cpu as usize
	}
}

#[cfg(target_os = "windows")]
#[inline]
fn query_cpu() -> usize {
	use winapi::um::processthreadsapi::GetCurrentProcessorNumber;
	unsafe { GetCurrentProcessorNumber() as usize }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[inline]
fn query_cpu() -> usize {
	// Fallback: hash the thread ID. Stable per producer thread, so we
	// still get producer→shard affinity (just not CPU-locality based).
	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};
	let mut h = DefaultHasher::new();
	std::thread::current().id().hash(&mut h);
	h.finish() as usize
}
