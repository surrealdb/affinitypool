#![cfg(target_os = "freebsd")]

use super::CoreId;
use libc::CPU_ISSET;
use libc::CPU_LEVEL_WHICH;
use libc::CPU_SET;
use libc::CPU_SETSIZE;
use libc::CPU_WHICH_TID;
use libc::cpu_set_t;
use libc::sched_getaffinity;
use libc::sched_setaffinity;
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
	// Turn `core_id` into a `libc::cpuset_t` with only
	// one core active.
	let mut set = new_cpu_set();

	unsafe { CPU_SET(core_id.id, &mut set) };

	// Set the current thread's core affinity.
	let res = unsafe {
		// FreeBSD's sched_setaffinity currently operates on process id,
		// therefore using cpuset_setaffinity instead.
		cpuset_setaffinity(
			CPU_LEVEL_WHICH,
			CPU_WHICH_TID,
			-1, // -1 == current thread
			mem::size_of::<cpuset_t>(),
			&set,
		)
	};
	res == 0
}

fn get_affinity_mask() -> Option<cpuset_t> {
	let mut set = new_cpu_set();

	// Try to get current core affinity mask.
	let result = unsafe {
		// FreeBSD's sched_getaffinity currently operates on process id,
		// therefore using cpuset_getaffinity instead.
		cpuset_getaffinity(
			CPU_LEVEL_WHICH,
			CPU_WHICH_TID,
			-1, // -1 == current thread
			mem::size_of::<cpuset_t>(),
			&mut set,
		)
	};

	if result == 0 {
		Some(set)
	} else {
		None
	}
}

fn new_cpu_set() -> cpuset_t {
	unsafe { mem::zeroed::<cpuset_t>() }
}

#[cfg(test)]
mod tests {
	use num_cpus;

	use super::*;

	#[test]
	fn test_freebsd_get_affinity_mask() {
		match get_affinity_mask() {
			Some(_) => {}
			None => {
				assert!(false);
			}
		}
	}

	#[test]
	fn test_freebsd_get_core_ids() {
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
	fn test_freebsd_set_for_current() {
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
