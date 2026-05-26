pub mod affinity;
mod builder;
mod cpu;
mod data;
mod error;
mod global;
mod local;
mod queue;
mod sentry;

pub use crate::builder::Builder;
pub use crate::error::Error;
pub use crate::local::SpawnFuture;

use crate::data::Data;
use crate::global::THREADPOOL;
use crate::queue::Queue;
use crate::sentry::Sentry;
use async_task::{Builder as TaskBuilder, Runnable};
use parking_lot::Mutex;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Maximum number of worker threads allowed in a thread pool.
pub const MAX_THREADS: usize = 512;

/// Queue a new command for execution on the global threadpool.
///
/// If no global threadpool has been created, then this function
/// runs the provided closure immediately, without passing it to
/// a blocking threadpool.
pub async fn spawn<F, R>(func: F) -> R
where
	F: FnOnce() -> R + Send + 'static,
	R: Send + 'static,
{
	if let Some(threadpool) = THREADPOOL.get() {
		threadpool.spawn(func).await
	} else {
		func()
	}
}

/// Queue a new command for execution on the global threadpool.
/// The future of this function will block the current thread
/// if it is not fully awaited and driven to completion.
///
/// If no global threadpool has been created, then this function
/// runs the provided closure immediately, without passing it to
/// a blocking threadpool.
pub async fn spawn_local<'pool, F, R>(func: F) -> R
where
	F: FnOnce() -> R,
	F: Send + 'pool,
	R: Send + 'pool,
{
	if let Some(threadpool) = THREADPOOL.get() {
		threadpool.spawn_local(func).await
	} else {
		func()
	}
}

pub struct Threadpool {
	pub(crate) data: Arc<Data>,
}

impl Default for Threadpool {
	fn default() -> Self {
		Threadpool::new(num_cpus::get())
	}
}

impl Threadpool {
	/// Create a new thread pool.
	pub fn new(workers: usize) -> Self {
		// Validate worker count.
		let workers = workers.clamp(1, MAX_THREADS);
		// Create the threadpool shared data.
		let data = Arc::new(Data {
			name: None,
			stack_size: None,
			num_threads: AtomicUsize::new(workers),
			thread_count: AtomicUsize::new(0),
			queue: Arc::new(Queue::new(workers)),
			thread_handles: Mutex::new(Vec::new()),
		});
		// Spawn the desired number of workers.
		for index in 0..workers {
			Self::spin_up(None, data.clone(), index);
		}
		// Return the new threadpool.
		Threadpool {
			data,
		}
	}

	/// Queue a new command for execution on this pool.
	///
	/// The closure is scheduled **immediately** — by the time this
	/// function returns, the task is already on the worker queue and
	/// may already be running. The returned future resolves to the
	/// closure's return value. Dropping the future before completion
	/// cancels the task: a queued-but-unrun task is dropped without
	/// running; a currently-running task completes but its result is
	/// discarded.
	///
	/// Returning a future (rather than being an `async fn`) means
	/// pipelined producers — e.g. `let h1 = pool.spawn(a); let h2 =
	/// pool.spawn(b)` — start both `a` and `b` on workers
	/// concurrently, before any `.await`. This matches the semantics
	/// of `tokio::task::spawn_blocking`.
	pub fn spawn<F, R>(&self, func: F) -> impl Future<Output = R> + Send + 'static
	where
		F: FnOnce() -> R,
		F: Send + 'static,
		R: Send + 'static,
	{
		// Clone the Arc<Queue> into the schedule closure. The closure
		// is `Fn(Runnable) + Send + Sync + 'static`, which is what
		// async-task requires.
		let queue = self.data.queue.clone();
		let schedule = move |runnable: Runnable| queue.push(runnable);
		// Wrap the closure in `catch_unwind` so a panic inside the
		// closure is reified into a `Result::Err(payload)` on the
		// worker side instead of propagating up through
		// `Runnable::run` and unwinding the worker thread. The
		// awaiter then `resume_unwind`s the payload, matching the
		// previous behaviour where panics surfaced on the `.await`
		// side, not the worker side. Without this wrapper, a
		// panicking closure would tear down the worker and force the
		// sentry to respawn a thread on every panic.
		let (runnable, task) = TaskBuilder::new().spawn(
			move |()| async move { std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)) },
			schedule,
		);
		// Push the runnable onto the queue right now so a worker can
		// start processing it before the caller awaits.
		runnable.schedule();
		// Returning the future to the caller — dropping it cancels.
		// The outer async block re-raises any captured panic, so the
		// future's output type stays `R` (not `Result<R, _>`).
		async move {
			match task.await {
				Ok(value) => value,
				Err(payload) => std::panic::resume_unwind(payload),
			}
		}
	}

	/// Queue a new command for execution on this pool with access to
	/// the local variables.
	///
	/// Unlike [`spawn`](Self::spawn), the closure is scheduled lazily
	/// on first poll of the returned future. Dropping a
	/// [`SpawnFuture`] without ever polling it never queues the
	/// runnable to a worker, so the drop returns immediately. Once
	/// polled, dropping the future cancels the task; if the worker is
	/// currently running the closure, the dropping thread is parked
	/// until the worker has finished, so closure borrows of `'pool`
	/// data cannot dangle.
	///
	/// # Drop-blocking caveats
	///
	/// The `Drop` impl on a polled `SpawnFuture` blocks the dropping
	/// thread synchronously. Two consequences are worth keeping in
	/// mind:
	///
	/// 1. **Async-context footgun.** Dropping a polled `SpawnFuture`
	///    from inside an async task — for example by holding it in a
	///    `select!`/`tokio::join!` branch that loses — blocks the
	///    underlying async runtime's worker thread for the duration
	///    of the closure. On a multi-thread runtime this just stalls
	///    one worker; on a current-thread runtime it can deadlock the
	///    whole runtime if the cancellation depends on other tasks
	///    on the same executor.
	/// 2. **Lazy scheduling is intentional.** A `SpawnFuture`
	///    constructed and dropped without polling never queues the
	///    runnable, so a 1-worker pool can safely do
	///    `let _ = pool.spawn_local(|| …);` from inside its own
	///    worker. Polling once and *then* dropping still queues and
	///    cancels normally.
	pub fn spawn_local<'pool, F, R>(&'pool self, func: F) -> SpawnFuture<'pool, R>
	where
		F: FnOnce() -> R,
		F: Send + 'pool,
		R: Send + 'pool,
	{
		let queue = self.data.queue.clone();
		let schedule = move |runnable: Runnable| queue.push(runnable);
		// Same `catch_unwind` wrapper rationale as `spawn` — keep
		// panics off the worker thread, surface them on the awaiter.
		// SAFETY: `async_task::Builder::spawn_unchecked` lifts the
		// `'static` bound on the future. Soundness is preserved by:
		//   1. `SpawnFuture<'pool, R>` carries the `'pool` borrow, so
		//      the future cannot outlive the pool.
		//   2. `SpawnFuture::drop` blocks (via `Task::cancel().await`)
		//      until the runnable has stopped, so the closure's
		//      `'pool` borrows are not accessed after they expire.
		//   3. The same `mem::forget` caveat as the previous
		//      implementation applies — see `local.rs` module docs.
		let (runnable, task) = unsafe {
			TaskBuilder::new().spawn_unchecked(
				move |()| async move { std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)) },
				schedule,
			)
		};
		// Defer `runnable.schedule()` to first poll. Eager scheduling
		// here would deadlock a 1-worker pool whenever the only worker
		// constructs and drops a `SpawnFuture` without polling: the
		// worker would block in `Drop` waiting for itself to consume
		// the runnable it just queued. The runnable is handed to
		// `SpawnFuture`, which schedules it from its `poll` and drops
		// it cleanly if the caller never polls.
		SpawnFuture::new(runnable, task)
	}

	/// Set this threadpool as the global threadpool.
	pub fn build_global(self) -> Result<(), Error> {
		// Check if the threadpool has been created.
		if THREADPOOL.get().is_some() {
			return Err(Error::GlobalThreadpoolExists);
		}
		// Set this threadpool as the global threadpool.
		THREADPOOL.get_or_init(|| self);
		// Global threadpool was created successfully.
		Ok(())
	}

	/// Get the total number of worker threads in this pool.
	pub fn thread_count(&self) -> usize {
		self.data.thread_count.load(Ordering::Relaxed)
	}

	/// Get the specified number of threads for this pool.
	pub fn num_threads(&self) -> usize {
		self.data.num_threads.load(Ordering::Relaxed)
	}

	/// Spin up a new worker thread in this pool.
	#[cfg(not(target_family = "wasm"))]
	pub(crate) fn spin_up(coreid: Option<usize>, data: Arc<Data>, index: usize) {
		// Create a new thread builder.
		let mut builder = std::thread::Builder::new();
		// Assign a name to the thread if specified.
		if let Some(ref name) = data.name {
			builder = builder.name(name.clone());
		}
		// Assign a stack size to the thread if specified.
		if let Some(stack_size) = data.stack_size {
			builder = builder.stack_size(stack_size);
		}
		// Increase the thread count counter.
		data.thread_count.fetch_add(1, Ordering::Relaxed);
		// Create a new sentry watcher.
		let sentry = Sentry::new(coreid, index, Arc::downgrade(&data));
		// Clone the queue handle for the worker loop.
		let queue = data.queue.clone();
		// Clone data for handle storage.
		let data_clone = data.clone();
		// Spawn a new worker thread.
		let handle = builder.spawn(move || {
			// Assign this thread to a core.
			if let Some(coreid) = coreid {
				affinity::set_for_current(coreid.into());
			}
			// Register this worker with the queue: creates a fresh
			// per-worker deque and installs its `Stealer` in slot
			// `index`. On panic-respawn, the replacement thread
			// re-runs this and overwrites the slot.
			let ctx = queue.register_worker(index);
			// Mark this thread as a worker for `queue` so that
			// `pool.spawn(...)` calls from inside `runnable.run()`
			// route directly into the local deque, skipping the
			// shared injector. Dropped at the end of this scope,
			// clearing the thread-local registration before the
			// `WorkerContext` itself goes out of scope.
			let _worker_scope = queue.enter_worker_scope(&ctx);
			// Worker loop: pop a runnable, run it, repeat. The
			// worker hands its `ctx` so the queue can drain the
			// local deque first, then steal a batch from the
			// preferred injector, then scan peers, then park.
			while let Some(runnable) = queue.pop_blocking(&ctx) {
				runnable.run();
			}
			// Clean exit — cancel the sentry so its Drop does not
			// trigger a respawn.
			sentry.cancel();
		});
		// Store the thread handle if spawning succeeded.
		if let Ok(handle) = handle {
			data_clone.thread_handles.lock().push(handle);
		}
	}

	/// WASM stub — affinity and worker threads are not supported.
	#[cfg(target_family = "wasm")]
	pub(crate) fn spin_up(_coreid: Option<usize>, _data: Arc<Data>, _index: usize) {
		// Do nothing in WASM.
	}
}

impl Drop for Threadpool {
	fn drop(&mut self) {
		// Signal all workers to shut down. The Condvar wakes everyone
		// blocked in `pop_blocking`; once the queue drains, each worker
		// observes `None` and exits.
		self.data.queue.shutdown();
		// Take all of the thread handles for joining.
		let handles = self.data.thread_handles.lock().drain(..).collect::<Vec<_>>();
		// Join all worker threads.
		for handle in handles {
			let _ = handle.join();
		}
		// Decrement thread count to 0.
		self.data.thread_count.store(0, Ordering::Relaxed);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::pin::Pin;
	use std::sync::mpsc;
	use std::time::Duration;

	/// Confirms the worker self-spawn fast path actually engaged
	/// (not just that the work eventually ran via the fallback).
	/// Reads the test-only `Queue::foreign_pushes` counter: every
	/// `Threadpool::spawn` that takes the foreign-producer path
	/// bumps it. A self-spawn from inside a worker closure should
	/// NOT bump it.
	///
	/// Uses a 1-worker pool so the foreign-push branch hits the
	/// `mask == 0` fast path and never calls `cpu::current_cpu()`
	/// (`sched_getcpu` isn't supported under miri).
	///
	/// The inner `JoinHandle` is sent to the test thread via an
	/// mpsc channel and awaited there, instead of `mem::forget`-ed
	/// — keeps the test leak-free under miri while still letting
	/// the inner closure run to completion on the worker.
	#[tokio::test]
	async fn self_spawn_does_not_increment_foreign_push_counter() {
		type InnerFuture = Pin<Box<dyn Future<Output = u32> + Send + 'static>>;

		let pool = Arc::new(Threadpool::new(1));

		// Drain any startup noise from constructing the pool.
		pool.data.queue.foreign_pushes.store(0, Ordering::Relaxed);

		let pool_clone = pool.clone();
		let (handle_tx, handle_rx) = mpsc::channel::<InnerFuture>();

		// The outer spawn is a foreign push (we're calling from
		// the tokio runtime thread, not a worker). It bumps the
		// counter by 1.
		pool.spawn(move || {
			// We're now on a worker thread for `pool`. The
			// self-spawn fast path should engage: this push goes
			// directly into the worker's local deque, NOT through
			// the foreign-producer path.
			let inner: InnerFuture = Box::pin(pool_clone.spawn(|| 42u32));
			handle_tx.send(inner).unwrap();
		})
		.await;

		// Receive the inner future on the test thread and await
		// it. The inner closure runs on the worker (via the local
		// deque if the fast path engaged), and awaiting yields
		// its return value.
		let inner = handle_rx
			.recv_timeout(Duration::from_secs(5))
			.expect("worker didn't send inner handle");
		let v = inner.await;
		assert_eq!(v, 42);

		// Only the outer spawn should have hit the foreign-push
		// path; the inner self-spawn must have taken the fast
		// path.
		let foreign_count = pool.data.queue.foreign_pushes.load(Ordering::Relaxed);
		assert_eq!(
			foreign_count, 1,
			"expected exactly one foreign push (the outer); got {foreign_count} — self-spawn fast path didn't engage"
		);
	}
}
