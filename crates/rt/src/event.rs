//! Auto-reset Win32 events used to wake real-time threads.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};
use windows::core::PCWSTR;

/// An unnamed auto-reset event: one `set()` releases exactly one waiter.
pub struct AutoEvent(HANDLE);

// The handle is a kernel object; Win32 event calls are thread-safe.
unsafe impl Send for AutoEvent {}
unsafe impl Sync for AutoEvent {}

impl AutoEvent {
    pub fn new() -> windows::core::Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
        Ok(Self(handle))
    }

    /// Signal the event (wakes one waiter, or the next one to wait).
    pub fn set(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }

    /// Clear a pending signal.
    pub fn reset(&self) {
        unsafe {
            let _ = ResetEvent(self.0);
        }
    }

    pub fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for AutoEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wait::{Wait, wait_any};
    use std::time::Duration;

    #[test]
    fn set_wakes_exactly_once() {
        let ev = AutoEvent::new().unwrap();
        ev.set();
        let h = [ev.handle()];
        assert_eq!(
            wait_any(&h, Some(Duration::from_millis(50))).unwrap(),
            Wait::Signaled(0)
        );
        // auto-reset: a second wait times out
        assert_eq!(
            wait_any(&h, Some(Duration::from_millis(20))).unwrap(),
            Wait::Timeout
        );
    }

    #[test]
    fn reset_clears_pending_signal() {
        let ev = AutoEvent::new().unwrap();
        ev.set();
        ev.reset();
        let h = [ev.handle()];
        assert_eq!(
            wait_any(&h, Some(Duration::from_millis(20))).unwrap(),
            Wait::Timeout
        );
    }
}
