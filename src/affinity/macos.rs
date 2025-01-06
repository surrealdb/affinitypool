#![cfg(target_os = "macos")]

use std::mem;

use libc::{c_int, c_uint, pthread_self};

use num_cpus;

use super::CoreId;

type kern_return_t = c_int;
type integer_t = c_int;
type natural_t = c_uint;
type thread_t = c_uint;
type thread_policy_flavor_t = natural_t;
type mach_msg_type_number_t = natural_t;

#[repr(C)]
struct thread_affinity_policy_data_t {
	affinity_tag: integer_t,
}

type thread_policy_t = *mut thread_affinity_policy_data_t;

const THREAD_AFFINITY_POLICY: thread_policy_flavor_t = 4;

extern "C" {
	fn thread_policy_set(
		thread: thread_t,
		flavor: thread_policy_flavor_t,
		policy_info: thread_policy_t,
		count: mach_msg_type_number_t,
	) -> kern_return_t;
}

pub fn get_core_ids() -> Option<Vec<CoreId>> {
	Some(
		(0..(num_cpus::get()))
			.into_iter()
			.map(|n| CoreId {
				id: n as usize,
			})
			.collect::<Vec<_>>(),
	)
}

pub fn set_for_current(core_id: CoreId) -> bool {
	let thread_affinity_policy_count: mach_msg_type_number_t =
		mem::size_of::<thread_affinity_policy_data_t>() as mach_msg_type_number_t
			/ mem::size_of::<integer_t>() as mach_msg_type_number_t;

	let mut info = thread_affinity_policy_data_t {
		affinity_tag: core_id.id as integer_t,
	};

	let res = unsafe {
		thread_policy_set(
			pthread_self() as thread_t,
			THREAD_AFFINITY_POLICY,
			&mut info as thread_policy_t,
			thread_affinity_policy_count,
		)
	};
	res == 0
}

#[cfg(test)]
mod tests {
	use num_cpus;

	use super::*;

	#[test]
	fn test_macos_get_core_ids() {
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
	fn test_macos_set_for_current() {
		let ids = get_core_ids().unwrap();
		assert!(ids.len() > 0);
		assert!(set_for_current(ids[0]))
	}
}
