//! Process power-throttling control.
//!
//! While streaming, WinPods opts out of EcoQoS execution-speed throttling and asks
//! Windows to honour timer-resolution requests even when the window is hidden. The
//! timer-resolution policy only exists on Windows 11; where Windows rejects the
//! combined request, the execution-speed opt-out is applied on its own. What Windows
//! accepted is kept for the caller to log ([`ThrottleGuard::opt_out`]). The setting
//! is restored when the last guard is dropped.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// Outcome of the latest opt-out (`None` before the first guard).
static OPT_OUT: Mutex<Option<ThrottleOptOut>> = Mutex::new(None);

/// What Windows accepted when the power-throttling opt-out was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleOptOut {
    /// Execution-speed throttling is off and timer-resolution requests are honoured.
    Full,
    /// Execution-speed throttling is off; Windows rejected the timer-resolution policy
    /// (it only exists on Windows 11) with this HRESULT.
    ExecutionSpeedOnly { hresult: i32 },
    /// Windows rejected the opt-out entirely (HRESULT of the execution-speed request).
    Failed { hresult: i32 },
}

/// While at least one guard is alive the process is excluded from power throttling.
pub struct ThrottleGuard(());

impl ThrottleGuard {
    pub fn streaming() -> Self {
        if ACTIVE.fetch_add(1, Ordering::SeqCst) == 0 {
            let outcome = opt_out_with(apply);
            *OPT_OUT.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        }
        Self(())
    }

    /// What Windows accepted the last time the opt-out was applied. `None` only while
    /// the first guard of another thread is still applying it.
    pub fn opt_out(&self) -> Option<ThrottleOptOut> {
        *OPT_OUT.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for ThrottleGuard {
    fn drop(&mut self) {
        if ACTIVE.fetch_sub(1, Ordering::SeqCst) == 1 {
            // ControlMask 0: let Windows decide again. Nothing to do if it refuses.
            let _ = apply(0);
        }
    }
}

/// Opt out of execution-speed throttling and timer-resolution ignoring in one call,
/// falling back to execution speed alone when Windows rejects the combination.
fn opt_out_with(mut apply: impl FnMut(u32) -> Result<(), i32>) -> ThrottleOptOut {
    let execution_speed = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
    match apply(execution_speed | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION) {
        Ok(()) => ThrottleOptOut::Full,
        Err(combined) => match apply(execution_speed) {
            Ok(()) => ThrottleOptOut::ExecutionSpeedOnly { hresult: combined },
            Err(hresult) => ThrottleOptOut::Failed { hresult },
        },
    }
}

/// ControlMask = the policies we take control of; StateMask = 0 turns them *off*.
/// Returns the HRESULT on failure.
fn apply(control_mask: u32) -> Result<(), i32> {
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
    }
    .map_err(|e| e.code().0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: u32 =
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
    const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
    const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;

    #[test]
    fn nested_guards_apply_and_restore() {
        let a = ThrottleGuard::streaming();
        let b = ThrottleGuard::streaming();
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 2);
        // Every supported Windows accepts at least the execution-speed opt-out.
        let outcome = a.opt_out().expect("applied by the first guard");
        assert!(
            !matches!(outcome, ThrottleOptOut::Failed { .. }),
            "{outcome:?}"
        );
        assert_eq!(b.opt_out(), Some(outcome));
        drop(a);
        drop(b);
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn combined_request_is_tried_first() {
        let mut calls = Vec::new();
        let outcome = opt_out_with(|mask| {
            calls.push(mask);
            Ok(())
        });
        assert_eq!(outcome, ThrottleOptOut::Full);
        assert_eq!(calls, [BOTH]);
    }

    #[test]
    fn execution_speed_alone_when_the_timer_policy_is_rejected() {
        // Windows 10 rejects PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, which used
        // to discard the execution-speed opt-out with it.
        let mut calls = Vec::new();
        let outcome = opt_out_with(|mask| {
            calls.push(mask);
            if mask == BOTH {
                Err(E_INVALIDARG)
            } else {
                Ok(())
            }
        });
        assert_eq!(
            outcome,
            ThrottleOptOut::ExecutionSpeedOnly {
                hresult: E_INVALIDARG
            }
        );
        assert_eq!(calls, [BOTH, PROCESS_POWER_THROTTLING_EXECUTION_SPEED]);
    }

    #[test]
    fn failure_of_both_requests_is_reported() {
        let mut calls = 0;
        let outcome = opt_out_with(|mask| {
            calls += 1;
            Err(if mask == BOTH {
                E_INVALIDARG
            } else {
                E_ACCESSDENIED
            })
        });
        assert_eq!(
            outcome,
            ThrottleOptOut::Failed {
                hresult: E_ACCESSDENIED
            }
        );
        assert_eq!(calls, 2);
    }
}
