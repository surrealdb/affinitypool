#![cfg(target_os = "windows")]

use winapi::shared::basetsd::{DWORD_PTR, PDWORD_PTR};
use winapi::um::processthreadsapi::{GetCurrentProcess, GetCurrentThread};
use winapi::um::winbase::{GetProcessAffinityMask, SetThreadAffinityMask};

use super::CoreId;

pub fn get_core_ids() -> Option<Vec<CoreId>> {
	if let Some(mask) = get_affinity_mask() {
		// Find all active cores in the bitmask.
		let mut core_ids: Vec<CoreId> = Vec::new();

		for i in 0..64 as u64 {
			let test_mask = 1 << i;

			if (mask & test_mask) == test_mask {
				core_ids.push(CoreId {
					id: i as usize,
				});
			}
		}

		Some(core_ids)
	} else {
		None
	}
}

pub fn set_for_current(core_id: CoreId) -> bool {
	// Convert `CoreId` back into mask.
	let mask: u64 = 1 << core_id.id;

	// Set core affinity for current thread.
	let res = unsafe { SetThreadAffinityMask(GetCurrentThread(), mask as DWORD_PTR) };
	res != 0
}

fn get_affinity_mask() -> Option<u64> {
	let mut system_mask: usize = 0;
	let mut process_mask: usize = 0;

	let res = unsafe {
		GetProcessAffinityMask(
			GetCurrentProcess(),
			&mut process_mask as PDWORD_PTR,
			&mut system_mask as PDWORD_PTR,
		)
	};

	// Successfully retrieved affinity mask
	if res != 0 {
		Some(process_mask as u64)
	}
	// Failed to retrieve affinity mask
	else {
		None
	}
}

#[cfg(test)]
mod tests {
	use num_cpus;

	use super::*;

	#[test]
	fn test_windows_get_core_ids() {
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
	fn test_windows_set_for_current() {
		let ids = get_core_ids().unwrap();

		assert!(ids.len() > 0);

		assert!(set_for_current(ids[0]));
	}
}
