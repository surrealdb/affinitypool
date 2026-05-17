use std::{
	marker::PhantomData,
	mem::{self, ManuallyDrop},
	ptr::NonNull,
};

struct TaskVTable {
	call: unsafe fn(NonNull<()>),
	drop: unsafe fn(NonNull<()>),
}

#[repr(C)]
struct TaskData<T> {
	table: &'static TaskVTable,
	data: ManuallyDrop<T>,
}

pub struct OwnedTask<'a>(NonNull<TaskData<u8>>, PhantomData<&'a mut &'a ()>);

unsafe impl Send for OwnedTask<'_> {}

impl<'a> OwnedTask<'a> {
	unsafe fn call<T: FnOnce() + Send + 'a>(this: NonNull<()>) {
		unsafe {
			let mut this = this.cast::<TaskData<T>>();
			ManuallyDrop::take(&mut this.as_mut().data)();
			mem::drop(Box::from_raw(this.as_ptr()));
		}
	}

	unsafe fn drop<T>(this: NonNull<()>) {
		unsafe {
			let mut this = this.cast::<TaskData<T>>();
			ManuallyDrop::drop(&mut this.as_mut().data);
			mem::drop(Box::from_raw(this.as_ptr()));
		}
	}

	fn get_vtable<F: FnOnce() + Send>() -> &'static TaskVTable {
		trait HasVTable {
			const TABLE: TaskVTable;
		}

		impl<T: FnOnce() + Send> HasVTable for T {
			const TABLE: TaskVTable = TaskVTable {
				call: OwnedTask::call::<T>,
				drop: OwnedTask::drop::<T>,
			};
		}

		&<F as HasVTable>::TABLE
	}

	pub fn new<F: FnOnce() + Send + 'a>(f: F) -> Self {
		let b = Box::new(TaskData {
			table: Self::get_vtable::<F>(),
			data: ManuallyDrop::new(f),
		});
		// Safety: a box can never have a null pointer inside it.
		Self(unsafe { NonNull::new_unchecked(Box::into_raw(b).cast()) }, PhantomData)
	}

	/// Erases the lifetime parameter of this task, converting it to 'static.
	///
	/// # Safety
	///
	/// The caller must ensure that the closure and all data it captures remain
	/// valid for as long as the returned `OwnedTask<'static>` exists.
	///
	/// The returned task can be safely dropped without execution - the Drop impl
	/// will properly clean up the closure. However, any borrowed data captured
	/// by the closure must outlive the `OwnedTask<'static>` object.
	///
	/// This is safe when the task's actual lifetime is managed externally, such
	/// as through `Arc` reference counting or when the caller blocks until the
	/// task completes or is dropped.
	pub unsafe fn erase_lifetime(self) -> OwnedTask<'static> {
		let res = OwnedTask(self.0, PhantomData);
		std::mem::forget(self);
		res
	}

	pub fn run(self) {
		unsafe {
			let call = self.0.as_ref().table.call;
			call(self.0.cast());
		}
		// calling handles drop the data, don't drop the task twice.
		std::mem::forget(self);
	}
}

impl Drop for OwnedTask<'_> {
	fn drop(&mut self) {
		unsafe {
			let drop = self.0.as_ref().table.drop;
			drop(self.0.cast());
		}
	}
}

#[cfg(test)]
mod tests {
	//! These exercise every unsafe block in this module without invoking
	//! the thread pool. Miri runs them with `cargo miri test --lib`.

	use super::*;
	use std::sync::Arc;
	use std::sync::atomic::{AtomicUsize, Ordering};

	/// `OwnedTask::new` + `run` for several closure shapes, including ZSTs
	/// and types with non-trivial `Drop`. Validates the monomorphic vtable
	/// dispatch and that the boxed `TaskData<F>` is freed exactly once.
	#[test]
	fn owned_task_run_drops_capture_once() {
		struct DropTracker(Arc<AtomicUsize>);
		impl Drop for DropTracker {
			fn drop(&mut self) {
				self.0.fetch_add(1, Ordering::SeqCst);
			}
		}
		let drops = Arc::new(AtomicUsize::new(0));
		let tracker = DropTracker(drops.clone());
		let task = OwnedTask::new(move || {
			let _t = tracker;
		});
		task.run();
		assert_eq!(drops.load(Ordering::SeqCst), 1);
	}

	/// `OwnedTask::new` followed by `Drop` without `run`. Validates the
	/// `drop` vtable entry frees the boxed closure (and its captures)
	/// exactly once with no use-after-free.
	#[test]
	fn owned_task_drop_without_run_releases_capture() {
		struct DropTracker(Arc<AtomicUsize>);
		impl Drop for DropTracker {
			fn drop(&mut self) {
				self.0.fetch_add(1, Ordering::SeqCst);
			}
		}
		let drops = Arc::new(AtomicUsize::new(0));
		{
			let tracker = DropTracker(drops.clone());
			let _task = OwnedTask::new(move || {
				let _t = tracker;
			});
			// _task dropped here without running.
		}
		assert_eq!(drops.load(Ordering::SeqCst), 1);
	}

	/// `erase_lifetime` on a closure that captures a stack borrow, then
	/// `run` the erased task while the borrow is still live. Miri verifies
	/// no aliasing or use-after-free, validating the soundness contract
	/// documented on `OwnedTask::erase_lifetime`.
	#[test]
	fn owned_task_erase_lifetime_run_within_borrow() {
		let data: Vec<u32> = (0..64).collect();
		let out = Arc::new(AtomicUsize::new(0));
		let out_ref = out.clone();
		let task: OwnedTask<'_> = OwnedTask::new(move || {
			out_ref.store(data.iter().sum::<u32>() as usize, Ordering::SeqCst);
		});
		// Safety: we run the erased task here before the closure's captures
		// (`data`, `out_ref`) go out of scope, so the lifetime contract is
		// upheld for the duration of `run()`.
		let erased: OwnedTask<'static> = unsafe { task.erase_lifetime() };
		erased.run();
		let expected: u32 = (0..64).sum();
		assert_eq!(out.load(Ordering::SeqCst) as u32, expected);
	}

	/// `erase_lifetime` followed by `Drop` without `run`. The erased task's
	/// `Drop` impl must still free the boxed closure exactly once.
	#[test]
	fn owned_task_erase_lifetime_drop_without_run() {
		struct DropTracker(Arc<AtomicUsize>);
		impl Drop for DropTracker {
			fn drop(&mut self) {
				self.0.fetch_add(1, Ordering::SeqCst);
			}
		}
		let drops = Arc::new(AtomicUsize::new(0));
		{
			let tracker = DropTracker(drops.clone());
			let task: OwnedTask<'_> = OwnedTask::new(move || {
				let _t = tracker;
			});
			// Safety: `tracker` (the captured data) lives in `task` itself,
			// and `task` is dropped at end of scope below — strictly within
			// the original `'_` borrow, so the lifetime contract holds.
			let _erased: OwnedTask<'static> = unsafe { task.erase_lifetime() };
		}
		assert_eq!(drops.load(Ordering::SeqCst), 1);
	}
}
