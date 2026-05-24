//! `spawn_local` implementation: single-allocation, state-machine-driven
//! task primitive with a block-on-drop guard.
//!
//! Like [`crate::job::Job`], a [`LocalJob`] packs the closure storage,
//! the result slot, the readiness state machine, an [`AtomicWaker`] for
//! async polling, and an Arc-style refcount into one heap allocation.
//! The two differences vs. `Job` are:
//!
//! 1. The closure does not require `'static` — it can borrow from
//!    `'pool` (the borrow of [`Threadpool`] held by the calling
//!    [`SpawnFuture`]).
//! 2. A `drop_waiter` slot (`Mutex<Option<Thread>>`) lets [`SpawnFuture`]
//!    block its dropping thread until the worker has either run the
//!    closure (`READY`) or dropped it (`ABORTED`). This is what makes
//!    erasing the `'pool` lifetime sound when the task is pushed to the
//!    global injector.
//!
//! # Leak safety
//!
//! `SpawnFuture` carries a borrow with lifetime `'pool`. Internally the
//! task pushed to the global injector is wrapped via
//! [`OwnedTask::from_raw`] and so has its lifetime erased to `'static`.
//! Soundness of `spawn_local` therefore depends on either:
//!
//! * The future's `Drop` running, which blocks until the worker has
//!   finished with the closure (consuming or dropping it), or
//! * The future being `mem::forget`-ed, which keeps the allocation
//!   alive forever (memory leak, but no use-after-free). This is the
//!   standard Rust leak-amplification rule.

use crate::atomic_waker::AtomicWaker;
use crate::job::Slot;
use crate::task::{OwnedTask, TaskVTable};
use crate::{Ordering, Threadpool};
use parking_lot::Mutex;
use std::cell::UnsafeCell;
use std::future::Future;
use std::panic::{self, AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, AtomicUsize};
use std::task::{Context, Poll};
use std::thread;

const EMPTY: u8 = 0;
const READY: u8 = 1;
const TAKEN: u8 = 2;
const ABORTED: u8 = 3;

#[repr(C)]
struct LocalJob<F, R> {
	// SAFETY: must remain the first field. See `Job` in `src/job.rs` for
	// the vtable-cast soundness argument; the same applies here.
	_table: &'static TaskVTable,
	state: AtomicU8,
	refs: AtomicUsize,
	waker: AtomicWaker,
	/// Optional [`Thread`] waiting in [`SpawnFuture::drop`]. The worker,
	/// after transitioning `state` away from `EMPTY`, takes this and
	/// unparks the thread (if any). Uses a `parking_lot::Mutex` for
	/// serialisation; in the steady-state happy path (no drop while
	/// `EMPTY`) the lock is never contended and the field stays `None`.
	drop_waiter: Mutex<Option<thread::Thread>>,
	slot: UnsafeCell<Slot<F, R>>,
}

// Safety: same argument as `Job<F, R>`. The closure and the result move
// between threads inside `MaybeUninit` cells gated by `state`; both `F`
// and `R` must be `Send`. `drop_waiter` holds a `Thread`, which is `Sync`.
unsafe impl<F: Send, R: Send> Send for LocalJob<F, R> {}
unsafe impl<F: Send, R: Send> Sync for LocalJob<F, R> {}

impl<F, R> LocalJob<F, R>
where
	F: FnOnce() -> R + Send,
	R: Send,
{
	#[inline]
	fn vtable() -> &'static TaskVTable {
		trait HasLocalVTable {
			const TABLE: TaskVTable;
		}
		impl<F: FnOnce() -> R + Send, R: Send> HasLocalVTable for LocalJob<F, R> {
			const TABLE: TaskVTable = TaskVTable {
				call: LocalJob::<F, R>::call_worker,
				drop: LocalJob::<F, R>::drop_worker,
			};
		}
		&<Self as HasLocalVTable>::TABLE
	}

	#[inline]
	fn allocate(f: F) -> (OwnedTask<'static>, NonNull<LocalJob<F, R>>) {
		let boxed = Box::new(LocalJob {
			_table: Self::vtable(),
			state: AtomicU8::new(EMPTY),
			refs: AtomicUsize::new(2),
			waker: AtomicWaker::new(),
			drop_waiter: Mutex::new(None),
			slot: UnsafeCell::new(Slot::new_closure(f)),
		});
		// Safety: a `Box::into_raw` pointer is never null.
		let ptr: NonNull<LocalJob<F, R>> = unsafe { NonNull::new_unchecked(Box::into_raw(boxed)) };
		// Safety: the allocation's first field is `&'static TaskVTable`.
		// `SpawnFuture::drop` blocks until the worker has finished with
		// the allocation, so the erased `'static` lifetime never escapes
		// the closure's actual `'pool` borrow.
		let task = unsafe { OwnedTask::from_raw(ptr.cast()) };
		(task, ptr)
	}

	/// Worker entry point. Mirrors `Job::call_worker` with an additional
	/// notification to any thread waiting in [`SpawnFuture::drop`].
	#[inline]
	unsafe fn call_worker(this: NonNull<()>) {
		let job_ptr = this.cast::<LocalJob<F, R>>();
		let job_ref: &LocalJob<F, R> = unsafe { job_ptr.as_ref() };
		// Safety: state is `EMPTY` so the closure variant is live and
		// we are the unique writer (the worker).
		let closure: F = unsafe { Slot::read_closure(job_ref.slot.get()) };
		let outcome = catch_unwind(AssertUnwindSafe(closure));
		// Safety: closure consumed; the result variant becomes live.
		unsafe {
			Slot::write_result(job_ref.slot.get(), outcome);
		}
		job_ref.state.store(READY, Ordering::Release);
		job_ref.waker.wake();
		Self::notify_drop_waiter(job_ref);
		unsafe {
			Self::release_ref(job_ptr);
		}
	}

	/// Worker drop path. Invoked when the [`OwnedTask`] is dropped
	/// without running (e.g. pool shutdown).
	#[inline]
	unsafe fn drop_worker(this: NonNull<()>) {
		let job_ptr = this.cast::<LocalJob<F, R>>();
		let job_ref: &LocalJob<F, R> = unsafe { job_ptr.as_ref() };
		// Safety: state is `EMPTY` so the closure variant is live.
		unsafe {
			Slot::drop_closure(job_ref.slot.get());
		}
		job_ref.state.store(ABORTED, Ordering::Release);
		job_ref.waker.wake();
		Self::notify_drop_waiter(job_ref);
		unsafe {
			Self::release_ref(job_ptr);
		}
	}

	/// Unpark a thread waiting in [`SpawnFuture::drop`], if any.
	#[inline]
	fn notify_drop_waiter(job: &LocalJob<F, R>) {
		let waiter = job.drop_waiter.lock().take();
		if let Some(t) = waiter {
			t.unpark();
		}
	}
}

impl<F, R> LocalJob<F, R> {
	/// Decrement the refcount. On the final decrement, drop any value
	/// still in the slot and free the allocation. No `F: Send + 'static`
	/// bound — see the analogous note on `Job::release_ref_no_bound`.
	#[inline]
	unsafe fn release_ref(ptr: NonNull<LocalJob<F, R>>) {
		let job_ref: &LocalJob<F, R> = unsafe { ptr.as_ref() };
		if job_ref.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
			let final_state = job_ref.state.load(Ordering::Relaxed);
			if final_state == READY {
				unsafe {
					Slot::drop_result(job_ref.slot.get());
				}
			} else if final_state == EMPTY {
				// Defensive: should not occur in normal flow.
				unsafe {
					Slot::drop_closure(job_ref.slot.get());
				}
			}
			drop(unsafe { Box::from_raw(ptr.as_ptr()) });
		}
	}
}

enum State<F, R> {
	Init(F),
	Running(NonNull<LocalJob<F, R>>),
	Done,
}

/// A future that spawns a task on the threadpool and returns the result.
///
/// # Leak safety
///
/// `SpawnFuture` carries a borrow with lifetime `'pool` and erases that
/// lifetime internally when pushing the task to the global injector.
/// Soundness of `spawn_local` therefore does **not** depend on this
/// future's `Drop` running.
///
/// If the future is `mem::forget`-ed instead of dropped, the
/// [`LocalJob`] (and through it, the captured closure plus any `'pool`
/// borrows it holds) stays alive forever — leaked, but never accessed
/// after its lifetime has ended. This is the standard Rust
/// leak-amplification rule.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SpawnFuture<'pool, F, R> {
	pool: &'pool Threadpool,
	state: State<F, R>,
}

// Safety: `SpawnFuture` is `Send` iff the closure (held in `Init`) and
// the eventual return type are. The `Running` variant holds a raw
// pointer to a `LocalJob` whose `Send`/`Sync` are conditional on the
// same bounds.
unsafe impl<F: Send, R: Send> Send for SpawnFuture<'_, F, R> {}

impl<'pool, F, R> SpawnFuture<'pool, F, R>
where
	F: FnOnce() -> R + Send,
	R: Send,
{
	pub(crate) fn new(pool: &'pool Threadpool, f: F) -> Self {
		SpawnFuture {
			pool,
			state: State::Init(f),
		}
	}
}

impl<F, R> Future for SpawnFuture<'_, F, R>
where
	F: FnOnce() -> R + Send,
	R: Send,
{
	type Output = R;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		// Safety: structural pinning preserved throughout.
		let this = unsafe { self.as_mut().get_unchecked_mut() };
		loop {
			match this.state {
				State::Init(_) => {
					let State::Init(f) = std::mem::replace(&mut this.state, State::Done) else {
						unreachable!()
					};
					let (task, job_ptr) = LocalJob::allocate(f);
					// Register waker before submitting so we never miss
					// a completion that races our first poll.
					// Safety: we hold the awaiter's refcount (one of
					// the two created at `allocate`) and have not yet
					// stored the pointer anywhere a worker could
					// release from.
					let job_ref = unsafe { job_ptr.as_ref() };
					job_ref.waker.register(cx.waker());
					// Push to the injector. Same SeqCst handshake as
					// `Threadpool::spawn`.
					this.pool.data.injector.push(task);
					std::sync::atomic::fence(Ordering::SeqCst);
					if this.pool.data.parked_count.load(Ordering::Acquire) > 0
						&& let Some(t) = this.pool.data.parked_threads.pop()
					{
						this.pool.data.parked_count.fetch_sub(1, Ordering::Release);
						t.unpark();
					}
					this.state = State::Running(job_ptr);
				}
				State::Running(job_ptr) => {
					// Safety: we hold one refcount; the worker holds the
					// other. The allocation is live until both release.
					let job_ref = unsafe { job_ptr.as_ref() };
					match job_ref.state.load(Ordering::Acquire) {
						READY => return this.take_ready(job_ptr),
						ABORTED => {
							this.state = State::Done;
							// Safety: this awaiter owns one ref; release
							// it. Worker has already released. The
							// allocation will be freed by the final
							// decrement here.
							unsafe { LocalJob::<F, R>::release_ref(job_ptr) };
							panic::resume_unwind(Box::new(
								"affinitypool spawn_local task aborted before execution",
							));
						}
						_ => {}
					}
					// Slow path: register waker, re-check to close the
					// race with a worker completing between the load
					// above and this register.
					job_ref.waker.register(cx.waker());
					match job_ref.state.load(Ordering::Acquire) {
						READY => return this.take_ready(job_ptr),
						ABORTED => {
							this.state = State::Done;
							unsafe { LocalJob::<F, R>::release_ref(job_ptr) };
							panic::resume_unwind(Box::new(
								"affinitypool spawn_local task aborted before execution",
							));
						}
						_ => return Poll::Pending,
					}
				}
				State::Done => panic!("SpawnFuture polled after completion"),
			}
		}
	}
}

impl<F, R> SpawnFuture<'_, F, R> {
	#[inline]
	fn take_ready(&mut self, job_ptr: NonNull<LocalJob<F, R>>) -> Poll<R> {
		// Safety: state == READY observed under Acquire above.
		let job_ref = unsafe { job_ptr.as_ref() };
		let value = unsafe { Slot::read_result(job_ref.slot.get()) };
		job_ref.state.store(TAKEN, Ordering::Release);
		self.state = State::Done;
		// Release the awaiter's ref. Worker may have already released.
		unsafe { LocalJob::<F, R>::release_ref(job_ptr) };
		match value {
			Ok(v) => Poll::Ready(v),
			Err(payload) => panic::resume_unwind(payload),
		}
	}
}

impl<F, R> Drop for SpawnFuture<'_, F, R> {
	fn drop(&mut self) {
		// If we're in the Running state and the worker hasn't yet
		// transitioned `state` away from `EMPTY`, we must block until
		// it does — otherwise dropping `LocalJob` here while a worker
		// holds a pointer into it would be a use-after-free. The
		// closure may also reference `'pool` data that goes out of
		// scope as soon as this drop returns.
		let job_ptr = match self.state {
			State::Running(ptr) => ptr,
			_ => return,
		};
		// Safety: we hold the awaiter's refcount.
		let job_ref = unsafe { job_ptr.as_ref() };
		// Block until state is no longer EMPTY.
		if job_ref.state.load(Ordering::Acquire) == EMPTY {
			// Register this thread as the drop-waiter.
			*job_ref.drop_waiter.lock() = Some(thread::current());
			// Re-check after installing — a worker that completed
			// between the first load and the install will have
			// already taken `drop_waiter` (None, so we install ours
			// safely) but state will be non-EMPTY now and we skip
			// the park loop.
			while job_ref.state.load(Ordering::Acquire) == EMPTY {
				thread::park();
			}
			// Clear our slot in case the worker raced and stored
			// nothing (a no-op if the worker already took it).
			let _ = job_ref.drop_waiter.lock().take();
		}
		// Release the awaiter's reference. If the worker has already
		// released, this is the final decrement and frees the box.
		unsafe { LocalJob::<F, R>::release_ref(job_ptr) };
	}
}
