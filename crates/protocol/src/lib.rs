//! The protocol between the Argon analyzer and its GUI.
//!
//! These types compile for the web, so a GUI running in a browser speaks the
//! same protocol as the native one.

pub mod config;
pub mod rpc;
pub mod wire;

/// Returns memory the allocator has freed but still holds to the operating
/// system. A large layout update frees many pages the allocator would keep.
pub fn release_free_memory() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone names every malloc zone, and a zero goal asks
        // for as much as can be returned.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
}
