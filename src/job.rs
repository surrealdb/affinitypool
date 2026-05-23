//! Single-allocation task primitive for [`Threadpool::spawn`].
//!
//! A [`Job`] packs the worker-callable closure, the result slot, the
//! readiness state machine, an [`AtomicWaker`] and an Arc-style refcount
//! into one heap allocation. This replaces the prior pair of allocations
//! (one `Box<TaskData<F>>` for the closure and one `Arc<SpawnCompletion>`
//! for the result) used by `Threadpool::spawn`, halving allocator
//! traffic on the hot path.
//!
//! # Layout and dispatch
//!
//! [`Job`] is `#[repr(C)]` and its first field is a `&'static TaskVTable`,
//! which makes a `NonNull<Job<F, R>>` cast-compatible with the
//! `NonNull<TaskData<u8>>` that [`OwnedTask`] holds. The producer side
//! constructs an `OwnedTask` from the same allocation via
//! [`OwnedTask::from_raw`] without an extra box. The vtable's `call`
//! and `drop` function pointers are emitted per-(F, R) and know how to
//! cast back to the concrete `Job<F, R>` type.
//!
//! # State machine
//!
//! ```text
//!     EMPTY  --(worker call)--->  READY  --(awaiter take)-->  TAKEN
//!       |
//!       \--(worker drop)-->  ABORTED
//! ```
//!
//! Transitions out of `EMPTY` are exclusively performed by the worker
//! that holds the [`OwnedTask`]; the transition `READY -> TAKEN` is
//! exclusively performed by the awaiter ([`JobHandle::poll`]).
//!
//! # Reference counting
//!
//! `refs` begins at 2: one strong reference for the worker (held by the
//! [`OwnedTask`] pushed to the injector) and one for the awaiter (held
//! by the [`JobHandle`] returned to the caller). Whichever side hits
//! zero last is responsible for dropping any unconsumed result and
//! freeing the box.
//!
//! Acquire/Release on `refs.fetch_sub` is required: the final decrement
//! must synchronise with the other side's writes (worker's result write
//! or awaiter's `TAKEN` store) so the cleanup branch observes the latest
//! value of `state`.
//!
//! # Closure/result union
//!
//! The closure and the result never coexist (the worker consumes the
//! closure to produce the result), so their `MaybeUninit` cells share
//! one [`UnsafeCell<Slot<F, R>>`] union. This shrinks each [`Job`]
//! allocation from `header + sizeof(F) + sizeof(R)` to `header +
//! max(sizeof(F), sizeof(R))`, which often drops the allocation into a
//! smaller allocator size class.
//!
//! Soundness: which variant of the union is live is dictated entirely
//! by the [`state`](Job::state) machine. Worker writes to
//! `slot.closure` are only valid while `state == EMPTY`; worker writes
//! to `slot.result` only occur after `slot.closure` has been consumed
//! (via `assume_init_read`) and only before the `EMPTY -> READY`
//! transition. Awaiter reads of `slot.result` only occur after observing
//! `state == READY` under `Acquire`. The two windows never overlap.

use crate::atomic_waker::AtomicWaker;
use crate::task::{OwnedTask, TaskVTable};
use std::cell::UnsafeCell;
use std::future::Future;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::panic::{self, AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::ptr::{NonNull, addr_of_mut};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::thread;

/// Initial state: the worker has not yet observed the closure.
const EMPTY: u8 = 0;
/// The worker has executed the closure and written `result`. The awaiter
/// may now read it.
const READY: u8 = 1;
/// The awaiter has moved `result` out. The slot is logically uninhabited.
const TAKEN: u8 = 2;
/// The worker dropped the task without running it (e.g. pool shutdown).
/// The closure has been dropped; `result` was never written.
const ABORTED: u8 = 3;

/// Storage shared by the closure (live while `state == EMPTY`) and the
/// result (live while `state` is `READY`). The two never coexist; see
/// the closure/result union note in the module documentation.
///
/// `MaybeUninit` has no `Drop`, so the union itself has no `Drop`, and
/// Rust's "union field that may have a destructor" rule does not apply.
/// All cleanup of either variant is driven explicitly by the state
/// machine in [`Job`].
#[repr(C)]
pub(crate) union Slot<F, R> {
	closure: ManuallyDrop<MaybeUninit<F>>,
	result: ManuallyDrop<MaybeUninit<thread::Result<R>>>,
}

#[repr(C)]
pub(crate) struct Job<F, R> {
	// SAFETY: must remain the first field. `OwnedTask::from_raw` casts a
	// `NonNull<Job<F, R>>` to `NonNull<TaskData<u8>>`, whose own first
	// field is `&'static TaskVTable`. Reordering would corrupt vtable
	// dispatch.
	_table: &'static TaskVTable,
	state: AtomicU8,
	refs: AtomicUsize,
	waker: AtomicWaker,
	slot: UnsafeCell<Slot<F, R>>,
}

// Safety: the closure and the result move between threads inside
// `MaybeUninit` cells gated by `state`. The minimum sound bound is that
// both `F` and `R` are `Send` (the worker constructs `R` from `F()`).
unsafe impl<F: Send, R: Send> Send for Job<F, R> {}
unsafe impl<F: Send, R: Send> Sync for Job<F, R> {}

impl<F, R> Job<F, R> {
	/// Read the closure out of the slot in place. Caller must ensure
	/// the `closure` variant is the live one (i.e. `state == EMPTY`
	/// and no concurrent worker access).
	#[inline]
	unsafe fn read_closure(slot: *mut Slot<F, R>) -> F {
		// `addr_of_mut!` avoids creating an intermediate `&mut` to the
		// union field, which would otherwise be flagged as
		// `deref_nullptr`-adjacent UB in some toolchains. The pointer
		// we form is a raw pointer-to-MaybeUninit<F> through the
		// union's `closure` variant.
		unsafe {
			let closure_field: *mut ManuallyDrop<MaybeUninit<F>> =
				addr_of_mut!((*slot).closure);
			(*closure_field).assume_init_read()
		}
	}

	/// Drop the closure in place. Caller must ensure the `closure`
	/// variant is the live one.
	#[inline]
	unsafe fn drop_closure(slot: *mut Slot<F, R>) {
		unsafe {
			let closure_field: *mut ManuallyDrop<MaybeUninit<F>> =
				addr_of_mut!((*slot).closure);
			(*closure_field).assume_init_drop();
		}
	}

	/// Write the result into the slot in place. Caller must ensure the
	/// closure has already been consumed and no concurrent reader can
	/// see the slot until `state` transitions to `READY`.
	#[inline]
	unsafe fn write_result(slot: *mut Slot<F, R>, value: thread::Result<R>) {
		unsafe {
			let result_field: *mut ManuallyDrop<MaybeUninit<thread::Result<R>>> =
				addr_of_mut!((*slot).result);
			std::ptr::write(result_field, ManuallyDrop::new(MaybeUninit::new(value)));
		}
	}

	/// Read the result out of the slot. Caller must ensure the
	/// `result` variant is the live one (i.e. `state == READY`).
	#[inline]
	unsafe fn read_result(slot: *mut Slot<F, R>) -> thread::Result<R> {
		unsafe {
			let result_field: *mut ManuallyDrop<MaybeUninit<thread::Result<R>>> =
				addr_of_mut!((*slot).result);
			(*result_field).assume_init_read()
		}
	}

	/// Drop the result in place. Caller must ensure the `result`
	/// variant is the live one.
	#[inline]
	unsafe fn drop_result(slot: *mut Slot<F, R>) {
		unsafe {
			let result_field: *mut ManuallyDrop<MaybeUninit<thread::Result<R>>> =
				addr_of_mut!((*slot).result);
			(*result_field).assume_init_drop();
		}
	}
}

impl<F, R> Job<F, R>
where
	F: FnOnce() -> R + Send + 'static,
	R: Send + 'static,
{
	/// Allocate a fresh job and split it into an [`OwnedTask`] for the
	/// worker side and a [`JobHandle`] for the awaiter side.
	pub(crate) fn allocate(f: F) -> (OwnedTask<'static>, JobHandle<F, R>) {
		let boxed = Box::new(Job {
			_table: Self::vtable(),
			state: AtomicU8::new(EMPTY),
			refs: AtomicUsize::new(2),
			waker: AtomicWaker::new(),
			slot: UnsafeCell::new(Slot {
				closure: ManuallyDrop::new(MaybeUninit::new(f)),
			}),
		});
		// Safety: a `Box::into_raw` pointer is never null.
		let ptr: NonNull<Job<F, R>> = unsafe { NonNull::new_unchecked(Box::into_raw(boxed)) };
		// Safety: `ptr` is a live `Box`-derived allocation whose first
		// field is `&'static TaskVTable`. The vtable's `call`/`drop`
		// functions both cast `NonNull<()>` back to `*mut Job<F, R>`.
		let task = unsafe { OwnedTask::from_raw(ptr.cast()) };
		let handle = JobHandle::new(ptr);
		(task, handle)
	}

	fn vtable() -> &'static TaskVTable {
		trait HasJobVTable {
			const TABLE: TaskVTable;
		}
		impl<F: FnOnce() -> R + Send + 'static, R: Send + 'static> HasJobVTable for Job<F, R> {
			const TABLE: TaskVTable = TaskVTable {
				call: Job::<F, R>::call_worker,
				drop: Job::<F, R>::drop_worker,
			};
		}
		&<Self as HasJobVTable>::TABLE
	}

	/// Worker entry point. Runs the closure (catching panics), writes the
	/// result, transitions `EMPTY -> READY`, wakes the awaiter, and
	/// releases the worker's reference.
	unsafe fn call_worker(this: NonNull<()>) {
		let job_ptr = this.cast::<Job<F, R>>();
		let job_ref: &Job<F, R> = unsafe { job_ptr.as_ref() };
		// Move the closure out of its slot. After this read the slot's
		// storage is logically uninhabited until `result` is written.
		// Safety: we have exclusive worker access until `state` leaves
		// `EMPTY`, and `state` is `EMPTY` here (the worker has not yet
		// transitioned it). The closure variant of the union is the
		// live one while `state == EMPTY`.
		let closure: F = unsafe { Self::read_closure(job_ref.slot.get()) };
		// Catch panics so they don't unwind through the worker loop.
		let outcome = catch_unwind(AssertUnwindSafe(closure));
		// Safety: same exclusivity argument — no other thread reads or
		// writes the slot while `state` is `EMPTY`. The closure has been
		// consumed, so writing the `result` variant of the union is the
		// next legal access. The transition `EMPTY -> READY` happens
		// after this write.
		unsafe {
			Self::write_result(job_ref.slot.get(), outcome);
		}
		// Release the write of `result` to any consumer that observes
		// `READY` under an `Acquire` load.
		job_ref.state.store(READY, Ordering::Release);
		job_ref.waker.wake();
		// Release the worker's reference. If the awaiter has already
		// dropped, this frees the allocation.
		unsafe {
			Self::release_ref(job_ptr);
		}
	}

	/// Worker drop path. Invoked when the [`OwnedTask`] is dropped without
	/// being run (the only practical trigger is pool shutdown discarding
	/// queued tasks). Drops the closure, transitions to `ABORTED`, wakes
	/// the awaiter (so its `await` doesn't hang indefinitely), and
	/// releases the worker's reference.
	unsafe fn drop_worker(this: NonNull<()>) {
		let job_ptr = this.cast::<Job<F, R>>();
		let job_ref: &Job<F, R> = unsafe { job_ptr.as_ref() };
		// Safety: same exclusivity argument as `call_worker`. The closure
		// variant of the union is live while `state == EMPTY`.
		unsafe {
			Self::drop_closure(job_ref.slot.get());
		}
		job_ref.state.store(ABORTED, Ordering::Release);
		job_ref.waker.wake();
		unsafe {
			Self::release_ref(job_ptr);
		}
	}

	/// Decrement the refcount. On the final decrement, drop any value
	/// still in the slot and free the allocation.
	unsafe fn release_ref(ptr: NonNull<Job<F, R>>) {
		let job_ref: &Job<F, R> = unsafe { ptr.as_ref() };
		// AcqRel: this release synchronises with concurrent writes to
		// `state` on the other side; the acquire half ensures we observe
		// those writes on the final decrement.
		if job_ref.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
			// We hold the unique reference; observe the latest `state`.
			// A relaxed load is fine: the AcqRel fetch_sub above already
			// synchronised with all prior writes.
			let final_state = job_ref.state.load(Ordering::Relaxed);
			if final_state == READY {
				// Worker wrote a result the awaiter never took. Drop it
				// before deallocating.
				// Safety: state == READY guarantees the result variant
				// of the slot is initialised, and refs == 0 guarantees
				// no other reference.
				unsafe {
					Self::drop_result(job_ref.slot.get());
				}
			}
			// Reclaim the allocation.
			// Safety: we hold the unique reference and the box originated
			// from `Box::into_raw` in `allocate`.
			drop(unsafe { Box::from_raw(ptr.as_ptr()) });
		}
	}
}

/// Awaiter-side handle to a [`Job`]. Holds one strong reference; releases
/// it on drop or after a successful poll.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub(crate) struct JobHandle<F, R> {
	ptr: Option<NonNull<Job<F, R>>>,
}

// Safety: same bounds as `Job<F, R>`. The handle merely holds a pointer
// to the shared allocation; the `Send`/`Sync` semantics come from `Job`.
unsafe impl<F: Send, R: Send> Send for JobHandle<F, R> {}
unsafe impl<F: Send, R: Send> Sync for JobHandle<F, R> {}

impl<F, R> JobHandle<F, R>
where
	F: FnOnce() -> R + Send + 'static,
	R: Send + 'static,
{
	fn new(ptr: NonNull<Job<F, R>>) -> Self {
		Self {
			ptr: Some(ptr),
		}
	}

	/// Consume the ready result. Caller must have already observed
	/// `state == READY` under an `Acquire` load.
	fn take_ready(&mut self) -> Poll<R> {
		let ptr = self.ptr.take().expect("JobHandle polled after completion");
		let job_ref: &Job<F, R> = unsafe { ptr.as_ref() };
		// Safety: state == READY guarantees the `result` variant of the
		// slot is initialised, and because we observed it under
		// `Acquire` the write is visible. Marking `state` as `TAKEN`
		// immediately after keeps the invariant that exactly one side
		// will see `READY` on the cleanup path.
		let value = unsafe { Job::<F, R>::read_result(job_ref.slot.get()) };
		job_ref.state.store(TAKEN, Ordering::Release);
		// Release our reference. May or may not be the last.
		unsafe { Job::<F, R>::release_ref(ptr) };
		match value {
			Ok(v) => Poll::Ready(v),
			Err(payload) => panic::resume_unwind(payload),
		}
	}
}

impl<F, R> Future for JobHandle<F, R>
where
	F: FnOnce() -> R + Send + 'static,
	R: Send + 'static,
{
	type Output = R;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		// JobHandle has no pinned fields.
		let this = Pin::into_inner(self);
		let ptr = this.ptr.expect("JobHandle polled after completion");
		let job_ref: &Job<F, R> = unsafe { ptr.as_ref() };
		// Fast path: result already ready, skip the waker dance.
		match job_ref.state.load(Ordering::Acquire) {
			READY => return this.take_ready(),
			ABORTED => {
				// Worker dropped the task without running. Surface this
				// as a panic on the awaiter — same shape as a closure
				// panic, but with a fixed payload.
				let _ = this.ptr.take();
				unsafe { Job::<F, R>::release_ref(ptr) };
				panic::resume_unwind(Box::new("affinitypool job aborted before execution"));
			}
			_ => {}
		}
		// Slow path: register the waker, then re-check to close the race
		// with a concurrent worker transition between the load above and
		// the register call.
		job_ref.waker.register(cx.waker());
		match job_ref.state.load(Ordering::Acquire) {
			READY => this.take_ready(),
			ABORTED => {
				let _ = this.ptr.take();
				unsafe { Job::<F, R>::release_ref(ptr) };
				panic::resume_unwind(Box::new("affinitypool job aborted before execution"));
			}
			_ => Poll::Pending,
		}
	}
}

impl<F, R> Drop for JobHandle<F, R> {
	fn drop(&mut self) {
		if let Some(ptr) = self.ptr.take() {
			// Release the awaiter's reference. The worker side may still
			// be running; if it has already finished, this is the final
			// decrement and frees the allocation.
			//
			// Safety: `release_ref` requires `F: FnOnce()->R + Send +
			// 'static` and `R: Send + 'static`; `JobHandle<F, R>` only
			// exists with those bounds (the constructor enforces them).
			// The pointer was originated by `Job::<F, R>::allocate`.
			//
			// We don't impose the bounds on `impl Drop` because Drop
			// implementations cannot add bounds the type itself doesn't
			// have. Instead, the static dispatch below uses `Job::<F,
			// R>::release_ref` which itself carries the bounds — and
			// since `JobHandle<F, R>` is only ever produced by
			// `Job::<F, R>::allocate`, those bounds hold at every call
			// site that can actually drop a `JobHandle`.
			unsafe { Job::<F, R>::release_ref_no_bound(ptr) };
		}
	}
}

impl<F, R> Job<F, R> {
	/// `release_ref` minus the `'static` bound on the type parameters. We
	/// need this for `Drop` (which cannot restate the trait bounds), and
	/// it is sound because: (a) the bounds only matter for the closure
	/// execution path, which has already happened (or aborted) by the
	/// time any `JobHandle` can be dropped; (b) `Box::from_raw` and
	/// `MaybeUninit::assume_init_drop` are not bound-dependent.
	unsafe fn release_ref_no_bound(ptr: NonNull<Job<F, R>>) {
		let job_ref: &Job<F, R> = unsafe { ptr.as_ref() };
		if job_ref.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
			let final_state = job_ref.state.load(Ordering::Relaxed);
			if final_state == READY {
				unsafe {
					Self::drop_result(job_ref.slot.get());
				}
			} else if final_state == EMPTY {
				// Defensive: should not be reachable in normal flow
				// (the worker always transitions away from EMPTY before
				// releasing its ref). If it ever is, drop the closure
				// variant so we don't leak it.
				unsafe {
					Self::drop_closure(job_ref.slot.get());
				}
			}
			drop(unsafe { Box::from_raw(ptr.as_ptr()) });
		}
	}
}

#[cfg(test)]
mod tests {
	//! Miri exercisers for every unsafe block in this module. Run with
	//! `cargo miri test --lib job::`.

	use super::*;
	use std::sync::Arc;
	use std::sync::atomic::AtomicUsize;
	use std::task::Waker;

	fn noop_ctx() -> Context<'static> {
		Context::from_waker(Waker::noop())
	}

	/// `allocate` then run the worker via the vtable, then poll the
	/// handle. Exercises `call_worker`, `release_ref` (worker side, not
	/// final), `take_ready`, and `release_ref` (awaiter side, final).
	#[test]
	fn allocate_run_poll_returns_value() {
		let (task, mut handle) = Job::allocate(|| 0xfeedu32);
		task.run();
		let mut cx = noop_ctx();
		match Pin::new(&mut handle).poll(&mut cx) {
			Poll::Ready(v) => assert_eq!(v, 0xfeed),
			Poll::Pending => panic!("expected Ready"),
		}
	}

	/// Drop the awaiter before the worker runs. Worker runs, writes
	/// result, and on `release_ref` becomes the final reference and
	/// must drop the result before freeing.
	#[test]
	fn drop_handle_then_run_worker_frees_result() {
		struct DropTracker(Arc<AtomicUsize>);
		impl Drop for DropTracker {
			fn drop(&mut self) {
				self.0.fetch_add(1, Ordering::SeqCst);
			}
		}
		let drops = Arc::new(AtomicUsize::new(0));
		let drops_clone = drops.clone();
		let (task, handle) = Job::allocate(move || DropTracker(drops_clone));
		drop(handle);
		task.run();
		assert_eq!(drops.load(Ordering::SeqCst), 1);
	}

	/// Drop the worker without running, then poll the handle. The
	/// handle observes `ABORTED` and resumes the unwind. Verify the
	/// closure's captures were dropped.
	#[test]
	fn drop_task_then_poll_handle_panics_with_abort() {
		struct DropTracker(Arc<AtomicUsize>);
		impl Drop for DropTracker {
			fn drop(&mut self) {
				self.0.fetch_add(1, Ordering::SeqCst);
			}
		}
		let drops = Arc::new(AtomicUsize::new(0));
		let drops_clone = drops.clone();
		let tracker = DropTracker(drops_clone);
		let (task, mut handle) = Job::allocate(move || {
			let _t = tracker;
			0u32
		});
		drop(task);
		assert_eq!(drops.load(Ordering::SeqCst), 1, "closure captures must be dropped on abort");
		let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			let mut cx = noop_ctx();
			let _ = Pin::new(&mut handle).poll(&mut cx);
		}));
		assert!(result.is_err(), "ABORTED must surface as a panic");
	}

	/// Worker panics inside the closure. The awaiter must see the
	/// captured panic resumed on poll.
	#[test]
	fn worker_panic_propagates_to_awaiter() {
		let (task, mut handle) = Job::allocate(|| -> u32 { panic!("boom") });
		task.run();
		let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			let mut cx = noop_ctx();
			let _ = Pin::new(&mut handle).poll(&mut cx);
		}));
		assert!(result.is_err(), "panic must propagate from worker to awaiter");
	}

	/// Poll the handle before the worker runs (Pending), then run the
	/// worker, then poll again (Ready). Exercises the
	/// register-then-re-check race-closing path.
	#[test]
	fn poll_pending_then_run_then_ready() {
		let (task, mut handle) = Job::allocate(|| 7u32);
		let mut cx = noop_ctx();
		match Pin::new(&mut handle).poll(&mut cx) {
			Poll::Pending => {}
			Poll::Ready(_) => panic!("should be Pending before run"),
		}
		task.run();
		match Pin::new(&mut handle).poll(&mut cx) {
			Poll::Ready(v) => assert_eq!(v, 7),
			Poll::Pending => panic!("should be Ready after run"),
		}
	}

	/// Confirms the closure/result union actually shrinks `Job<F, R>`
	/// when `sizeof(F)` and `sizeof(R)` overlap usefully. With separate
	/// fields the slot would occupy `sizeof(F) + sizeof(thread::Result<R>)`;
	/// with the union it occupies `max(sizeof(F), sizeof(thread::Result<R>))`.
	#[test]
	fn slot_union_shrinks_job_size() {
		use std::mem::size_of;
		// Stand-in for a closure that captures four usize-sized values:
		// `Slot<F, u8>` has the same size as `Slot<FakeClosure, u8>`
		// because layout depends only on `size_of` / `align_of`, not on
		// the trait-object-ness of F.
		#[repr(transparent)]
		#[allow(dead_code)]
		struct FakeClosure([usize; 4]);
		let f_size = size_of::<FakeClosure>();
		let r_size = size_of::<thread::Result<u8>>();
		let slot_size = size_of::<super::Slot<FakeClosure, u8>>();
		assert_eq!(
			slot_size,
			f_size.max(r_size),
			"slot ({slot_size}) should equal max(F={f_size}, R={r_size}) under the union"
		);
		assert!(
			slot_size < f_size + r_size,
			"slot ({slot_size}) should be strictly smaller than F+R={}",
			f_size + r_size,
		);
	}

	/// Result type that owns a heap allocation. Validates that the slot
	/// drops the unconsumed result on the final-ref path without leaking
	/// or double-freeing. Miri catches both.
	#[test]
	fn unconsumed_owned_result_dropped_exactly_once() {
		struct Tracker(Arc<AtomicUsize>);
		impl Drop for Tracker {
			fn drop(&mut self) {
				self.0.fetch_add(1, Ordering::SeqCst);
			}
		}
		let drops = Arc::new(AtomicUsize::new(0));
		let drops_clone = drops.clone();
		let (task, handle) = Job::allocate(move || Tracker(drops_clone));
		task.run();
		drop(handle);
		assert_eq!(drops.load(Ordering::SeqCst), 1);
	}
}
