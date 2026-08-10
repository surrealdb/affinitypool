//! Regression tests for the self-spawn wake-up handshake.
//!
//! A worker that pushes into its own deque is normally also the
//! consumer, but it can block before returning to its pop loop — a
//! polled-then-dropped [`affinitypool::SpawnFuture`] runs
//! `block_on_cancel` -> `thread::park()`, waiting for the very runnable
//! it just queued. At that point only a peer steal can make progress,
//! so the self-spawn path has to wake a parked peer. It previously did
//! not, and the pool hung.
#![cfg(not(loom))]

use affinitypool::Threadpool;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc;
use std::task::{Context, Waker};
use std::thread;
use std::time::Duration;

/// Poll a `SpawnFuture` exactly once (which schedules it) and then drop
/// it, from inside a worker of the same pool. Polling is what puts the
/// runnable on the queue; dropping then blocks this worker until that
/// runnable stops. With every peer parked, only a wake from the
/// self-spawn path can free it.
///
/// Run on a helper thread with a bounded `recv_timeout` so a regression
/// fails the test instead of hanging CI.
#[test]
fn self_spawn_wakes_parked_peer_when_spawner_blocks() {
	let (done_tx, done_rx) = mpsc::channel::<()>();

	let worker = thread::spawn(move || {
		let pool = Arc::new(Threadpool::new(2));
		let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

		// Warm both workers, then let them settle into a park so the
		// peer really is asleep when the self-spawn happens.
		rt.block_on(async {
			let _ = pool.spawn(|| 0u32).await;
		});
		thread::sleep(Duration::from_millis(100));

		let inner = pool.clone();
		// A foreign push: wakes exactly one worker to run this closure.
		let outer = pool.spawn(move || {
			// On a worker of `inner` now, so this takes the self-spawn
			// path on first poll: the runnable lands in *this* worker's
			// own deque.
			// SAFETY: polled and dropped within this closure, never
			// leaked, and the closure borrows nothing non-'static.
			let fut = unsafe { inner.spawn_local(|| 42u32) };
			let mut fut = Box::pin(fut);
			let mut cx = Context::from_waker(Waker::noop());
			let _ = fut.as_mut().poll(&mut cx);
			// Dropping while scheduled blocks this worker until the
			// runnable it just queued has stopped.
			drop(fut);
		});

		rt.block_on(outer);
		let _ = done_tx.send(());
	});

	assert!(
		done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
		"deadlock: a self-spawned runnable was stranded in the deque of a \
		 worker that then blocked, while its peer stayed parked"
	);
	worker.join().unwrap();
}

/// The same shape, but the self-spawned work is awaited normally rather
/// than cancelled. This is the common case and must keep working: the
/// spawning worker consumes its own deque.
#[test]
fn self_spawn_still_runs_on_spawning_worker() {
	let (done_tx, done_rx) = mpsc::channel::<u32>();

	let worker = thread::spawn(move || {
		let pool = Arc::new(Threadpool::new(2));
		let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
		let inner = pool.clone();

		type Inner = Pin<Box<dyn Future<Output = u32> + Send>>;
		let (tx, rx) = mpsc::channel::<Inner>();

		let outer = pool.spawn(move || {
			// Self-spawn, then hand the future out to be awaited off
			// the worker so this closure can return normally.
			let f: Inner = Box::pin(inner.spawn(|| 42u32));
			tx.send(f).unwrap();
		});

		rt.block_on(async {
			outer.await;
			let f = rx.recv_timeout(Duration::from_secs(5)).expect("no inner future");
			let _ = done_tx.send(f.await);
		});
	});

	assert_eq!(
		done_rx.recv_timeout(Duration::from_secs(10)).ok(),
		Some(42),
		"self-spawned runnable did not complete"
	);
	worker.join().unwrap();
}
