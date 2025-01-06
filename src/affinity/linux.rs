#![cfg(any(target_os = "android", target_os = "linux"))]

use super::CoreId;
use libc::cpu_set_t;
use libc::sched_getaffinity;
use libc::sched_setaffinity;
use libc::CPU_ISSET;
use libc::CPU_SET;
use libc::CPU_SETSIZE;
use std::mem;

pub fn get_core_ids() -> Option<Vec<CoreId>> {
	if let Some(full_set) = get_affinity_mask() {
		let mut core_ids: Vec<CoreId> = Vec::new();

		for i in 0..CPU_SETSIZE as usize {
			if unsafe { CPU_ISSET(i, &full_set) } {
				core_ids.push(CoreId {
					id: i,
				});
			}
		}

		Some(core_ids)
	} else {
		None
	}
}

pub fn set_for_current(core_id: CoreId) -> bool {
	// Turn `core_id` into a `libc::cpu_set_t` with only
	// one core active.
	let mut set = new_cpu_set();

	unsafe { CPU_SET(core_id.id, &mut set) };

	// Set the current thread's core affinity.
	let res = unsafe {
		sched_setaffinity(
			0, // Defaults to current thread
			mem::size_of::<cpu_set_t>(),
			&set,
		)
	};
	res == 0
}

fn get_affinity_mask() -> Option<cpu_set_t> {
	let mut set = new_cpu_set();

	// Try to get current core affinity mask.
	let result = unsafe {
		sched_getaffinity(
			0, // Defaults to current thread
			mem::size_of::<cpu_set_t>(),
			&mut set,
		)
	};

	if result == 0 {
		Some(set)
	} else {
		None
	}
}

fn new_cpu_set() -> cpu_set_t {
	unsafe { mem::zeroed::<cpu_set_t>() }
}

#[cfg(test)]
mod tests {
	use num_cpus;

	use super::*;

	#[test]
	fn test_linux_get_affinity_mask() {
		match get_affinity_mask() {
			Some(_) => {}
			None => {
				assert!(false);
			}
		}
	}

	#[test]
	fn test_linux_get_core_ids() {
		match get_core_ids() {
			Some(set) => {
				assert_eq!(set.len(), num_cpus::get());
			}
			None => {
				assert!(false);
			}
		}
	}

	#[test]
	fn test_linux_set_for_current() {
		let ids = get_core_ids().unwrap();

		assert!(ids.len() > 0);

		let res = set_for_current(ids[0]);
		assert_eq!(res, true);

		// Ensure that the system pinned the current thread
		// to the specified core.
		let mut core_mask = new_cpu_set();
		unsafe { CPU_SET(ids[0].id, &mut core_mask) };

		let new_mask = get_affinity_mask().unwrap();

		let mut is_equal = true;

		for i in 0..CPU_SETSIZE as usize {
			let is_set1 = unsafe { CPU_ISSET(i, &core_mask) };
			let is_set2 = unsafe { CPU_ISSET(i, &new_mask) };

			if is_set1 != is_set2 {
				is_equal = false;
			}
		}

		assert!(is_equal);
	}
}
