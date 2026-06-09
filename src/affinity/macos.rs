#![cfg(target_os = "macos")]

use std::mem;

use libc::{c_int, c_uint, pthread_mach_thread_np, pthread_self};

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
const KERN_SUCCESS: kern_return_t = 0;
// `KERN_NOT_SUPPORTED` from `mach/kern_return.h`. Apple Silicon returns
// this for `THREAD_AFFINITY_POLICY` (no hardware affinity support). The
// previous value (268435459 = 0x10000003) was actually
// `MACH_SEND_INVALID_DEST` — the symptom of passing a bogus thread port
// (`pthread_self()` cast straight to `thread_t`), now fixed by routing
// through `pthread_mach_thread_np`.
const KERN_NOT_SUPPORTED: kern_return_t = 46;

unsafe extern "C" {
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
			.map(|n| CoreId {
				id: n,
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

	// `thread_policy_set` expects a mach thread port, NOT a `pthread_t`.
	// Convert via `pthread_mach_thread_np`; casting `pthread_self()`
	// (an opaque pointer) straight to `thread_t` yields a bogus port and
	// silently fails on Intel.
	let res = unsafe {
		thread_policy_set(
			pthread_mach_thread_np(pthread_self()) as thread_t,
			THREAD_AFFINITY_POLICY,
			&mut info as thread_policy_t,
			thread_affinity_policy_count,
		)
	};

	// On Apple Silicon (ARM64), thread affinity is not supported and returns KERN_NOT_SUPPORTED.
	// We treat this as a successful operation since the hardware doesn't support manual affinity control.
	match res {
		KERN_SUCCESS => true,
		KERN_NOT_SUPPORTED => true, // Treat as success on unsupported platforms (Apple Silicon)
		_ => false,
	}
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
				panic!("Failed to get core IDs");
			}
		}
	}

	#[test]
	fn test_macos_set_for_current() {
		let ids = get_core_ids().unwrap();
		assert!(!ids.is_empty());
		assert!(set_for_current(ids[0]))
	}
}
