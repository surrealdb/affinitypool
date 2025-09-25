#![allow(unused)]
#![allow(non_camel_case_types)]

//! This module manages CPU affinities.
//!
//! ```
//! use std::thread;
//!
//! // Retrieve the IDs of all active CPU cores.
//! let core_ids = affinitypool::affinity::get_core_ids().unwrap();
//!
//! // Create a thread for each active CPU core.
//! let handles = core_ids.into_iter().map(|id| {
//!     thread::spawn(move || {
//!         // Pin this thread to a single CPU core.
//!         let res = affinitypool::affinity::set_for_current(id);
//!         if res {
//!             // Do more work after this.
//!         }
//!     })
//! }).collect::<Vec<_>>();
//!
//! for handle in handles.into_iter() {
//!     handle.join().unwrap();
//! }
//! ```

mod freebsd;
mod linux;
mod macos;
mod windows;

/// This function tries to retrieve information
/// on all the "cores" on which the current thread
/// is allowed to run.
pub fn get_core_ids() -> Option<Vec<CoreId>> {
	get_core_ids_helper()
}

/// This function tries to pin the current
/// thread to the specified core.
///
/// # Arguments
///
/// * core_id - ID of the core to pin
pub fn set_for_current(core_id: CoreId) -> bool {
	set_for_current_helper(core_id)
}

/// This represents a CPU core.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoreId {
	pub id: usize,
}

impl From<usize> for CoreId {
	fn from(id: usize) -> CoreId {
		CoreId {
			id,
		}
	}
}

// Linux Section

#[cfg(any(target_os = "android", target_os = "linux"))]
#[inline]
fn get_core_ids_helper() -> Option<Vec<CoreId>> {
	linux::get_core_ids()
}

#[cfg(any(target_os = "android", target_os = "linux"))]
#[inline]
fn set_for_current_helper(core_id: CoreId) -> bool {
	linux::set_for_current(core_id)
}

// Windows Section

#[cfg(target_os = "windows")]
#[inline]
fn get_core_ids_helper() -> Option<Vec<CoreId>> {
	windows::get_core_ids()
}

#[cfg(target_os = "windows")]
#[inline]
fn set_for_current_helper(core_id: CoreId) -> bool {
	windows::set_for_current(core_id)
}

// MacOS Section

#[cfg(target_os = "macos")]
#[inline]
fn get_core_ids_helper() -> Option<Vec<CoreId>> {
	macos::get_core_ids()
}

#[cfg(target_os = "macos")]
#[inline]
fn set_for_current_helper(core_id: CoreId) -> bool {
	macos::set_for_current(core_id)
}

// FreeBSD Section

#[cfg(target_os = "freebsd")]
#[inline]
fn get_core_ids_helper() -> Option<Vec<CoreId>> {
	freebsd::get_core_ids()
}

#[cfg(target_os = "freebsd")]
#[inline]
fn set_for_current_helper(core_id: CoreId) -> bool {
	freebsd::set_for_current(core_id)
}

// Stub Section

#[cfg(not(any(
	target_os = "linux",
	target_os = "android",
	target_os = "windows",
	target_os = "macos",
	target_os = "freebsd"
)))]
#[inline]
fn get_core_ids_helper() -> Option<Vec<CoreId>> {
	None
}

#[cfg(not(any(
	target_os = "linux",
	target_os = "android",
	target_os = "windows",
	target_os = "macos",
	target_os = "freebsd"
)))]
#[inline]
fn set_for_current_helper(_core_id: CoreId) -> bool {
	false
}
