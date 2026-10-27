//! Windows audio input for WinPods (workstream: audio). CONTRACT: the public items in
//! `devices`, `watch` and `capture` are used by the engine and UI; keep signatures.
//!
//! Port of `legacy/python/winpods/capture.py` + `tone.py` on top of the `wasapi`
//! crate: loopback capture of render devices (default device follows Windows
//! default-device changes), recording devices, conversion of any mix format/rate to
//! 44.1 kHz stereo i16 frames pushed into the `rt` ring, and a generated test tone.
//!
//! Threads owned by this crate:
//!
//! * `device-watch` ([`DeviceWatch`]): COM MTA, owns the endpoint enumerator and the
//!   default-device notification registration, answers list/default requests;
//! * `capture` / `tone` ([`CaptureHandle`]): one per running capture, MMCSS
//!   "Pro Audio", the only writer of the ring producer while it runs.

pub mod capture;
mod convert;
pub mod devices;
pub mod resample;
pub mod tone;
pub mod watch;

pub use capture::{CaptureError, CaptureExit, CaptureHandle, CaptureInfo, CaptureSource};
pub use devices::{DeviceInfo, DeviceKind, LOOPBACK_SUFFIX, display_name, find_device};
pub use resample::{OUTPUT_RATE, ResampleError, StereoResampler};
pub use tone::ToneGenerator;
pub use watch::{DeviceEvent, DeviceWatch};

/// Per-thread allocation counter for tests that check real-time paths do not
/// allocate.
#[cfg(test)]
mod test_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    }

    struct Counting;

    fn count() {
        // `try_with`: allocations can happen while thread-locals are torn down.
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
    }

    // SAFETY: every method forwards to the system allocator unchanged.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count();
            unsafe { System.realloc(ptr, layout, new_size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    /// Allocations made by the current thread so far.
    pub(crate) fn allocations() -> u64 {
        ALLOCATIONS.with(Cell::get)
    }
}
