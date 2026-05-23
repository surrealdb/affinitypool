use std::{marker::PhantomData, ptr::NonNull};

/// VTable for type-erased task dispatch.
///
/// The `call` and `drop` function pointers take a `NonNull<()>` to the
/// owning allocation. Each per-(closure-type, payload-type) static
/// vtable instance knows how to cast the pointer back to its concrete
/// type. The first field of any allocation referenced by an
/// [`OwnedTask`] must be a `&'static TaskVTable`; this is what makes
/// the polymorphic dispatch sound.
pub(crate) struct TaskVTable {
	pub(crate) call: unsafe fn(NonNull<()>),
	pub(crate) drop: unsafe fn(NonNull<()>),
}

/// Type-erased handle to a task allocation managed elsewhere
/// (`crate::job::Job` for `spawn`, `crate::local::LocalJob` for
/// `spawn_local`). The pointed-to allocation must begin with a
/// `&'static TaskVTable` so the vtable's `call`/`drop` can dispatch
/// back to the concrete type.
pub struct OwnedTask<'a>(NonNull<()>, PhantomData<&'a mut &'a ()>);

unsafe impl Send for OwnedTask<'_> {}

impl<'a> OwnedTask<'a> {
	/// Wrap a raw pointer to an externally-managed allocation as an
	/// [`OwnedTask`]. The allocation must begin with a
	/// `&'static TaskVTable` field whose `call` and `drop` functions
	/// know how to cast `ptr` back to the concrete type.
	///
	/// # Safety
	///
	/// * `ptr` must point to a live allocation whose first field is a
	///   `&'static TaskVTable`.
	/// * The vtable's `call` is invoked at most once by [`Self::run`];
	///   the vtable's `drop` is invoked at most once by [`Drop`]. The
	///   callee is responsible for any release of the allocation
	///   itself; this wrapper does not free the allocation.
	/// * Any captured borrows of `'a` must outlive the returned
	///   `OwnedTask<'a>`.
	#[inline]
	pub(crate) unsafe fn from_raw(ptr: NonNull<()>) -> Self {
		Self(ptr, PhantomData)
	}

	/// Read the vtable from the allocation. Safe because the vtable
	/// pointer is the first field by contract.
	#[inline]
	unsafe fn vtable(&self) -> &'static TaskVTable {
		unsafe { self.0.cast::<&'static TaskVTable>().as_ref() }
	}

	#[inline]
	pub fn run(self) {
		let vtable = unsafe { self.vtable() };
		unsafe { (vtable.call)(self.0) };
		// `call` is responsible for the allocation; don't run our `Drop`.
		std::mem::forget(self);
	}
}

impl Drop for OwnedTask<'_> {
	fn drop(&mut self) {
		let vtable = unsafe { self.vtable() };
		unsafe { (vtable.drop)(self.0) };
	}
}

#[cfg(test)]
mod tests {
	//! Exercise the vtable dispatch in [`OwnedTask`] without invoking
	//! the thread pool. Miri walks every unsafe block here.
	//!
	//! The unsafe machinery that *uses* `OwnedTask` (`Job`,
	//! `LocalJob`) has its own dedicated unit-test modules; these
	//! tests cover only the wrapper's dispatch contract.

	use super::*;
	use std::cell::UnsafeCell;
	use std::sync::Arc;
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

	/// Minimal vtabled allocation: a single bool tracking whether the
	/// `call` or `drop` vtable entry fired.
	#[repr(C)]
	struct TestTask {
		table: &'static TaskVTable,
		ran: UnsafeCell<bool>,
		dropped: Arc<AtomicBool>,
	}

	unsafe fn test_call(this: NonNull<()>) {
		let p = this.cast::<TestTask>().as_ptr();
		unsafe { *(*p).ran.get() = true };
		// `call` takes ownership: free the allocation.
		drop(unsafe { Box::from_raw(p) });
	}

	unsafe fn test_drop(this: NonNull<()>) {
		let p = this.cast::<TestTask>().as_ptr();
		unsafe { (*p).dropped.store(true, Ordering::SeqCst) };
		drop(unsafe { Box::from_raw(p) });
	}

	static TEST_VTABLE: TaskVTable = TaskVTable {
		call: test_call,
		drop: test_drop,
	};

	fn make_task(dropped: Arc<AtomicBool>) -> (OwnedTask<'static>, NonNull<TestTask>) {
		let boxed = Box::new(TestTask {
			table: &TEST_VTABLE,
			ran: UnsafeCell::new(false),
			dropped,
		});
		let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(boxed)) };
		let task = unsafe { OwnedTask::from_raw(ptr.cast()) };
		(task, ptr)
	}

	/// `OwnedTask::run` invokes the vtable's `call`. The allocation is
	/// freed inside `call` and the wrapper does not run its own `Drop`.
	#[test]
	fn run_dispatches_to_call_and_skips_drop() {
		let dropped = Arc::new(AtomicBool::new(false));
		let (task, ptr) = make_task(dropped.clone());
		task.run();
		// `call` set `ran = true` and freed the box. We can't deref
		// `ptr` after free; the assertion is that `dropped` was never
		// set (drop vtable did not fire).
		assert!(!dropped.load(Ordering::SeqCst));
		let _ = ptr; // suppress unused warning
	}

	/// Dropping `OwnedTask` without `run` invokes the vtable's `drop`.
	#[test]
	fn drop_dispatches_to_drop_vtable() {
		let dropped = Arc::new(AtomicBool::new(false));
		let (task, _ptr) = make_task(dropped.clone());
		drop(task);
		assert!(dropped.load(Ordering::SeqCst));
	}

	/// Many distinct vtables can coexist for distinct allocation
	/// shapes. Miri also walks an allocation with non-trivial payload
	/// to catch any aliasing slipups in `vtable()`.
	#[test]
	fn vtable_first_field_is_observed_correctly() {
		#[repr(C)]
		struct Tracker {
			table: &'static TaskVTable,
			counter: Arc<AtomicUsize>,
			payload: Vec<u8>,
		}
		unsafe fn call(this: NonNull<()>) {
			let p = this.cast::<Tracker>().as_ptr();
			unsafe {
				(*p).counter.fetch_add((*p).payload.len(), Ordering::SeqCst);
			}
			drop(unsafe { Box::from_raw(p) });
		}
		unsafe fn drop_fn(this: NonNull<()>) {
			drop(unsafe { Box::from_raw(this.cast::<Tracker>().as_ptr()) });
		}
		static VT: TaskVTable = TaskVTable {
			call,
			drop: drop_fn,
		};
		let counter = Arc::new(AtomicUsize::new(0));
		let boxed = Box::new(Tracker {
			table: &VT,
			counter: counter.clone(),
			payload: vec![1, 2, 3, 4, 5],
		});
		let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(boxed)) };
		let task = unsafe { OwnedTask::from_raw(ptr.cast()) };
		task.run();
		assert_eq!(counter.load(Ordering::SeqCst), 5);
	}
}
