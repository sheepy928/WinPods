//! Supervisor: port of `legacy/python/winpods/engine.py` on the new architecture.
//!
//! One engine thread runs a tokio current-thread runtime. A loop reconciles the
//! desired state (connected or not, settings) with reality every 250 ms or when a
//! command, device event or background job result arrives:
//!
//! * capture runs while a connection is wanted; it is restarted when the selection
//!   changes, when the device is lost, and (for the default output) when Windows'
//!   default render device changes. Starting and stopping capture and listing
//!   devices touch WASAPI/COM and can stall, so they run on tokio's blocking pool
//!   and report back to the loop;
//! * the stream is (re)built by a connect task ([`crate::connect`]: route → PTP →
//!   discovery → `Session::connect` → sockets → `Session::setup` →
//!   `Session::start_playback`), then `runtime::start` here, with reconnect backoff
//!   2/3/5/10/20/30 s on failure. The loop keeps handling commands while the task
//!   runs: Disconnect, Quit and setting changes cancel it (a session that already
//!   exists still gets its TEARDOWN), Find starts at once;
//! * volume changes go to a per-stream worker that sends only the newest value, so
//!   a receiver that stops answering never holds up the loop;
//! * a [`Snapshot`] is published through an `ArcSwap` for the UI/CLI.
//!
//! The engine never writes config; it reports what it learns through the snapshot.

use std::future::Future;
use std::net::Ipv4Addr;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinError;
use tracing::{debug, error, info, warn};

use winpods_airplay::ntp::TimingResponder;
use winpods_airplay::ptp::PtpService;
use winpods_airplay::runtime::{self, StreamHandle};
use winpods_airplay::session::{Session, SessionConfig, TEARDOWN_TIMEOUT};
use winpods_airplay::{ReceiverInfo, StreamParams};
use winpods_audio::{
    CaptureError, CaptureExit, CaptureHandle, CaptureSource, DeviceEvent, DeviceInfo, DeviceWatch,
};
use winpods_rt::{RingConsumer, RingProducer, RingShared};

use crate::connect::{
    self, Canceller, ConnectError, ConnectOutcome, ConnectRequest, Connected, PtpSlot,
};
use crate::scan;
use crate::types::{
    CaptureSelection, Command, EngineApi, EngineSettings, ScanState, Snapshot, State,
};

const BACKOFF_SECS: [u64; 6] = [2, 3, 5, 10, 20, 30];
const CAPTURE_RETRY: Duration = Duration::from_secs(2);
/// A capture start still running after this is reported as a stuck device.
const CAPTURE_START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits for a capture start/stop to finish.
const CAPTURE_STOP_WAIT: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(250);
const DEVICE_EVENT_DEBOUNCE: Duration = Duration::from_millis(250);
const SCAN_DURATION: Duration = Duration::from_secs(5);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);
/// How long shutdown waits for a cancelled connect attempt (its TEARDOWN included).
const CONNECT_CANCEL_WAIT: Duration = TEARDOWN_TIMEOUT.saturating_add(Duration::from_secs(1));
/// How long dropping the runtime waits for blocking-pool jobs (a stuck WASAPI call
/// must not keep the process alive).
const RUNTIME_SHUTDOWN_WAIT: Duration = Duration::from_millis(500);
/// Level meter: without a capture chunk for this long the level reads 0.
pub const PEAK_STALE: Duration = Duration::from_millis(250);
const DMAP_TITLE: &str = "Windows audio";

/// Handle to the engine thread.
pub struct EngineHandle {
    commands: UnboundedSender<Command>,
    snapshot: Arc<ArcSwap<Snapshot>>,
    ring: Arc<RingShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    finished: Mutex<Option<std_mpsc::Receiver<()>>>,
}

type Prepare = Box<dyn FnOnce(&mut Engine) + Send>;

impl EngineHandle {
    pub fn start(settings: EngineSettings) -> std::io::Result<EngineHandle> {
        Self::spawn(settings, Box::new(|_| {}))
    }

    /// Start the engine thread; `prepare` runs on the new engine before its loop.
    fn spawn(settings: EngineSettings, prepare: Prepare) -> std::io::Result<EngineHandle> {
        winpods_rt::clock::init();
        let (producer, consumer) = winpods_rt::ring(winpods_rt::ring::DEFAULT_CAPACITY)?;
        let ring = producer.shared().clone();
        let (commands, command_rx) = unbounded_channel();
        let snapshot = Arc::new(ArcSwap::from_pointee(initial_snapshot(&settings)));
        let (done_tx, done_rx) = std_mpsc::channel();

        let shared_snapshot = snapshot.clone();
        let thread = std::thread::Builder::new()
            .name("winpods-engine".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        error!("engine runtime failed to start: {err}");
                        let _ = done_tx.send(());
                        return;
                    }
                };
                runtime.block_on(async move {
                    let mut engine =
                        Engine::new(settings, producer, consumer, shared_snapshot, command_rx);
                    prepare(&mut engine);
                    engine.run().await;
                });
                // Blocking-pool jobs stuck in WASAPI (or a cancelled PTP lock wait)
                // are left behind instead of holding up the exit.
                runtime.shutdown_timeout(RUNTIME_SHUTDOWN_WAIT);
                let _ = done_tx.send(());
            })?;

        Ok(EngineHandle {
            commands,
            snapshot,
            ring,
            thread: Mutex::new(Some(thread)),
            finished: Mutex::new(Some(done_rx)),
        })
    }

    /// Stop streaming (TEARDOWN) and end the engine thread; blocks up to ~8 s.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
        let done = self.finished.lock().take();
        let thread = self.thread.lock().take();
        if let Some(done) = done
            && done.recv_timeout(SHUTDOWN_WAIT).is_err()
        {
            warn!("engine did not stop within {SHUTDOWN_WAIT:?}");
            return; // leave the thread detached rather than hang the UI
        }
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        if self.thread.lock().is_some() {
            self.shutdown();
        }
    }
}

impl EngineApi for EngineHandle {
    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
    fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }
    fn peak(&self) -> f32 {
        meter_peak(
            self.ring.peak(),
            self.ring.last_write_ns(),
            winpods_rt::mono_ns(),
        )
    }
}

/// The level-meter value: the last chunk's peak while capture keeps writing, 0 once
/// nothing was written for [`PEAK_STALE`] (capture stopped, or an idle loopback).
fn meter_peak(peak: f32, last_write_ns: u64, now_ns: u64) -> f32 {
    let stale_ns = PEAK_STALE.as_nanos() as u64;
    if last_write_ns == 0 || now_ns.saturating_sub(last_write_ns) > stale_ns {
        0.0
    } else {
        peak
    }
}

fn initial_snapshot(settings: &EngineSettings) -> Snapshot {
    Snapshot {
        device_ip: settings.device_ip.clone(),
        latency_ms: settings.latency_ms,
        codec: settings.codec,
        timing: settings.timing,
        volume: settings.volume,
        capture: settings.capture.clone(),
        ..Snapshot::default()
    }
}

/// Sends receiver volume changes in the background. Only the newest value is kept,
/// so at most one SET_PARAMETER is outstanding and a slider drag never queues up
/// requests behind a receiver that stopped answering.
struct VolumeWorker {
    latest: watch::Sender<Option<f32>>,
    task: tokio::task::JoinHandle<()>,
}

impl VolumeWorker {
    fn spawn<F, Fut>(apply: F) -> VolumeWorker
    where
        F: Fn(f32) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (latest, mut changes) = watch::channel(None::<f32>);
        let task = tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let value = *changes.borrow_and_update();
                if let Some(pct) = value {
                    apply(pct).await;
                }
            }
        });
        VolumeWorker { latest, task }
    }

    /// Queue `pct`, replacing any value not sent yet. Never blocks.
    fn set(&self, pct: f32) {
        self.latest.send_replace(Some(pct));
    }

    /// Stop the worker; any request it had in flight is abandoned.
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// The live parts of one stream.
struct Stream {
    /// Shared with the volume worker; unwrapped again for TEARDOWN.
    session: Arc<Session>,
    handle: StreamHandle,
    params: StreamParams,
    ptp: Option<Arc<PtpService>>,
    ntp: Option<TimingResponder>,
    volume: VolumeWorker,
    started: Instant,
}

/// A connection attempt running as its own task.
struct ConnectJob {
    canceller: Canceller,
    task: tokio::task::JoinHandle<ConnectOutcome>,
    /// The receiver address this attempt is for.
    address: Ipv4Addr,
    /// The volume the session was configured with.
    volume: Option<f32>,
    cancelled: bool,
}

impl ConnectJob {
    fn cancel(&mut self) {
        if !self.cancelled {
            self.cancelled = true;
            self.canceller.cancel();
        }
    }
}

/// Result of a capture start or stop on the blocking pool.
enum CaptureDone {
    Started(Result<CaptureHandle, (CaptureError, RingProducer)>),
    Stopped(RingProducer, CaptureExit),
}

struct CaptureTask {
    handle: tokio::task::JoinHandle<CaptureDone>,
    since: Instant,
    starting: bool,
}

type DeviceList = std::io::Result<Vec<DeviceInfo>>;

struct Engine {
    settings: EngineSettings,
    commands: UnboundedReceiver<Command>,
    snapshot: Arc<ArcSwap<Snapshot>>,

    // ring ends while not owned by a capture / stream
    producer: Option<RingProducer>,
    consumer: Option<RingConsumer>,
    ring: Arc<RingShared>,

    // devices
    watch: Option<Arc<DeviceWatch>>,
    device_events: UnboundedReceiver<DeviceEvent>,
    pending_default: Option<(String, Instant)>,
    devices: Arc<Vec<DeviceInfo>>,
    device_list: Option<tokio::task::JoinHandle<DeviceList>>,
    /// Another refresh was asked for while one was running.
    devices_stale: bool,

    // capture
    capture: Option<CaptureHandle>,
    capture_task: Option<CaptureTask>,
    capture_retry_at: Option<Instant>,
    restart_capture: bool,
    /// Why capture is not running while it should; empty while capturing.
    capture_error: String,
    last_capture_name: String,
    last_capture_rate: u32,
    last_capture_format: String,

    // stream
    want_connected: bool,
    stream: Option<Stream>,
    connecting: Option<ConnectJob>,
    ptp_slot: PtpSlot,
    receiver: Option<ReceiverInfo>,
    restart_stream: bool,
    failures: usize,
    retry_at: Option<Instant>,
    state: State,
    last_error: String,
    discovered_name: Option<String>,
    last_stats: runtime::StreamStats,

    // scan
    scan: ScanState,
    scan_result: Option<oneshot::Receiver<std::io::Result<Vec<crate::types::ScanResult>>>>,

    shutdown: bool,
}

/// Wait for the job in `slot`, or forever when there is none (for `select!`).
async fn join_connect(slot: &mut Option<ConnectJob>) -> Result<ConnectOutcome, JoinError> {
    match slot {
        Some(job) => (&mut job.task).await,
        None => std::future::pending().await,
    }
}

async fn join_capture(slot: &mut Option<CaptureTask>) -> Result<CaptureDone, JoinError> {
    match slot {
        Some(task) => (&mut task.handle).await,
        None => std::future::pending().await,
    }
}

async fn join_list(
    slot: &mut Option<tokio::task::JoinHandle<DeviceList>>,
) -> Result<DeviceList, JoinError> {
    match slot {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

impl Engine {
    fn new(
        settings: EngineSettings,
        producer: RingProducer,
        consumer: RingConsumer,
        snapshot: Arc<ArcSwap<Snapshot>>,
        commands: UnboundedReceiver<Command>,
    ) -> Engine {
        let ring = producer.shared().clone();
        let (event_tx, device_events) = unbounded_channel();
        let watch = match DeviceWatch::start(event_tx) {
            Ok(watch) => Some(Arc::new(watch)),
            Err(err) => {
                warn!("device watch unavailable: {err}");
                None
            }
        };
        Engine {
            settings,
            commands,
            snapshot,
            producer: Some(producer),
            consumer: Some(consumer),
            ring,
            watch,
            device_events,
            pending_default: None,
            devices: Arc::new(Vec::new()),
            device_list: None,
            devices_stale: false,
            capture: None,
            capture_task: None,
            capture_retry_at: None,
            restart_capture: false,
            capture_error: String::new(),
            last_capture_name: String::new(),
            last_capture_rate: 0,
            last_capture_format: String::new(),
            want_connected: false,
            stream: None,
            connecting: None,
            ptp_slot: Arc::new(Mutex::new(Weak::new())),
            receiver: None,
            restart_stream: false,
            failures: 0,
            retry_at: None,
            state: State::Disconnected,
            last_error: String::new(),
            discovered_name: None,
            last_stats: runtime::StreamStats::default(),
            scan: ScanState::Idle,
            scan_result: None,
            shutdown: false,
        }
    }

    async fn run(&mut self) {
        self.refresh_devices();
        self.publish();
        while !self.shutdown {
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => self.handle_command(command),
                    None => self.shutdown = true,
                },
                Some(event) = self.device_events.recv() => self.handle_device_event(event),
                joined = join_connect(&mut self.connecting) => {
                    if let Some(job) = self.connecting.take() {
                        self.finish_connect(job, joined).await;
                    }
                }
                joined = join_capture(&mut self.capture_task) => {
                    self.capture_task = None;
                    self.finish_capture(joined);
                }
                joined = join_list(&mut self.device_list) => {
                    self.device_list = None;
                    self.finish_device_list(joined);
                }
                _ = tokio::time::sleep(TICK) => {}
            }
            if self.shutdown {
                break;
            }
            self.poll_scan();
            self.step().await;
            self.publish();
        }
        self.shut_down().await;
    }

    /// Cancel a connect attempt (TEARDOWN if it got that far), stop the stream and
    /// capture, all within [`SHUTDOWN_WAIT`].
    async fn shut_down(&mut self) {
        info!("engine shutting down");
        if let Some(mut job) = self.connecting.take() {
            job.cancel();
            match tokio::time::timeout(CONNECT_CANCEL_WAIT, &mut job.task).await {
                Ok(Ok(outcome)) => {
                    // Finished just before the cancel arrived: still say goodbye.
                    if let Ok(connected) = outcome.result {
                        connected.session.teardown().await;
                    }
                }
                Ok(Err(err)) => warn!("connect task failed: {err}"),
                Err(_) => {
                    warn!("connect attempt did not stop within {CONNECT_CANCEL_WAIT:?}");
                    job.task.abort();
                }
            }
        }
        self.stop_stream().await;
        self.settle_capture().await;
        if self.capture.is_some() {
            self.stop_capture();
            self.settle_capture().await;
        }
        self.state = State::Disconnected;
        self.publish();
    }

    /// Wait (bounded) for a running capture start/stop and take its result.
    async fn settle_capture(&mut self) {
        if let Some(mut task) = self.capture_task.take() {
            match tokio::time::timeout(CAPTURE_STOP_WAIT, &mut task.handle).await {
                Ok(joined) => self.finish_capture(joined),
                Err(_) => warn!("audio capture did not stop within {CAPTURE_STOP_WAIT:?}"),
            }
        }
    }

    // ------------------------------------------------------------------ commands
    fn handle_command(&mut self, command: Command) {
        debug!(?command, "command");
        match command {
            Command::Connect => {
                self.want_connected = true;
                self.failures = 0;
                self.retry_at = None;
                if self.state == State::Error {
                    self.state = State::Disconnected;
                }
            }
            Command::Disconnect => {
                self.want_connected = false;
                self.last_error.clear();
                self.capture_error.clear();
                self.cancel_connect();
            }
            Command::SetLatency(ms) => {
                let ms = ms.clamp(
                    winpods_airplay::MIN_LATENCY_MS,
                    winpods_airplay::MAX_LATENCY_MS,
                );
                if ms != self.settings.latency_ms {
                    self.settings.latency_ms = ms;
                    self.stream_settings_changed();
                }
            }
            Command::SetCapture(selection) => {
                if selection != self.settings.capture {
                    self.settings.capture = selection;
                    self.restart_capture = true;
                    self.capture_error.clear();
                }
            }
            Command::SetDeviceIp(ip) => {
                let ip = ip.trim().to_string();
                if ip != self.settings.device_ip {
                    self.settings.device_ip = ip;
                    self.receiver = None;
                    self.discovered_name = None;
                    self.stream_settings_changed();
                }
                // The only permanent error is "no speaker address"; an address
                // answers it.
                if self.state == State::Error && !self.settings.device_ip.is_empty() {
                    self.state = State::Disconnected;
                    self.last_error.clear();
                }
            }
            Command::SetVolume(volume) => {
                self.settings.volume = volume;
                if let (Some(pct), Some(stream)) = (volume, self.stream.as_ref()) {
                    stream.volume.set(pct);
                }
            }
            Command::SetTiming(timing) => {
                if timing != self.settings.timing {
                    self.settings.timing = timing;
                    self.stream_settings_changed();
                }
            }
            Command::SetCodec(codec) => {
                if codec != self.settings.codec {
                    self.settings.codec = codec;
                    self.stream_settings_changed();
                }
            }
            Command::RefreshDevices => self.refresh_devices(),
            Command::Scan => self.start_scan(),
            Command::Shutdown => self.shutdown = true,
        }
    }

    /// A setting the stream was built with changed: rebuild it (or restart the
    /// attempt under way), and try at once instead of waiting out a backoff.
    fn stream_settings_changed(&mut self) {
        self.restart_stream = true;
        self.failures = 0;
        self.retry_at = None;
    }

    fn cancel_connect(&mut self) {
        if let Some(job) = self.connecting.as_mut() {
            job.cancel();
        }
    }

    fn handle_device_event(&mut self, event: DeviceEvent) {
        match event {
            DeviceEvent::DefaultRenderChanged { id } => {
                debug!(%id, "default render device changed");
                self.pending_default = Some((id, Instant::now() + DEVICE_EVENT_DEBOUNCE));
                self.refresh_devices();
            }
            DeviceEvent::DevicesChanged => {
                debug!("audio endpoints changed");
                self.refresh_devices();
            }
        }
    }

    /// List devices on the blocking pool (the watch thread may be slow to answer).
    fn refresh_devices(&mut self) {
        let Some(watch) = &self.watch else {
            return;
        };
        if self.device_list.is_some() {
            self.devices_stale = true;
            return;
        }
        let watch = watch.clone();
        self.device_list = Some(tokio::task::spawn_blocking(move || watch.list()));
    }

    fn finish_device_list(&mut self, joined: Result<DeviceList, JoinError>) {
        match joined {
            Ok(Ok(list)) => self.devices = Arc::new(list),
            Ok(Err(err)) => warn!("listing capture devices failed: {err}"),
            Err(err) => warn!("device listing task failed: {err}"),
        }
        if std::mem::take(&mut self.devices_stale) {
            self.refresh_devices();
        }
    }

    fn start_scan(&mut self) {
        if matches!(self.scan, ScanState::Running) {
            return;
        }
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(scan::scan(SCAN_DURATION).await);
        });
        self.scan = ScanState::Running;
        self.scan_result = Some(rx);
    }

    fn poll_scan(&mut self) {
        let Some(rx) = self.scan_result.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(found)) => {
                self.scan = ScanState::Done(Arc::new(found));
                self.scan_result = None;
            }
            Ok(Err(err)) => {
                self.scan = ScanState::Failed(err.to_string());
                self.scan_result = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.scan = ScanState::Failed("scan task ended".into());
                self.scan_result = None;
            }
        }
    }

    // --------------------------------------------------------------- reconcile
    async fn step(&mut self) {
        if !self.want_connected {
            self.cancel_connect();
            if self.stream.is_some() {
                self.stop_stream().await;
            }
            if self.capture.is_some() {
                self.stop_capture();
            }
            self.capture_error.clear();
            self.capture_retry_at = None;
            if self.state != State::Error {
                self.state = State::Disconnected;
            }
            return;
        }

        if self.settings.device_ip.trim().is_empty() {
            self.fail_permanently("No speaker address set: enter the HomePod's IP or use Find...");
            return;
        }

        self.reconcile_capture();
        self.reconcile_stream().await;
    }

    fn fail_permanently(&mut self, message: &str) {
        self.last_error = message.to_string();
        self.state = State::Error;
        self.want_connected = false;
    }

    fn capture_source(&self) -> CaptureSource {
        if self.settings.test_tone {
            CaptureSource::TestTone
        } else if self.settings.capture.is_default() {
            CaptureSource::DefaultRender
        } else {
            CaptureSource::Device {
                id: self.settings.capture.id.clone(),
                name: self.settings.capture.name.clone(),
            }
        }
    }

    fn reconcile_capture(&mut self) {
        // Default output changed (debounced) while following it?
        if let Some((id, due)) = self.pending_default.clone()
            && Instant::now() >= due
        {
            self.pending_default = None;
            let following_default = !self.settings.test_tone && self.settings.capture.is_default();
            let current = self.capture.as_ref().map(|c| c.info().device_id.clone());
            if following_default && current.as_deref() != Some(id.as_str()) {
                info!("default output changed; switching capture");
                self.restart_capture = true;
            }
        }

        // A start or stop is running on the blocking pool; decide when it is done.
        if let Some(task) = &self.capture_task {
            if task.starting
                && task.since.elapsed() >= CAPTURE_START_TIMEOUT
                && self.capture_error.is_empty()
            {
                warn!("audio capture has not started after {CAPTURE_START_TIMEOUT:?}");
                self.capture_error = format!(
                    "the audio device is not responding (capture did not start within {} s)",
                    CAPTURE_START_TIMEOUT.as_secs()
                );
                self.clear_capture_facts();
            }
            return;
        }

        if self.restart_capture {
            self.restart_capture = false;
            self.capture_retry_at = None;
            if self.capture.is_some() {
                self.stop_capture();
                return;
            }
        }

        // Device lost or capture failed on its own?
        if let Some(exit) = self.capture.as_ref().and_then(|c| c.exit()) {
            warn!(?exit, "capture ended");
            self.capture_error = describe_capture_exit(&exit);
            self.clear_capture_facts();
            self.stop_capture();
            self.capture_retry_at = Some(Instant::now() + CAPTURE_RETRY);
            return;
        }

        if self.capture.is_none() && self.capture_retry_at.is_none_or(|t| Instant::now() >= t) {
            let Some(producer) = self.producer.take() else {
                return;
            };
            let source = self.capture_source();
            let handle = tokio::task::spawn_blocking(move || {
                CaptureDone::Started(CaptureHandle::start(&source, producer))
            });
            self.capture_task = Some(CaptureTask {
                handle,
                since: Instant::now(),
                starting: true,
            });
        }
    }

    fn finish_capture(&mut self, joined: Result<CaptureDone, JoinError>) {
        match joined {
            Ok(CaptureDone::Started(Ok(capture))) => {
                let info = capture.info();
                info!(device = %info.device_name, rate = info.sample_rate, format = %info.format, "capture started");
                self.last_capture_name = info.device_name.clone();
                self.last_capture_rate = info.sample_rate;
                self.last_capture_format = info.format.clone();
                self.capture = Some(capture);
                self.capture_retry_at = None;
                self.capture_error.clear();
            }
            Ok(CaptureDone::Started(Err((err, producer)))) => {
                warn!("capture failed to start: {err}");
                producer.set_peak(0.0);
                self.producer = Some(producer);
                self.capture_error = format!("audio capture: {err}");
                self.clear_capture_facts();
                self.capture_retry_at = Some(Instant::now() + CAPTURE_RETRY);
            }
            Ok(CaptureDone::Stopped(producer, exit)) => {
                debug!(?exit, "capture stopped");
                producer.set_peak(0.0);
                self.producer = Some(producer);
            }
            Err(err) => {
                // The producer went down with the task; capture cannot restart.
                error!("capture task failed: {err}");
                self.capture_error =
                    "audio capture failed unexpectedly; restart WinPods".to_string();
                self.clear_capture_facts();
            }
        }
    }

    /// Forget the last capture's device/rate/format (shown during a quick restart).
    fn clear_capture_facts(&mut self) {
        self.last_capture_name.clear();
        self.last_capture_rate = 0;
        self.last_capture_format.clear();
    }

    async fn reconcile_stream(&mut self) {
        if let Some(job) = self.connecting.as_mut() {
            if self.restart_stream && !job.cancelled {
                info!("settings changed; restarting the connection attempt");
                job.cancel();
            }
            self.state = State::Connecting;
            return;
        }

        if let Some(stream) = &self.stream {
            let reason = stream
                .session
                .end_reason()
                .or_else(|| stream.handle.end_reason());
            if self.restart_stream {
                info!("restarting stream (settings changed)");
                self.restart_stream = false;
                self.stop_stream().await;
            } else if let Some(reason) = reason {
                warn!("stream ended: {reason}");
                self.last_error = reason;
                self.stop_stream().await;
                self.schedule_retry();
                return;
            } else {
                self.state = State::Streaming;
                return;
            }
        }

        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            self.state = State::Reconnecting;
            return;
        }
        self.restart_stream = false;
        self.start_connect();
    }

    /// Spawn a connect attempt with the current settings.
    fn start_connect(&mut self) {
        let address: Ipv4Addr = match self.settings.device_ip.trim().parse() {
            Ok(address) => address,
            Err(_) => {
                self.last_error = format!("'{}' is not an IPv4 address", self.settings.device_ip);
                error!("connect failed: {}", self.last_error);
                self.schedule_retry();
                return;
            }
        };
        let request = ConnectRequest {
            address,
            receiver: self.receiver.clone(),
            config: SessionConfig {
                sender_name: self.settings.sender_name.clone(),
                device_id: self.settings.device_id.clone(),
                latency_ms: self.settings.latency_ms,
                codec: self.settings.codec,
                volume_pct: self.settings.volume,
                title: DMAP_TITLE.to_string(),
            },
            timing: self.settings.timing,
            require_standard_ptp_ports: self.settings.require_standard_ptp_ports,
            drop_every: self.settings.drop_every,
            ptp_slot: self.ptp_slot.clone(),
        };
        let (canceller, token) = connect::cancel_pair();
        let task = tokio::spawn(connect::connect(request, token));
        self.connecting = Some(ConnectJob {
            canceller,
            task,
            address,
            volume: self.settings.volume,
            cancelled: false,
        });
        self.state = State::Connecting;
    }

    async fn finish_connect(&mut self, job: ConnectJob, joined: Result<ConnectOutcome, JoinError>) {
        let outcome = joined.unwrap_or_else(|err| ConnectOutcome {
            discovered: None,
            result: Err(ConnectError::Failed(format!(
                "internal: connect task failed: {err}"
            ))),
        });
        let current: Option<Ipv4Addr> = self.settings.device_ip.trim().parse().ok();
        if let Some(found) = outcome.discovered
            && current == Some(job.address)
        {
            self.discovered_name = Some(found.name.clone());
            self.receiver = Some(found);
        }
        match outcome.result {
            Ok(connected) if job.cancelled || self.shutdown || !self.want_connected => {
                debug!("connect finished after it was cancelled; closing the session");
                connected.session.teardown().await;
            }
            Ok(connected) => self.start_stream(connected, job.volume).await,
            Err(ConnectError::Cancelled) => debug!("connect attempt cancelled"),
            Err(ConnectError::Failed(message)) if job.cancelled => {
                debug!("cancelled connect attempt ended with: {message}");
            }
            Err(ConnectError::Failed(message)) => {
                error!("connect failed: {message}");
                self.last_error = message;
                self.receiver = None;
                self.schedule_retry();
            }
        }
    }

    /// Hand a negotiated session to the real-time runtime.
    async fn start_stream(&mut self, connected: Connected, configured_volume: Option<f32>) {
        let Connected {
            session,
            params,
            control,
            options,
            ptp,
            ntp,
        } = connected;
        let Some(consumer) = self.consumer.take() else {
            session.teardown().await;
            self.last_error = "internal: ring consumer missing".into();
            self.schedule_retry();
            return;
        };
        let handle = match runtime::start(&params, control, consumer, options) {
            Ok(handle) => handle,
            Err((err, consumer)) => {
                self.consumer = Some(consumer);
                session.teardown().await;
                self.last_error = format!("could not start the stream: {err}");
                error!("connect failed: {}", self.last_error);
                self.receiver = None;
                self.schedule_retry();
                return;
            }
        };
        let session = Arc::new(session);
        let for_volume = session.clone();
        let volume = VolumeWorker::spawn(move |pct| {
            let session = for_volume.clone();
            async move {
                if let Err(err) = session.set_volume(pct).await {
                    warn!("volume change failed: {err}");
                }
            }
        });
        // The volume moved while the attempt was under way.
        if self.settings.volume != configured_volume
            && let Some(pct) = self.settings.volume
        {
            volume.set(pct);
        }
        info!(
            receiver = %params.receiver,
            latency_ms = params.latency_ms,
            "streaming"
        );
        self.stream = Some(Stream {
            session,
            handle,
            params,
            ptp,
            ntp,
            volume,
            started: Instant::now(),
        });
        self.state = State::Streaming;
        self.last_error.clear();
        self.failures = 0;
        self.retry_at = None;
    }

    fn schedule_retry(&mut self) {
        let delay = BACKOFF_SECS[self.failures.min(BACKOFF_SECS.len() - 1)];
        self.failures += 1;
        self.retry_at = Some(Instant::now() + Duration::from_secs(delay));
        self.state = State::Reconnecting;
        info!("retrying in {delay} s");
    }

    async fn stop_stream(&mut self) {
        if let Some(stream) = self.stream.take() {
            self.last_stats = stream.handle.stats();
            self.consumer = Some(stream.handle.stop());
            // No volume request may follow TEARDOWN, and the worker's session
            // reference must go before the session can be unwrapped.
            stream.volume.stop().await;
            match Arc::try_unwrap(stream.session) {
                Ok(session) => session.teardown().await,
                Err(_) => warn!("session still referenced; closing it without TEARDOWN"),
            }
            // PTP follower and NTP responder stop when `stream`'s fields drop here.
        }
    }

    /// Stop capture on the blocking pool; the producer comes back with the result.
    fn stop_capture(&mut self) {
        if let Some(capture) = self.capture.take() {
            let handle = tokio::task::spawn_blocking(move || {
                let (producer, exit) = capture.stop();
                CaptureDone::Stopped(producer, exit)
            });
            self.capture_task = Some(CaptureTask {
                handle,
                since: Instant::now(),
                starting: false,
            });
        }
    }

    // ------------------------------------------------------------------ status
    fn publish(&self) {
        let mut snap = Snapshot {
            state: self.state,
            // A stream error wins; otherwise say why capture is down (if it is).
            error: if self.last_error.is_empty() {
                self.capture_error.clone()
            } else {
                self.last_error.clone()
            },
            capture_error: self.capture_error.clone(),
            retry_in_s: self
                .retry_at
                .filter(|_| self.state == State::Reconnecting)
                .map(|t| t.saturating_duration_since(Instant::now()).as_secs_f32())
                .unwrap_or(0.0),
            device_ip: self.settings.device_ip.clone(),
            device_name: self
                .receiver
                .as_ref()
                .map(|r| r.name.clone())
                .unwrap_or_default(),
            device_model: self
                .receiver
                .as_ref()
                .map(|r| r.model.clone())
                .unwrap_or_default(),
            discovered_device_name: self.discovered_name.clone(),
            latency_ms: self.settings.latency_ms,
            codec: self.settings.codec,
            timing: self.settings.timing,
            volume: self.settings.volume,
            capture: self.settings.capture.clone(),
            devices: self.devices.clone(),
            ring_dropped: self.ring.dropped(),
            scan: self.scan.clone(),
            ..Snapshot::default()
        };
        if let Some(capture) = &self.capture {
            let info = capture.info();
            snap.capture_name = info.device_name.clone();
            snap.capture_rate = info.sample_rate;
            snap.capture_format = info.format.clone();
            snap.capture_discontinuities = capture.discontinuities();
        } else if self.want_connected && self.capture_error.is_empty() {
            // Restarting (device switch): keep showing the device until it is back.
            snap.capture_name = self.last_capture_name.clone();
            snap.capture_rate = self.last_capture_rate;
            snap.capture_format = self.last_capture_format.clone();
        }
        if let Some(stream) = &self.stream {
            let stats = stream.handle.stats();
            snap.render_ms = stream.params.render_ms;
            snap.uptime_s = stream.started.elapsed().as_secs_f64();
            snap.packets = stats.packets;
            snap.silence_packets = stats.silence_packets;
            snap.frames_trimmed = stats.frames_trimmed;
            snap.stalls = stats.stalls;
            snap.retransmit_requests = stats.retransmit_requests;
            snap.retransmitted = stats.retransmitted;
            snap.anchors = stats.anchors;
            snap.send_errors = stats.send_errors;
            snap.withheld = stats.withheld;
            snap.ring_ms = stats.ring_frames as f32 / 44.1;
            snap.pairing_retries = stream.session.pairing_retries();
            snap.feedback = stream.session.last_feedback();
            if let Some(ptp) = &stream.ptp {
                let clock = ptp.clock();
                snap.clock_ok = clock.locked();
                snap.ptp_offset_ms = clock.offset_ns().unwrap_or(0.0) / 1e6;
                snap.ptp_spread_ms = clock.delay_spread_ns() as f64 / 1e6;
                snap.ptp_syncs = clock.syncs();
            } else if let Some(ntp) = &stream.ntp {
                snap.timing_requests = ntp.requests();
                snap.clock_ok = ntp_clock_ok(snap.timing_requests);
            }
        }
        self.snapshot.store(Arc::new(snap));
    }
}

/// NTP timing is fine once the receiver has had any request answered this session:
/// AirPlay 2 receivers ask rarely (v0.1 showed NTP as OK while streaming).
fn ntp_clock_ok(requests_answered: u64) -> bool {
    requests_answered > 0
}

fn describe_capture_exit(exit: &CaptureExit) -> String {
    match exit {
        CaptureExit::Stopped => "capture stopped".into(),
        CaptureExit::DeviceLost { hresult, detail } => {
            format!("audio device lost ({detail}, {hresult:#010x}); restarting capture")
        }
        CaptureExit::Failed(message) => format!("audio capture failed: {message}"),
    }
}

/// Convenience used by binaries: the capture selection stored in config.
pub fn capture_selection(name: &str, id: Option<String>) -> CaptureSelection {
    if name.trim().is_empty()
        || name.eq_ignore_ascii_case(CaptureSelection::DEFAULT) && id.is_none()
    {
        CaptureSelection::default_output()
    } else {
        CaptureSelection {
            name: name.to_string(),
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Engine behaviour against loopback mocks only: receivers on 127.0.0.1 that
    //! hang or hang up, and the generated test tone as the source (nothing is
    //! played, no AirPlay device is contacted).

    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, UdpSocket};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use winpods_airplay::{Codec, TimingProtocol};

    fn settings(ip: &str, timing: TimingProtocol) -> EngineSettings {
        EngineSettings {
            device_ip: ip.to_string(),
            latency_ms: 100,
            capture: CaptureSelection::default_output(),
            volume: None,
            timing,
            codec: Codec::Alac,
            sender_name: "WinPods test".into(),
            device_id: "02:11:22:33:44:55".into(),
            test_tone: true,
            drop_every: None,
            require_standard_ptp_ports: true,
        }
    }

    fn receiver_at(port: u16) -> ReceiverInfo {
        ReceiverInfo {
            name: "Mock Speaker".into(),
            model: "AudioAccessory5,1".into(),
            address: Ipv4Addr::LOCALHOST,
            raop_port: port,
            features: 0x3C35_4BD0_4A7F_CA00,
            metadata_types: vec![0, 1, 2],
            encryption_types: vec![0, 3, 5],
            receiver_ids: vec!["0A0A0A0A-0000-4000-8000-000000000001".into()],
            requires_password: false,
            raop_txt: Vec::new(),
            airplay_txt: Vec::new(),
        }
    }

    /// RTSP port of a receiver on 127.0.0.1 that accepts connections and then
    /// never answers (a hung HomePod), or hangs up at once.
    struct MockReceiver {
        port: u16,
        accepted: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
    }

    impl MockReceiver {
        fn start(hang_up: bool) -> MockReceiver {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let accepted = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicUsize::new(0));
            let (a, c) = (accepted.clone(), closed.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    a.fetch_add(1, Ordering::SeqCst);
                    if hang_up {
                        drop(stream);
                        c.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                    let c = c.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 4096];
                        while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
                        c.fetch_add(1, Ordering::SeqCst);
                    });
                }
            });
            MockReceiver {
                port,
                accepted,
                closed,
            }
        }
        fn accepted(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }
        fn closed(&self) -> usize {
            self.closed.load(Ordering::SeqCst)
        }
    }

    /// Poll `done` until it holds; returns how long that took.
    fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) -> Duration {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < limit, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
        started.elapsed()
    }

    /// An engine whose receiver at 127.0.0.1 is already "discovered" (no mDNS).
    fn engine_for(settings: EngineSettings, receiver: Option<ReceiverInfo>) -> EngineHandle {
        EngineHandle::spawn(
            settings,
            Box::new(move |engine: &mut Engine| engine.receiver = receiver),
        )
        .expect("engine starts")
    }

    #[test]
    fn disconnect_and_quit_interrupt_a_hung_connect_attempt() {
        let mock = MockReceiver::start(false);
        let engine = engine_for(
            settings("127.0.0.1", TimingProtocol::Ntp),
            Some(receiver_at(mock.port)),
        );
        engine.send(Command::Connect);
        wait_until("the RTSP connection", Duration::from_secs(5), || {
            mock.accepted() == 1
        });
        // GET /info is never answered: the attempt would hang for 10 s.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(engine.snapshot().state, State::Connecting);

        engine.send(Command::Disconnect);
        let took = wait_until("Disconnected", Duration::from_secs(3), || {
            engine.snapshot().state == State::Disconnected
        });
        assert!(took < Duration::from_secs(1), "Disconnect took {took:?}");
        wait_until("the connection to close", Duration::from_secs(2), || {
            mock.closed() == 1
        });

        // A cancelled attempt is not a failure: no backoff, no error.
        engine.send(Command::Connect);
        wait_until("a new attempt", Duration::from_secs(2), || {
            mock.accepted() == 2
        });
        assert_eq!(engine.snapshot().error, "");
        assert_eq!(engine.snapshot().state, State::Connecting);

        let started = Instant::now();
        engine.shutdown();
        let took = started.elapsed();
        assert!(took < Duration::from_secs(2), "Quit took {took:?}");
        wait_until("the connection to close", Duration::from_secs(2), || {
            mock.closed() == 2
        });
    }

    #[test]
    fn setting_change_restarts_a_hung_attempt_at_once() {
        let mock = MockReceiver::start(false);
        let engine = engine_for(
            settings("127.0.0.1", TimingProtocol::Ntp),
            Some(receiver_at(mock.port)),
        );
        engine.send(Command::Connect);
        wait_until("the RTSP connection", Duration::from_secs(5), || {
            mock.accepted() == 1
        });
        engine.send(Command::SetLatency(150));
        let took = wait_until("the new attempt", Duration::from_secs(3), || {
            mock.accepted() == 2
        });
        assert!(took < Duration::from_secs(1), "restart took {took:?}");
        assert_eq!(mock.closed(), 1, "the old attempt's connection is closed");
        assert_eq!(engine.snapshot().latency_ms, 150);
        engine.shutdown();
    }

    #[test]
    fn setting_change_skips_the_reconnect_backoff() {
        let mock = MockReceiver::start(true);
        let engine = engine_for(
            settings("127.0.0.1", TimingProtocol::Ntp),
            Some(receiver_at(mock.port)),
        );
        engine.send(Command::Connect);
        wait_until("the failed attempt", Duration::from_secs(5), || {
            engine.snapshot().state == State::Reconnecting
        });
        let failed = engine.snapshot();
        assert!(
            failed.retry_in_s > 1.0,
            "backing off: {}",
            failed.retry_in_s
        );
        assert!(!failed.error.is_empty());

        engine.send(Command::SetCodec(Codec::Pcm));
        // Without the reset the next attempt would wait out the ~2 s backoff.
        let took = wait_until("a prompt new attempt", Duration::from_secs(3), || {
            let snap = engine.snapshot();
            snap.state == State::Connecting || snap.error != failed.error
        });
        assert!(took < Duration::from_secs(1), "retry took {took:?}");
        engine.shutdown();
    }

    #[test]
    fn busy_ptp_ports_fail_before_the_receiver_is_contacted() {
        // Hold 319/320 on loopback (when someone else already has them, the
        // follower cannot get them either).
        let _event = UdpSocket::bind((Ipv4Addr::LOCALHOST, 319));
        let _general = UdpSocket::bind((Ipv4Addr::LOCALHOST, 320));
        let mock = MockReceiver::start(false);
        let engine = engine_for(
            settings("127.0.0.1", TimingProtocol::Ptp),
            Some(receiver_at(mock.port)),
        );
        engine.send(Command::Connect);
        wait_until("the failed attempt", Duration::from_secs(5), || {
            engine.snapshot().state == State::Reconnecting
        });
        let snap = engine.snapshot();
        assert!(snap.error.contains("319/320"), "{}", snap.error);
        assert_eq!(mock.accepted(), 0, "no pairing when PTP cannot work");
        engine.shutdown();
    }

    #[test]
    fn setting_an_address_clears_the_first_run_error() {
        let engine = engine_for(settings("", TimingProtocol::Ntp), None);
        engine.send(Command::Connect);
        wait_until("the first-run error", Duration::from_secs(3), || {
            engine.snapshot().state == State::Error
        });
        assert!(engine.snapshot().error.contains("No speaker address"));

        // Not connected afterwards, so the TEST-NET address is never contacted.
        engine.send(Command::SetDeviceIp("192.0.2.1".into()));
        wait_until("the error to clear", Duration::from_secs(2), || {
            let snap = engine.snapshot();
            snap.state == State::Disconnected && snap.error.is_empty()
        });
        assert_eq!(engine.snapshot().device_ip, "192.0.2.1");
        engine.shutdown();
    }

    /// An engine driven by hand inside a test runtime.
    fn bare_engine(settings: EngineSettings) -> (Engine, Arc<ArcSwap<Snapshot>>) {
        let (producer, consumer) = winpods_rt::ring(4410).expect("ring");
        let snapshot = Arc::new(ArcSwap::from_pointee(Snapshot::default()));
        let (_commands, command_rx) = unbounded_channel();
        let engine = Engine::new(settings, producer, consumer, snapshot.clone(), command_rx);
        (engine, snapshot)
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn capture_failure_is_reported_while_streaming() {
        test_runtime().block_on(async {
            let mut settings = settings("192.0.2.1", TimingProtocol::Ntp);
            settings.test_tone = false;
            settings.capture = CaptureSelection {
                name: "WinPods Missing Test Device [Loopback]".into(),
                id: Some("{0.0.0.00000000}.{00000000-0000-0000-0000-000000000000}".into()),
            };
            let (mut engine, snapshot) = bare_engine(settings);
            // As if the stream were up and an earlier capture had run.
            engine.want_connected = true;
            engine.state = State::Streaming;
            engine.last_capture_name = "Headphones (USB) [Loopback]".into();
            engine.last_capture_rate = 48_000;
            engine.last_capture_format = "f32".into();

            engine.reconcile_capture();
            assert!(
                engine.capture_task.is_some(),
                "start runs on the blocking pool"
            );
            let joined = join_capture(&mut engine.capture_task).await;
            engine.capture_task = None;
            engine.finish_capture(joined);
            engine.publish();

            let snap = snapshot.load_full();
            assert_eq!(snap.state, State::Streaming);
            assert!(
                snap.capture_error.starts_with("audio capture:"),
                "{}",
                snap.capture_error
            );
            assert_eq!(
                snap.error, snap.capture_error,
                "shown without a stream error"
            );
            assert_eq!(snap.capture_name, "", "no stale device name");
            assert_eq!(snap.capture_rate, 0);
            assert_eq!(snap.capture_format, "");
            assert!(engine.producer.is_some(), "the producer came back");

            // A stream error takes `error`; the capture problem stays visible.
            engine.last_error = "receiver closed the connection".into();
            engine.publish();
            let snap = snapshot.load_full();
            assert_eq!(snap.error, "receiver closed the connection");
            assert!(!snap.capture_error.is_empty());
            engine.last_error.clear();

            // Picking another source clears the old failure.
            engine.handle_command(Command::SetCapture(CaptureSelection::default_output()));
            engine.publish();
            assert_eq!(snapshot.load().capture_error, "");

            engine.capture_error = "audio capture: gone".into();
            engine.handle_command(Command::Disconnect);
            engine.publish();
            assert_eq!(snapshot.load().capture_error, "");
            assert_eq!(snapshot.load().error, "");
        });
    }

    #[test]
    fn device_changes_refresh_the_source_list() {
        test_runtime().block_on(async {
            let (mut engine, _snapshot) = bare_engine(settings("", TimingProtocol::Ntp));
            if engine.watch.is_none() {
                eprintln!("no device watch on this machine; skipping");
                return;
            }
            engine.handle_device_event(DeviceEvent::DevicesChanged);
            assert!(engine.device_list.is_some(), "listing started");
            // A second change while listing asks for one more pass.
            engine.handle_device_event(DeviceEvent::DevicesChanged);
            assert!(engine.devices_stale);
            let joined = join_list(&mut engine.device_list).await;
            engine.device_list = None;
            engine.finish_device_list(joined);
            assert!(!engine.devices_stale);
            assert!(engine.device_list.is_some(), "the extra pass runs");
            let joined = join_list(&mut engine.device_list).await;
            engine.device_list = None;
            engine.finish_device_list(joined);
            assert!(engine.device_list.is_none());
        });
    }

    #[test]
    fn level_meter_falls_to_zero_without_fresh_audio() {
        let ms = 1_000_000u64;
        let now = 10_000 * ms;
        assert_eq!(meter_peak(0.8, now - 10 * ms, now), 0.8);
        assert_eq!(meter_peak(0.8, now - 250 * ms, now), 0.8);
        assert_eq!(meter_peak(0.8, now - 251 * ms, now), 0.0, "idle loopback");
        assert_eq!(meter_peak(0.8, 0, now), 0.0, "never written");
        // A write stamped after `now` (read race) is fresh.
        assert_eq!(meter_peak(0.5, now + ms, now), 0.5);
    }

    #[test]
    fn stopped_or_failed_capture_resets_the_published_peak() {
        test_runtime().block_on(async {
            let (mut engine, _snapshot) = bare_engine(settings("", TimingProtocol::Ntp));
            engine.want_connected = true;
            engine.reconcile_capture(); // the test tone
            let joined = join_capture(&mut engine.capture_task).await;
            engine.capture_task = None;
            engine.finish_capture(joined);
            assert!(engine.capture.is_some(), "tone running");
            // Stop in the middle of a beep (120 ms each second).
            wait_for(|| engine.ring.peak() > 0.0).await;
            engine.stop_capture();
            let joined = join_capture(&mut engine.capture_task).await;
            engine.capture_task = None;
            engine.finish_capture(joined);
            assert_eq!(engine.ring.peak(), 0.0, "after stop");

            // A failed start hands the producer back with the meter at 0 as well.
            engine
                .producer
                .as_ref()
                .expect("producer back")
                .set_peak(0.7);
            engine.settings.test_tone = false;
            engine.settings.capture = CaptureSelection {
                name: "WinPods Missing Test Device".into(),
                id: Some("{0.0.1.00000000}.{00000000-0000-0000-0000-000000000000}".into()),
            };
            engine.capture_retry_at = None;
            engine.reconcile_capture();
            let joined = join_capture(&mut engine.capture_task).await;
            engine.capture_task = None;
            engine.finish_capture(joined);
            assert!(engine.capture.is_none());
            assert!(engine.producer.is_some());
            assert_eq!(engine.ring.peak(), 0.0, "after a failed start");
        });
    }

    async fn wait_for(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("condition in time");
    }

    #[test]
    fn volume_worker_sends_only_the_newest_value_and_never_blocks() {
        test_runtime().block_on(async {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (s, g) = (sent.clone(), gate.clone());
            let worker = VolumeWorker::spawn(move |pct| {
                let (sent, gate) = (s.clone(), g.clone());
                async move {
                    sent.lock().push(pct);
                    // A receiver that takes its time answering.
                    gate.acquire().await.expect("gate").forget();
                }
            });
            worker.set(10.0);
            wait_for(|| sent.lock().len() == 1).await;
            // A slider drag while the first request hangs: none of these wait.
            let started = Instant::now();
            for step in 11..=30 {
                worker.set(step as f32);
            }
            assert!(started.elapsed() < Duration::from_millis(50));
            gate.add_permits(10);
            wait_for(|| sent.lock().len() == 2).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(*sent.lock(), vec![10.0, 30.0], "coalesced to the newest");
            worker.stop().await;
        });
    }

    #[test]
    fn ntp_clock_is_ok_once_a_request_was_answered() {
        assert!(!ntp_clock_ok(0));
        assert!(ntp_clock_ok(1));
        assert!(ntp_clock_ok(500));
    }
}
