//! Single instance + activation of the running instance. CONTRACT.
//!
//! Mutex `Local\WinPods-single-instance` (same name as v0.1, so the two versions never
//! run together; ERROR_ACCESS_DENIED counts as "already exists"). The primary creates
//! the auto-reset event `Local\WinPods-activate` before taking the mutex and an ack
//! event `Local\WinPods-activated`; a secondary sets the activate event (after
//! `AllowSetForegroundWindow(ASFW_ANY)`) and waits for the ack.

use std::time::Duration;

/// Result of trying to become the primary instance.
pub enum Instance {
    Primary(InstanceGuard),
    Secondary,
}

/// Held by the primary instance; dropping it releases the mutex and stops the
/// activation listener.
pub struct InstanceGuard {
    _private: (),
}

impl InstanceGuard {
    /// Called on a background thread whenever another launch asks us to show the
    /// window; the ack is sent automatically after the handler returns.
    pub fn on_activate(&self, handler: Box<dyn Fn() + Send + 'static>) {
        let _ = handler;
        todo!("single_instance: on_activate")
    }
}

/// Try to become the primary instance. With `retry_for` > 0 (after an update hand-off)
/// keep retrying while the old process exits.
pub fn acquire(retry_for: Duration) -> std::io::Result<Instance> {
    let _ = retry_for;
    todo!("single_instance: acquire")
}

/// Ask the primary instance to show its window. Returns true if it acknowledged
/// within `wait_ack` (false = an older/elevated/hung instance holds the mutex).
pub fn signal_existing(wait_ack: Duration) -> bool {
    let _ = wait_ack;
    todo!("single_instance: signal_existing")
}
