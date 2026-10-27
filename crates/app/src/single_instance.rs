//! Single instance + activation of the running instance. CONTRACT.
//!
//! Mutex `Local\WinPods-single-instance` (same name as v0.1, so the two versions never
//! run together; ERROR_ACCESS_DENIED counts as "already exists"). The primary creates
//! the auto-reset event `Local\WinPods-activate` before taking the mutex and an ack
//! event `Local\WinPods-activated`; a secondary sets the activate event (after
//! `AllowSetForegroundWindow(ASFW_ANY)`) and waits for the ack.
//!
//! The mutex is used for its existence only (never owned/waited on), exactly like
//! v0.1: the name exists while the primary holds its handle. An activation that
//! arrives before [`InstanceGuard::on_activate`] registers a handler is acknowledged
//! at once and delivered to the handler as soon as it is registered.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, SetLastError, WAIT_OBJECT_0,
    WIN32_ERROR,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, ResetEvent,
    SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows::core::{HRESULT, PCWSTR};

use crate::win::{self, OwnedHandle};

/// Name of the single-instance mutex (shared with v0.1).
pub const MUTEX_NAME: &str = r"Local\WinPods-single-instance";
/// Auto-reset event a second launch sets to ask the primary to show its window.
pub const ACTIVATE_EVENT_NAME: &str = r"Local\WinPods-activate";
/// Auto-reset event the primary sets once it handled an activation.
pub const ACK_EVENT_NAME: &str = r"Local\WinPods-activated";

/// Result of trying to become the primary instance.
pub enum Instance {
    Primary(InstanceGuard),
    Secondary,
}

type Handler = Box<dyn Fn() + Send + 'static>;

/// State shared between the guard and its listener thread.
struct Shared {
    handler: Mutex<Option<Handler>>,
    /// An activation arrived while no handler was registered.
    pending: AtomicBool,
}

/// Held by the primary instance; dropping it releases the mutex and stops the
/// activation listener.
pub struct InstanceGuard {
    shared: Arc<Shared>,
    activate: Arc<OwnedHandle>,
    stop: Arc<OwnedHandle>,
    listener: Option<JoinHandle<()>>,
    /// Declared last so it is closed after the listener has stopped.
    _mutex: OwnedHandle,
}

impl InstanceGuard {
    /// Called on a background thread whenever another launch asks us to show the
    /// window; the ack is sent automatically after the handler returns.
    pub fn on_activate(&self, handler: Box<dyn Fn() + Send + 'static>) {
        *self
            .shared
            .handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handler);
        if self.shared.pending.swap(false, Ordering::AcqRel) {
            // Replay the early activation on the listener thread.
            // SAFETY: the event handle is valid for the guard's lifetime.
            unsafe {
                let _ = SetEvent(self.activate.raw());
            }
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: the stop event is valid until `self.stop` is dropped below.
        unsafe {
            let _ = SetEvent(self.stop.raw());
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

/// Kernel object names. Injectable so tests use unique names.
#[derive(Debug, Clone)]
pub(crate) struct Names {
    pub(crate) mutex: String,
    pub(crate) activate: String,
    pub(crate) ack: String,
}

impl Names {
    fn real() -> Self {
        Self {
            mutex: MUTEX_NAME.into(),
            activate: ACTIVATE_EVENT_NAME.into(),
            ack: ACK_EVENT_NAME.into(),
        }
    }
}

/// Try to become the primary instance. With `retry_for` > 0 (after an update hand-off)
/// keep retrying while the old process exits.
pub fn acquire(retry_for: Duration) -> std::io::Result<Instance> {
    acquire_with(&Names::real(), retry_for)
}

/// Ask the primary instance to show its window. Returns true if it acknowledged
/// within `wait_ack` (false = an older/elevated/hung instance holds the mutex).
pub fn signal_existing(wait_ack: Duration) -> bool {
    // Let the primary bring its window to the foreground (we are the process the
    // user just started, so we hold the foreground right).
    // SAFETY: plain Win32 call without pointers.
    if let Err(e) = unsafe { AllowSetForegroundWindow(ASFW_ANY) } {
        tracing::debug!("AllowSetForegroundWindow failed: {e}");
    }
    signal_with(&Names::real(), wait_ack)
}

pub(crate) fn acquire_with(names: &Names, retry_for: Duration) -> io::Result<Instance> {
    let deadline = Instant::now() + retry_for;
    loop {
        if let Some(guard) = try_acquire(names)? {
            return Ok(Instance::Primary(guard));
        }
        if Instant::now() >= deadline {
            return Ok(Instance::Secondary);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_access_denied(e: &windows::core::Error) -> bool {
    e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
}

/// Create (or open) a named auto-reset event; `Ok(None)` on access denied (the object
/// belongs to an elevated instance).
fn create_event(name: &str) -> io::Result<Option<OwnedHandle>> {
    let name_w = win::wide(name.as_ref());
    // SAFETY: `name_w` is NUL-terminated and outlives the call.
    match unsafe { CreateEventW(None, false, false, PCWSTR(name_w.as_ptr())) } {
        Ok(handle) => Ok(Some(OwnedHandle(handle))),
        Err(e) if is_access_denied(&e) => Ok(None),
        Err(e) => Err(win::to_io(e)),
    }
}

/// One attempt: events first (so a secondary that sees the mutex always finds them),
/// then the mutex. `Ok(None)` = another instance exists.
fn try_acquire(names: &Names) -> io::Result<Option<InstanceGuard>> {
    let Some(activate) = create_event(&names.activate)? else {
        return Ok(None);
    };
    let Some(ack) = create_event(&names.ack)? else {
        return Ok(None);
    };

    let mutex_w = win::wide(names.mutex.as_ref());
    // SAFETY: `mutex_w` is NUL-terminated; the last error is cleared first so
    // ERROR_ALREADY_EXISTS is read from this very call.
    let created = unsafe {
        SetLastError(WIN32_ERROR(0));
        CreateMutexW(None, false, PCWSTR(mutex_w.as_ptr())).map(|h| (h, GetLastError()))
    };
    let mutex = match created {
        Ok((handle, last_error)) => {
            let handle = OwnedHandle(handle);
            if last_error == ERROR_ALREADY_EXISTS {
                return Ok(None);
            }
            handle
        }
        Err(e) if is_access_denied(&e) => return Ok(None),
        Err(e) => return Err(win::to_io(e)),
    };

    // SAFETY: plain Win32 call creating an unnamed manual-reset event.
    let stop = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(win::to_io)?;
    let stop = Arc::new(OwnedHandle(stop));
    let activate = Arc::new(activate);
    let shared = Arc::new(Shared {
        handler: Mutex::new(None),
        pending: AtomicBool::new(false),
    });

    let listener = {
        let (shared, activate, stop) = (shared.clone(), activate.clone(), stop.clone());
        std::thread::Builder::new()
            .name("winpods-activate".into())
            .spawn(move || listen(&shared, &activate, &ack, &stop))?
    };

    Ok(Some(InstanceGuard {
        shared,
        activate,
        stop,
        listener: Some(listener),
        _mutex: mutex,
    }))
}

/// Listener thread: wait for stop or activate; deliver activations, then ack.
fn listen(shared: &Shared, activate: &OwnedHandle, ack: &OwnedHandle, stop: &OwnedHandle) {
    let handles = [stop.raw(), activate.raw()];
    loop {
        // SAFETY: both handles stay valid while this thread runs (the guard joins it
        // before dropping them).
        let wait = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            return;
        }
        if wait.0 != WAIT_OBJECT_0.0 + 1 {
            tracing::error!("activation listener: wait failed ({:#x})", wait.0);
            return;
        }
        deliver(shared);
        // SAFETY: valid event handle (see above).
        unsafe {
            let _ = SetEvent(ack.raw());
        }
    }
}

/// Run the handler (outside the lock, so it may call `on_activate`), or remember the
/// activation until one is registered. A panicking handler is logged, not fatal.
fn deliver(shared: &Shared) {
    let taken = shared
        .handler
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let Some(handler) = taken else {
        shared.pending.store(true, Ordering::Release);
        return;
    };
    if catch_unwind(AssertUnwindSafe(&handler)).is_err() {
        tracing::error!("the activation handler panicked");
    }
    let mut slot = shared
        .handler
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(handler);
    }
}

/// Open an existing named event with `access`; `None` when it does not exist.
fn open_event(
    name: &str,
    access: windows::Win32::System::Threading::SYNCHRONIZATION_ACCESS_RIGHTS,
) -> Option<OwnedHandle> {
    let name_w = win::wide(name.as_ref());
    // SAFETY: `name_w` is NUL-terminated and outlives the call.
    unsafe { OpenEventW(access, false, PCWSTR(name_w.as_ptr())) }
        .ok()
        .map(OwnedHandle)
}

pub(crate) fn signal_with(names: &Names, wait_ack: Duration) -> bool {
    // A v0.1 instance (or none) has no events: nothing to signal.
    let Some(activate) = open_event(&names.activate, EVENT_MODIFY_STATE) else {
        return false;
    };
    let Some(ack) = open_event(&names.ack, EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE) else {
        return false;
    };
    let millis = u32::try_from(wait_ack.as_millis())
        .unwrap_or(INFINITE - 1)
        .min(INFINITE - 1);
    // SAFETY: both handles are valid; the calls take no pointers.
    unsafe {
        // Drop a stale ack from an earlier, timed-out signal.
        let _ = ResetEvent(ack.raw());
        if SetEvent(activate.raw()).is_err() {
            return false;
        }
        WaitForSingleObject(ack.raw(), millis) == WAIT_OBJECT_0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_suffix;
    use std::sync::atomic::AtomicUsize;

    fn test_names() -> Names {
        let id = unique_suffix();
        Names {
            mutex: format!(r"Local\WinPods-test-{id}-single-instance"),
            activate: format!(r"Local\WinPods-test-{id}-activate"),
            ack: format!(r"Local\WinPods-test-{id}-activated"),
        }
    }

    fn primary(names: &Names) -> InstanceGuard {
        match acquire_with(names, Duration::ZERO).unwrap() {
            Instance::Primary(guard) => guard,
            Instance::Secondary => panic!("expected to be primary"),
        }
    }

    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn real_names_match_v01_and_the_plan() {
        let names = Names::real();
        assert_eq!(names.mutex, r"Local\WinPods-single-instance");
        assert_eq!(names.activate, r"Local\WinPods-activate");
        assert_eq!(names.ack, r"Local\WinPods-activated");
    }

    #[test]
    fn second_acquire_is_secondary_until_the_primary_drops() {
        let names = test_names();
        let guard = primary(&names);
        assert!(matches!(
            acquire_with(&names, Duration::ZERO).unwrap(),
            Instance::Secondary
        ));
        drop(guard);
        let _again = primary(&names);
    }

    #[test]
    fn activation_runs_the_handler_and_is_acknowledged() {
        let names = test_names();
        let guard = primary(&names);
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        guard.on_activate(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(signal_with(&names, Duration::from_secs(5)));
        // the ack is sent after the handler returned
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(signal_with(&names, Duration::from_secs(5)));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn early_activation_is_delivered_once_a_handler_exists() {
        let names = test_names();
        let guard = primary(&names);
        assert!(signal_with(&names, Duration::from_secs(5)));
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        guard.on_activate(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(wait_until(|| count.load(Ordering::SeqCst) == 1));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_panicking_handler_does_not_stop_the_listener() {
        let names = test_names();
        let guard = primary(&names);
        guard.on_activate(Box::new(|| {
            panic!("handler failure (expected in this test)")
        }));
        assert!(signal_with(&names, Duration::from_secs(5)));
        assert!(signal_with(&names, Duration::from_secs(5)));
    }

    #[test]
    fn signal_without_a_v02_primary_returns_false_quickly() {
        let names = test_names();
        let start = Instant::now();
        assert!(!signal_with(&names, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_mutex_without_events_looks_like_an_older_version() {
        // v0.1 only creates the mutex.
        let names = test_names();
        let mutex_w = win::wide(names.mutex.as_ref());
        // SAFETY: NUL-terminated name.
        let _v01 =
            OwnedHandle(unsafe { CreateMutexW(None, false, PCWSTR(mutex_w.as_ptr())) }.unwrap());
        assert!(matches!(
            acquire_with(&names, Duration::ZERO).unwrap(),
            Instance::Secondary
        ));
        assert!(!signal_with(&names, Duration::from_millis(200)));
    }

    #[test]
    fn retry_for_waits_for_the_old_instance_to_exit() {
        let names = test_names();
        let old = primary(&names);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(old);
        });
        let start = Instant::now();
        let result = acquire_with(&names, Duration::from_secs(5)).unwrap();
        assert!(matches!(result, Instance::Primary(_)));
        assert!(start.elapsed() >= Duration::from_millis(250));
        releaser.join().unwrap();

        // without retry a held mutex gives up at once
        let start = Instant::now();
        assert!(matches!(
            acquire_with(&names, Duration::ZERO).unwrap(),
            Instance::Secondary
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
