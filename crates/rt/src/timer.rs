//! High-resolution waitable timers.
//!
//! `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timers fire with sub-millisecond accuracy
//! regardless of the global timer resolution, and keep working for hidden or
//! minimised processes (where Windows 11 ignores `timeBeginPeriod`).

use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimerEx,
    TIMER_ALL_ACCESS,
};
use windows::core::PCWSTR;

pub struct HiResTimer {
    handle: HANDLE,
    high_resolution: bool,
}

unsafe impl Send for HiResTimer {}
unsafe impl Sync for HiResTimer {}

impl HiResTimer {
    /// Create a one-shot timer; falls back to a normal timer on systems without
    /// high-resolution support (pre-1803 Windows 10).
    pub fn new() -> windows::core::Result<Self> {
        let access = TIMER_ALL_ACCESS.0;
        match unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                access,
            )
        } {
            Ok(handle) => Ok(Self {
                handle,
                high_resolution: true,
            }),
            Err(_) => {
                let handle = unsafe { CreateWaitableTimerExW(None, PCWSTR::null(), 0, access)? };
                Ok(Self {
                    handle,
                    high_resolution: false,
                })
            }
        }
    }

    /// Arm the timer to signal once, `after` from now (re-arming replaces the old due time).
    pub fn arm(&self, after: Duration) -> windows::core::Result<()> {
        // Negative due time = relative, in 100 ns units; at least one unit.
        let units = (after.as_nanos() / 100).max(1) as i64;
        let due = -units;
        unsafe { SetWaitableTimerEx(self.handle, &due, 0, None, None, None, 0) }
    }

    pub fn handle(&self) -> HANDLE {
        self.handle
    }

    pub fn is_high_resolution(&self) -> bool {
        self.high_resolution
    }
}

impl Drop for HiResTimer {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::mono_ns;
    use crate::wait::{Wait, wait_any};

    #[test]
    fn fires_close_to_due_time() {
        let timer = HiResTimer::new().unwrap();
        let mut worst = 0u64;
        for _ in 0..20 {
            let start = mono_ns();
            timer.arm(Duration::from_millis(4)).unwrap();
            let r = wait_any(&[timer.handle()], Some(Duration::from_millis(500))).unwrap();
            assert_eq!(r, Wait::Signaled(0));
            let elapsed = mono_ns() - start;
            assert!(elapsed >= 3_500_000, "fired early after {elapsed} ns");
            worst = worst.max(elapsed);
        }
        if timer.is_high_resolution() {
            // Generous bound for loaded CI machines; locally this is ~4.1 ms.
            assert!(worst < 12_000_000, "worst wake {worst} ns");
        }
    }

    #[test]
    fn rearm_replaces_due_time() {
        let timer = HiResTimer::new().unwrap();
        timer.arm(Duration::from_secs(10)).unwrap();
        timer.arm(Duration::from_millis(2)).unwrap();
        let r = wait_any(&[timer.handle()], Some(Duration::from_millis(500))).unwrap();
        assert_eq!(r, Wait::Signaled(0));
    }
}
