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
	/// This is extremely unsafe! The caller must ensure:
	/// 1. The closure within the task remains valid for the entire lifetime of the returned object
	/// 2. Any data captured by the closure outlives the task execution
	/// 3. The task will be executed before any referenced data is deallocated
	/// 4. Proper synchronization exists to prevent use-after-free
	///
	/// In practice, this should only be used when:
	/// - The task is immediately queued for execution
	/// - There's a synchronization mechanism (like Drop blocking) that ensures task completion
	/// - The threadpool joins all threads before deallocation
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
