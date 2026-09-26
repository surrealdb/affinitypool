//! Thread-local cached shard routing for producer→shard assignment.
//!
//! Producers call [`route_shard`] to pick an injector shard. The route is
//! derived from the producer thread's ID, hashed once and cached for the
//! lifetime of the thread. When consecutive pushes to the same shard exceed
//! [`SPILL_THRESHOLD`], routing rotates across shards to distribute single-producer
//! fan-out evenly across workers.

use std::cell::Cell;

/// Consecutive pushes to the same preferred shard before producer-
/// side spill kicks in. Multi-producer workloads rarely reach it
/// (their pushes interleave, resetting the counter), while a
/// single-producer fan-out trips it quickly so the work spreads
/// across shards before the other workers give up and park.
pub(crate) const SPILL_THRESHOLD: u32 = 8;

#[derive(Clone, Copy)]
struct ProducerRoute {
	shard_hint: usize,
	last_preferred: usize,
	consecutive: u32,
}

thread_local! {
	static ROUTE: Cell<Option<ProducerRoute>> = const { Cell::new(None) };
}

/// Route a push to an injector shard given the queue's `mask`.
/// Derives and caches the thread ID hash on first use, tracks consecutive
/// pushes to the preferred shard, and applies spill rotation in a single
/// thread-local access.
#[inline]
pub(crate) fn route_shard(mask: usize) -> usize {
	ROUTE.with(|cell| {
		let mut route = match cell.get() {
			Some(r) => r,
			None => ProducerRoute {
				shard_hint: hash_thread_id(),
				last_preferred: usize::MAX,
				consecutive: 0,
			},
		};
		let preferred = route.shard_hint & mask;
		let new_count = if route.last_preferred == preferred {
			route.consecutive.saturating_add(1)
		} else {
			1
		};
		route.last_preferred = preferred;
		route.consecutive = new_count;
		cell.set(Some(route));

		if new_count <= SPILL_THRESHOLD {
			preferred
		} else {
			(preferred + (new_count - SPILL_THRESHOLD) as usize) & mask
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
