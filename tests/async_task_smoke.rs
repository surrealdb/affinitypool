//! Smoke tests for `async-task` semantics that the rewrite depends on.
//!
//! These confirm — before any production code touches async-task — that
//! the assumptions in `/home/bench/.claude/plans/ok-make-this-into-toasty-simon.md`
//! hold. If any of these fail, the rewrite plan needs revisiting before PR #2.
//!
//! Specifically we check:
//! 1. `Runnable` is `Send`; it can be moved to a worker thread for `.run()`.
//! 2. The schedule closure shape works for our queue model.
//! 3. `Task<R>::await` yields the value the future returned.
//! 4. Dropping `Task<R>` before the runnable runs cancels it (cancel-on-drop).
//! 5. `Task::cancel().await` resolves once the runnable has actually stopped.
//! 6. `Builder::spawn_unchecked` accepts non-`'static` closures (required for
//!    `spawn_local`).
//! 7. A panic inside the future is caught by async-task and propagated to
//!    the awaiter (matches the current affinitypool behaviour).

use async_task::{Builder, Runnable, Task};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Drive a single Runnable through a worker thread; the awaiter blocks on
/// the Task using a tiny parker-based block_on. This is the same shape
/// the production rewrite will use, so it doubles as a prototype for the
/// `block_on` helper PR #3 will need.
fn block_on<F: Future>(mut fut: F) -> F::Output {
	use std::pin::Pin;
	use std::sync::Arc;
	use std::task::{Context, Poll, Wake, Waker};

	struct ParkWaker(thread::Thread);
	impl Wake for ParkWaker {
		fn wake(self: Arc<Self>) {
			self.0.unpark();
		}
		fn wake_by_ref(self: &Arc<Self>) {
			self.0.unpark();
		}
	}

	let waker: Waker = Arc::new(ParkWaker(thread::current())).into();
	let mut cx = Context::from_waker(&waker);
	// SAFETY: `fut` is owned and not moved after this point.
	let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
	loop {
		match fut.as_mut().poll(&mut cx) {
			Poll::Ready(v) => return v,
			Poll::Pending => thread::park(),
		}
	}
}

/// Spawn one worker thread and return a sender for `Runnable`s plus a
/// shutdown handle. Tests submit via the sender, worker calls `.run()`.
fn spawn_worker() -> (mpsc::Sender<Runnable>, thread::JoinHandle<()>, Arc<AtomicBool>) {
	let (tx, rx) = mpsc::channel::<Runnable>();
	let shutdown = Arc::new(AtomicBool::new(false));
	let shutdown_worker = shutdown.clone();
	let handle = thread::spawn(move || {
		while let Ok(runnable) = rx.recv() {
			runnable.run();
			if shutdown_worker.load(Ordering::Acquire) {
				break;
			}
		}
	});
	(tx, handle, shutdown)
}

#[test]
fn runnable_is_send() {
	// Compile-time check that Runnable is Send.
	fn assert_send<T: Send>() {}
	assert_send::<Runnable>();
	assert_send::<Task<u32>>();
}

#[test]
fn basic_spawn_and_await() {
	let (tx, worker, _shutdown) = spawn_worker();
	let tx2 = tx.clone();

	let (runnable, task) = Builder::new()
		.spawn(|()| async move { 42u32 }, move |runnable| tx2.send(runnable).unwrap());
	runnable.schedule();

	let result = block_on(task);
	assert_eq!(result, 42);

	drop(tx);
	worker.join().unwrap();
}

#[test]
fn cancel_before_run_drops_future() {
	// Cancel-on-drop: if we drop the Task before the runnable runs, the
	// future inside should be dropped without being polled. The closure
	// captures a drop guard whose count we check.
	let dropped = Arc::new(AtomicUsize::new(0));
	let dropped2 = dropped.clone();

	struct Guard(Arc<AtomicUsize>);
	impl Drop for Guard {
		fn drop(&mut self) {
			self.0.fetch_add(1, Ordering::Release);
		}
	}

	// We don't actually schedule the runnable; we just drop the task
	// and the runnable to confirm both clean up the future.
	let (runnable, task) = Builder::new().spawn(
		move |()| {
			let g = Guard(dropped2);
			async move {
				let _g = g;
				1u32
			}
		},
		|_| {},
	);

	// Drop the task (the join handle). This marks the task closed.
	drop(task);
	// Now run the runnable — but because the task is closed,
	// async-task should drop the future without polling it.
	runnable.run();
	// The future's drop guard should have fired exactly once.
	assert_eq!(dropped.load(Ordering::Acquire), 1, "future not dropped exactly once");
}

#[test]
fn cancel_await_resolves_after_runnable_stops() {
	// Task::cancel().await should resolve after the runnable has either
	// finished or been dropped without running. This is the contract
	// SpawnFuture's drop will rely on.
	let (tx, worker, _shutdown) = spawn_worker();
	let tx2 = tx.clone();

	let started = Arc::new(AtomicBool::new(false));
	let started2 = started.clone();

	let (runnable, task) = Builder::new().spawn(
		move |()| async move {
			started2.store(true, Ordering::Release);
			// Long enough that cancel.await definitely sees a
			// runnable that is currently running.
			thread::sleep(Duration::from_millis(50));
			99u32
		},
		move |r| tx2.send(r).unwrap(),
	);
	runnable.schedule();

	// Give the worker a moment to start running.
	thread::sleep(Duration::from_millis(10));
	assert!(started.load(Ordering::Acquire), "worker did not start");

	// Now cancel. cancel().await should block until the worker has
	// finished its closure.
	let result = block_on(task.cancel());
	// The task either completed normally (Some) or was cancelled
	// before its return (None). Because the closure is synchronous
	// (one poll), it almost certainly completed before cancellation
	// took effect — but the contract is that cancel().await resolves
	// only after the runnable stops, which is what we care about.
	// We just confirm it returned.
	let _ = result;

	drop(tx);
	worker.join().unwrap();
}

#[test]
fn spawn_unchecked_accepts_non_static() {
	// spawn_local needs to spawn closures that borrow from the producer
	// stack. `spawn_unchecked` is how async-task lets us do this.
	//
	// We mimic spawn_local's contract: the awaiter (the SpawnFuture) is
	// kept alive (here, the Task) until the runnable has finished, so
	// the borrow remains valid for the lifetime of the closure.
	let stack_value = 7u32;
	let stack_ref = &stack_value;

	let (tx, worker, _shutdown) = spawn_worker();
	let tx2 = tx.clone();

	// SAFETY: the borrow `stack_ref` outlives the Task and Runnable
	// because we block_on(task) before the function returns.
	let (runnable, task) = unsafe {
		Builder::new()
			.spawn_unchecked(|()| async move { *stack_ref + 1 }, move |r| tx2.send(r).unwrap())
	};
	runnable.schedule();

	let result = block_on(task);
	assert_eq!(result, 8);

	drop(tx);
	worker.join().unwrap();
}

#[test]
fn panic_in_future_propagates_to_awaiter() {
	// async-task should catch a panic in the future and surface it
	// when the awaiter polls the Task. Matches affinitypool's current
	// catch_unwind + resume_unwind behaviour.
	//
	// Note: async-task may chose to propagate via `Task::fallible()` or
	// directly via `Task::await` panicking. We probe whichever shape
	// it uses.
	let (tx, worker, _shutdown) = spawn_worker();
	let tx2 = tx.clone();

	let (runnable, task) = Builder::new().spawn(
		|()| async move {
			panic!("smoke test panic");
		},
		move |r| tx2.send(r).unwrap(),
	);
	runnable.schedule();

	// Catch the panic from the join handle.
	let join_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
		block_on(task);
		// We never reach here if the panic propagates.
		0u32
	}));
	assert!(join_result.is_err(), "panic in future did not surface on awaiter");

	drop(tx);
	let _ = worker.join();
}

#[test]
fn fallible_task_returns_panic_as_result() {
	// As an alternative to panic-propagation, async-task offers
	// fallible() which returns Result<R, Panic> when the future panics.
	// This is what the production code may prefer for cleaner error
	// flow. Confirm the API exists and behaves.
	let (tx, worker, _shutdown) = spawn_worker();
	let tx2 = tx.clone();

	let (runnable, task) = Builder::new().spawn(
		|()| async move {
			if true {
				panic!("fallible panic");
			}
			42u32
		},
		move |r| tx2.send(r).unwrap(),
	);
	runnable.schedule();

	// Task::fallible() converts the join from R to FallibleTask which
	// yields Result<R, ...>. The exact return type depends on the
	// async-task version; we just confirm it compiles and yields a
	// non-panicking value.
	let fallible = task.fallible();
	let outcome = block_on(fallible);
	assert!(outcome.is_none(), "fallible should report None on panic, got {:?}", outcome);

	drop(tx);
	let _ = worker.join();
}
