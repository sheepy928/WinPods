//! Device enumeration and default-device notifications. CONTRACT.
//!
//! A dedicated `device-watch` thread initialises COM as MTA, owns the wasapi
//! `DeviceEnumerator` and the notification registration for the whole process, and
//! serves list/default requests over a channel. The notification callback stays
//! trivial: ignore non-Render directions and the Communications role, forward the
//! new default id for Console/Multimedia with a non-blocking send. A 5 s poll of
//! `get_default_device_for_role(Render, Multimedia)` backs it up. Events with the
//! same id as the last one sent are suppressed.
//!
//! Windows reports one default change once per role, so a switch produces a short
//! burst of callbacks. The first relevant callback schedules a check
//! [`MERGE_WINDOW`] later; callbacks arriving meanwhile merge into that one check.
//! The check asks for the Multimedia default itself, so the event always carries the
//! endpoint capture would open for [`crate::CaptureSource::DefaultRender`].
//!
//! Endpoints being added, removed, enabled or disabled (a headset plugged in) are
//! merged the same way into one [`DeviceEvent::DevicesChanged`], so the source list
//! can be refreshed.

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use wasapi::{DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration, Direction, Role};

use crate::devices::{self, ComGuard, DeviceInfo};

/// Interval of the backup poll of the default render endpoint.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Notifications closer together than this are merged into one check.
pub const MERGE_WINDOW: Duration = Duration::from_millis(250);
/// How long [`DeviceWatch::list`] and [`DeviceWatch::default_render`] wait for the
/// watch thread before giving up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// The default render device (Multimedia role) changed to the endpoint `id`.
    DefaultRenderChanged { id: String },
    /// An endpoint was added, removed, enabled or disabled: list the devices again.
    DevicesChanged,
}

/// Messages to the watch thread.
enum Request {
    List(SyncSender<io::Result<Vec<DeviceInfo>>>),
    Default(SyncSender<io::Result<Option<DeviceInfo>>>),
    /// A relevant default-device callback fired.
    Notify,
    /// An endpoint added/removed/state-changed callback fired.
    DevicesNotify,
    Stop,
}

/// Handle to the device-watch thread. Dropping it stops the thread.
pub struct DeviceWatch {
    requests: Sender<Request>,
    thread: Option<JoinHandle<()>>,
}

impl DeviceWatch {
    /// Start the watch thread. Default render changes and endpoint changes are sent
    /// to `events`; the current default at start-up is not reported (ask
    /// [`Self::default_render`]).
    /// Fails if COM or the device enumerator cannot be set up. A failed notification
    /// registration is logged and leaves the 5 s poll in charge.
    pub fn start(events: UnboundedSender<DeviceEvent>) -> std::io::Result<DeviceWatch> {
        let (requests, inbox) = mpsc::channel();
        let notify = requests.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("device-watch".into())
            .spawn(move || watch_thread(inbox, notify, events, ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(DeviceWatch {
                requests,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(io::Error::other(
                    "device-watch thread ended during start-up",
                ))
            }
        }
    }

    /// All loopback-capable render endpoints (as `Loopback`) followed by recording
    /// endpoints (as `Input`), active devices only, sorted by display name.
    pub fn list(&self) -> std::io::Result<Vec<DeviceInfo>> {
        self.request(Request::List)
    }

    /// The current default render endpoint (Multimedia role).
    pub fn default_render(&self) -> std::io::Result<Option<DeviceInfo>> {
        self.request(Request::Default)
    }

    /// Send a request and wait for its reply.
    fn request<T>(&self, make: impl FnOnce(SyncSender<io::Result<T>>) -> Request) -> io::Result<T> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.requests
            .send(make(reply_tx))
            .map_err(|_| not_running())?;
        match reply_rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "device-watch thread did not answer",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(not_running()),
        }
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn not_running() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "device-watch thread is not running",
    )
}

/// Only default changes of the render direction for the roles WinPods follows.
fn is_relevant(direction: Direction, role: Role) -> bool {
    direction == Direction::Render && matches!(role, Role::Console | Role::Multimedia)
}

/// Tracks the last default id reported and turns observations into events.
#[derive(Debug, Default)]
struct DefaultFollower {
    last: Option<String>,
}

impl DefaultFollower {
    fn new(current: Option<String>) -> Self {
        Self { last: current }
    }

    /// Report `current` if it differs from the last reported id. Losing the default
    /// (no playback device) is remembered silently, so the same endpoint coming back
    /// is reported again.
    fn observe(&mut self, current: Option<String>) -> Option<DeviceEvent> {
        match current {
            None => {
                self.last = None;
                None
            }
            Some(id) if self.last.as_deref() == Some(id.as_str()) => None,
            Some(id) => {
                self.last = Some(id.clone());
                Some(DeviceEvent::DefaultRenderChanged { id })
            }
        }
    }
}

/// When the watch thread should next check the default endpoint.
#[derive(Debug)]
struct Schedule {
    next_poll: Instant,
    /// Deadline of a merged notification check, if one is pending.
    pending: Option<Instant>,
    /// Deadline of a merged "devices changed" report, if one is pending.
    devices: Option<Instant>,
}

impl Schedule {
    fn new(now: Instant) -> Self {
        Self {
            next_poll: now + POLL_INTERVAL,
            pending: None,
            devices: None,
        }
    }

    /// A notification arrived: check after the merge window (unless one is pending).
    fn notify(&mut self, now: Instant) {
        if self.pending.is_none() {
            self.pending = Some(now + MERGE_WINDOW);
        }
    }

    /// An endpoint changed: report after the merge window (unless one is pending).
    fn notify_devices(&mut self, now: Instant) {
        if self.devices.is_none() {
            self.devices = Some(now + MERGE_WINDOW);
        }
    }

    /// True when a merged "devices changed" report is due; consumes it.
    fn take_devices_due(&mut self, now: Instant) -> bool {
        let due = self.devices.is_some_and(|at| now >= at);
        if due {
            self.devices = None;
        }
        due
    }

    /// True when a check is due; consumes the pending check and restarts the poll.
    fn take_due(&mut self, now: Instant) -> bool {
        let due = now >= self.next_poll || self.pending.is_some_and(|at| now >= at);
        if due {
            self.pending = None;
            self.next_poll = now + POLL_INTERVAL;
        }
        due
    }

    /// How long to sleep until the next check.
    fn wait_time(&self, now: Instant) -> Duration {
        let next = [self.pending, self.devices]
            .into_iter()
            .flatten()
            .fold(self.next_poll, Instant::min);
        next.saturating_duration_since(now)
    }
}

/// Body of the `device-watch` thread.
fn watch_thread(
    inbox: Receiver<Request>,
    notify: Sender<Request>,
    events: UnboundedSender<DeviceEvent>,
    ready: SyncSender<io::Result<()>>,
) {
    // Declared first so COM is uninitialised after every COM object below.
    let _com = match ComGuard::mta() {
        Ok(guard) => guard,
        Err(e) => {
            let _ = ready.send(Err(io::Error::other(format!(
                "COM initialisation failed: {e}"
            ))));
            return;
        }
    };
    let enumerator = match DeviceEnumerator::new() {
        Ok(enumerator) => enumerator,
        Err(e) => {
            let _ = ready.send(Err(io::Error::other(format!(
                "cannot create the device enumerator: {e}"
            ))));
            return;
        }
    };
    let registration = register(&enumerator, notify);
    let initial = devices::default_render_id(&enumerator).unwrap_or_else(|e| {
        tracing::debug!("default render endpoint unavailable at start-up: {e}");
        None
    });
    let mut follower = DefaultFollower::new(initial);
    let _ = ready.send(Ok(()));
    drop(ready);

    let mut schedule = Schedule::new(Instant::now());
    loop {
        match inbox.recv_timeout(schedule.wait_time(Instant::now())) {
            Ok(Request::List(reply)) => {
                let _ = reply.send(devices::enumerate(&enumerator).map_err(io::Error::other));
            }
            Ok(Request::Default(reply)) => {
                let result = devices::default_render(&enumerator)
                    .map(|found| found.map(|(_, info)| info))
                    .map_err(io::Error::other);
                let _ = reply.send(result);
            }
            Ok(Request::Notify) => schedule.notify(Instant::now()),
            Ok(Request::DevicesNotify) => schedule.notify_devices(Instant::now()),
            Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if schedule.take_devices_due(Instant::now()) {
            tracing::debug!("audio endpoints changed");
            let _ = events.send(DeviceEvent::DevicesChanged);
        }
        if schedule.take_due(Instant::now()) {
            match devices::default_render_id(&enumerator) {
                Ok(current) => {
                    if let Some(event) = follower.observe(current) {
                        tracing::info!("default render endpoint changed: {event:?}");
                        // A closed receiver only means nobody listens any more.
                        let _ = events.send(event);
                    }
                }
                Err(e) => tracing::debug!("default render endpoint query failed: {e}"),
            }
        }
    }
    drop(registration);
    drop(enumerator);
}

/// Register the default-device and endpoint callbacks; `None` (logged) when Windows
/// refuses.
fn register(
    enumerator: &DeviceEnumerator,
    notify: Sender<Request>,
) -> Option<DeviceEventRegistration> {
    let mut callbacks = DeviceEventCallbacks::new();
    let added = notify.clone();
    let removed = notify.clone();
    let state = notify.clone();
    callbacks.set_default_device_callback(move |direction, role, _id| {
        if is_relevant(direction, role) {
            // Unbounded channel: never blocks the Windows notification thread.
            let _ = notify.send(Request::Notify);
        }
    });
    callbacks.set_device_added_callback(move |_id| {
        let _ = added.send(Request::DevicesNotify);
    });
    callbacks.set_device_removed_callback(move |_id| {
        let _ = removed.send(Request::DevicesNotify);
    });
    callbacks.set_device_state_callback(move |_id, _state| {
        let _ = state.send(Request::DevicesNotify);
    });
    match enumerator.register_notification_callback(callbacks) {
        Ok(registration) => Some(registration),
        Err(e) => {
            tracing::warn!("default-device notifications unavailable, polling only: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_filter() {
        assert!(is_relevant(Direction::Render, Role::Console));
        assert!(is_relevant(Direction::Render, Role::Multimedia));
        assert!(!is_relevant(Direction::Render, Role::Communications));
        assert!(!is_relevant(Direction::Capture, Role::Console));
        assert!(!is_relevant(Direction::Capture, Role::Multimedia));
    }

    #[test]
    fn follower_suppresses_duplicates() {
        let mut f = DefaultFollower::new(Some("a".into()));
        assert_eq!(
            f.observe(Some("a".into())),
            None,
            "initial default not reported"
        );
        assert_eq!(
            f.observe(Some("b".into())),
            Some(DeviceEvent::DefaultRenderChanged { id: "b".into() })
        );
        assert_eq!(f.observe(Some("b".into())), None);
        // Device gone, then back: reported again.
        assert_eq!(f.observe(None), None);
        assert_eq!(
            f.observe(Some("b".into())),
            Some(DeviceEvent::DefaultRenderChanged { id: "b".into() })
        );
        let mut empty = DefaultFollower::default();
        assert!(empty.observe(Some("x".into())).is_some());
    }

    #[test]
    fn notifications_merge_within_the_window() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        assert_eq!(s.wait_time(t0), POLL_INTERVAL);
        assert!(!s.take_due(t0));

        s.notify(t0);
        s.notify(t0 + Duration::from_millis(100)); // merged
        assert_eq!(
            s.wait_time(t0 + Duration::from_millis(100)),
            Duration::from_millis(150)
        );
        assert!(!s.take_due(t0 + Duration::from_millis(249)));
        assert!(s.take_due(t0 + MERGE_WINDOW));
        // One check for the whole burst; the poll restarts from the check.
        assert!(!s.take_due(t0 + Duration::from_millis(300)));
        assert_eq!(s.wait_time(t0 + MERGE_WINDOW), POLL_INTERVAL);

        // A later notification opens a new window.
        let t1 = t0 + Duration::from_secs(1);
        s.notify(t1);
        assert!(s.take_due(t1 + MERGE_WINDOW));
    }

    #[test]
    fn device_changes_merge_into_one_report() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        assert!(!s.take_devices_due(t0));

        // A headset arriving fires added + state changed for each endpoint.
        s.notify_devices(t0);
        s.notify_devices(t0 + Duration::from_millis(50));
        s.notify_devices(t0 + Duration::from_millis(120));
        assert_eq!(
            s.wait_time(t0 + Duration::from_millis(120)),
            Duration::from_millis(130),
            "the report is due one window after the first callback"
        );
        assert!(!s.take_devices_due(t0 + Duration::from_millis(249)));
        assert!(s.take_devices_due(t0 + MERGE_WINDOW));
        assert!(!s.take_devices_due(t0 + Duration::from_millis(300)), "once");
        assert_eq!(s.wait_time(t0 + MERGE_WINDOW), POLL_INTERVAL - MERGE_WINDOW);

        // Independent of the default-device check.
        assert!(!s.take_due(t0 + MERGE_WINDOW));
        let t1 = t0 + Duration::from_secs(1);
        s.notify_devices(t1);
        assert!(!s.take_due(t1 + MERGE_WINDOW));
        assert!(s.take_devices_due(t1 + MERGE_WINDOW));
    }

    #[test]
    fn poll_fires_every_interval() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        assert!(!s.take_due(t0 + POLL_INTERVAL - Duration::from_millis(1)));
        assert!(s.take_due(t0 + POLL_INTERVAL));
        assert!(!s.take_due(t0 + POLL_INTERVAL + Duration::from_millis(1)));
        assert!(s.take_due(t0 + 2 * POLL_INTERVAL));
        // Overdue checks never ask for a negative sleep.
        assert_eq!(s.wait_time(t0 + 10 * POLL_INTERVAL), Duration::ZERO);
    }

    #[test]
    fn handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DeviceWatch>();
    }

    /// Lists the real endpoints (read-only; opens no stream). Tolerates machines
    /// without audio devices, such as CI runners.
    #[test]
    fn lists_devices() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = DeviceWatch::start(tx).expect("device watch starts");
        let devices = watch.list().expect("list devices");
        let loopbacks = devices
            .iter()
            .take_while(|d| d.kind == crate::DeviceKind::Loopback)
            .count();
        assert!(
            devices[loopbacks..]
                .iter()
                .all(|d| d.kind == crate::DeviceKind::Input),
            "loopbacks first"
        );
        for d in &devices {
            assert!(!d.id.is_empty());
            assert!(d.sample_rate > 0 && d.channels > 0, "{d:?}");
            match d.kind {
                crate::DeviceKind::Loopback => {
                    assert_eq!(
                        d.display_name,
                        format!("{}{}", d.name, devices::LOOPBACK_SUFFIX)
                    );
                }
                crate::DeviceKind::Input => {
                    assert_eq!(d.display_name, d.name);
                    assert!(!d.is_default);
                }
            }
        }
        let default = watch.default_render().expect("default render");
        if let Some(default) = &default {
            assert!(default.is_default);
            assert_eq!(default.kind, crate::DeviceKind::Loopback);
            let listed = devices.iter().find(|d| d.id == default.id);
            assert!(listed.is_some_and(|d| d.is_default));
        }
        assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
        eprintln!(
            "{} endpoints, default {:?}",
            devices.len(),
            default.map(|d| d.display_name)
        );
        for d in &devices {
            eprintln!(
                "  {:?} {} ({} Hz, {} ch)",
                d.kind, d.display_name, d.sample_rate, d.channels
            );
        }
        drop(watch); // stops and joins the thread
    }
}
