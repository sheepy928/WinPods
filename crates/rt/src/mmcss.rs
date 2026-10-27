//! Thread scheduling: MMCSS registration and plain thread priorities.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AVRT_PRIORITY_HIGH, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    AvSetMmThreadPriority, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY,
    THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::core::w;

/// Registers the *current* thread with the Multimedia Class Scheduler Service under
/// the "Pro Audio" task for as long as the guard lives. Must be dropped on the same
/// thread that created it.
pub struct MmcssGuard {
    handle: HANDLE,
    // !Send: MMCSS registration is per-thread.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl MmcssGuard {
    /// Join the "Pro Audio" MMCSS task at high priority. Returns `None` if the
    /// service refuses (e.g. it is disabled); callers continue without it.
    pub fn pro_audio() -> Option<Self> {
        let mut task_index = 0u32;
        let handle =
            unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok()?;
        unsafe {
            let _ = AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH);
        }
        Some(Self {
            handle,
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.handle);
        }
    }
}

/// Plain Win32 thread priorities for threads that should not join MMCSS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPriority {
    AboveNormal,
    Highest,
    TimeCritical,
}

/// Set the priority of the calling thread. Returns false if Windows refused.
pub fn set_current_thread_priority(priority: ThreadPriority) -> bool {
    let value: THREAD_PRIORITY = match priority {
        ThreadPriority::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL,
        ThreadPriority::Highest => THREAD_PRIORITY_HIGHEST,
        ThreadPriority::TimeCritical => THREAD_PRIORITY_TIME_CRITICAL,
    };
    unsafe { SetThreadPriority(GetCurrentThread(), value).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmcss_registration_round_trip() {
        // MMCSS can be unavailable on locked-down CI images; only check it does not crash.
        let guard = MmcssGuard::pro_audio();
        drop(guard);
    }

    #[test]
    fn thread_priority_can_be_raised() {
        std::thread::spawn(|| assert!(set_current_thread_priority(ThreadPriority::AboveNormal)))
            .join()
            .unwrap();
    }
}
