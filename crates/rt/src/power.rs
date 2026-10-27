//! Process power-throttling control.
//!
//! While streaming, WinPods opts out of EcoQoS execution-speed throttling and asks
//! Windows to honour timer-resolution requests even when the window is hidden.
//! The setting is restored when the last guard is dropped.

use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// While at least one guard is alive the process is excluded from power throttling.
pub struct ThrottleGuard(());

impl ThrottleGuard {
    pub fn streaming() -> Self {
        if ACTIVE.fetch_add(1, Ordering::SeqCst) == 0 {
            apply(
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                    | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            );
        }
        Self(())
    }
}

impl Drop for ThrottleGuard {
    fn drop(&mut self) {
        if ACTIVE.fetch_sub(1, Ordering::SeqCst) == 1 {
            apply(0); // ControlMask 0: let Windows decide again
        }
    }
}

/// ControlMask = the policies we take control of; StateMask = 0 turns them *off*.
fn apply(control_mask: u32) -> bool {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: control_mask,
        StateMask: 0,
    };
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_guards_apply_and_restore() {
        let a = ThrottleGuard::streaming();
        let b = ThrottleGuard::streaming();
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 2);
        drop(a);
        drop(b);
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
    }
}
