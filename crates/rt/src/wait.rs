//! Waiting on several kernel objects at once.

use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

/// Outcome of [`wait_any`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The handle at this index was signaled (lowest index wins if several are).
    Signaled(usize),
    Timeout,
}

/// Wait until any of `handles` is signaled or `timeout` passes (`None` = forever).
///
/// The timeout has millisecond granularity and is subject to the system timer
/// resolution; for precise short waits include a [`crate::HiResTimer`] handle instead.
pub fn wait_any(handles: &[HANDLE], timeout: Option<Duration>) -> std::io::Result<Wait> {
    assert!(!handles.is_empty() && handles.len() <= 64, "1..=64 handles");
    let ms = match timeout {
        None => INFINITE,
        Some(d) => d.as_millis().min(u128::from(INFINITE - 1)) as u32,
    };
    let rc = unsafe { WaitForMultipleObjects(handles, false, ms) };
    if rc == WAIT_TIMEOUT {
        return Ok(Wait::Timeout);
    }
    if rc == WAIT_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    let index = rc.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
    if index < handles.len() {
        Ok(Wait::Signaled(index))
    } else {
        // WAIT_ABANDONED_* only applies to mutexes, which we never wait on here.
        Err(std::io::Error::other(format!(
            "unexpected wait result {:#x}",
            rc.0
        )))
    }
}
