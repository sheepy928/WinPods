//! Supervisor: port of `legacy/python/winpods/engine.py` on the new architecture.
//!
//! One engine thread runs a tokio current-thread runtime. A loop reconciles the
//! desired state (connected or not, settings) with reality every 250 ms or when a
//! command/device event arrives:
//!
//! * capture runs while a connection is wanted; it is restarted when the selection
//!   changes, when the device is lost, and (for the default output) when Windows'
//!   default render device changes;
//! * the stream is (re)built as discovery → `Session::connect` → bind control/timing
//!   sockets → `Session::setup` → `Session::start_playback` → `runtime::start`, with
//!   reconnect backoff 2/3/5/10/20/30 s on failure;
//! * a [`Snapshot`] is published through an `ArcSwap` for the UI/CLI.
//!
//! The engine never writes config; it reports what it learns through the snapshot.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use winpods_airplay::ntp::{NtpClock, TimingResponder};
use winpods_airplay::ptp::PtpService;
use winpods_airplay::runtime::{self, ControlSocket, RuntimeOptions, StreamHandle};
use winpods_airplay::session::{Session, SessionConfig, TimingSetup};
use winpods_airplay::{ReceiverInfo, StreamParams, TimingProtocol, mdns};
use winpods_audio::{CaptureExit, CaptureHandle, CaptureSource, DeviceEvent, DeviceWatch};
use winpods_rt::{RingConsumer, RingProducer, RingShared};

use crate::scan;
use crate::types::{
    CaptureSelection, Command, EngineApi, EngineSettings, ScanState, Snapshot, State,
};

const BACKOFF_SECS: [u64; 6] = [2, 3, 5, 10, 20, 30];
const CAPTURE_RETRY: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(250);
const DEVICE_EVENT_DEBOUNCE: Duration = Duration::from_millis(250);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const SCAN_DURATION: Duration = Duration::from_secs(5);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);
const DMAP_TITLE: &str = "Windows audio";

/// Handle to the engine thread.
pub struct EngineHandle {
    commands: UnboundedSender<Command>,
    snapshot: Arc<ArcSwap<Snapshot>>,
    ring: Arc<RingShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    finished: Mutex<Option<std_mpsc::Receiver<()>>>,
}

impl EngineHandle {
    pub fn start(settings: EngineSettings) -> std::io::Result<EngineHandle> {
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
                    engine.run().await;
                });
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
        self.ring.peak()
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

/// The live parts of one stream.
struct Stream {
    session: Session,
    handle: StreamHandle,
    params: StreamParams,
    ptp: Option<Arc<PtpService>>,
    ntp: Option<TimingResponder>,
    started: Instant,
}

struct Engine {
    settings: EngineSettings,
    commands: UnboundedReceiver<Command>,
    snapshot: Arc<ArcSwap<Snapshot>>,

    // ring ends while not owned by a capture / stream
    producer: Option<RingProducer>,
    consumer: Option<RingConsumer>,
    ring: Arc<RingShared>,

    // devices
    watch: Option<DeviceWatch>,
    device_events: UnboundedReceiver<DeviceEvent>,
    pending_default: Option<(String, Instant)>,
    devices: Arc<Vec<winpods_audio::DeviceInfo>>,

    // capture
    capture: Option<CaptureHandle>,
    capture_retry_at: Option<Instant>,
    restart_capture: bool,
    last_capture_name: String,
    last_capture_rate: u32,
    last_capture_format: String,

    // stream
    want_connected: bool,
    stream: Option<Stream>,
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
            Ok(watch) => Some(watch),
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
            capture: None,
            capture_retry_at: None,
            restart_capture: false,
            last_capture_name: String::new(),
            last_capture_rate: 0,
            last_capture_format: String::new(),
            want_connected: false,
            stream: None,
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
                    Some(command) => self.handle_command(command).await,
                    None => self.shutdown = true,
                },
                Some(event) = self.device_events.recv() => self.handle_device_event(event),
                _ = tokio::time::sleep(TICK) => {}
            }
            if self.shutdown {
                break;
            }
            self.poll_scan();
            self.step().await;
            self.publish();
        }
        info!("engine shutting down");
        self.stop_stream().await;
        self.stop_capture();
        self.state = State::Disconnected;
        self.publish();
    }

    // ------------------------------------------------------------------ commands
    async fn handle_command(&mut self, command: Command) {
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
            }
            Command::SetLatency(ms) => {
                let ms = ms.clamp(
                    winpods_airplay::MIN_LATENCY_MS,
                    winpods_airplay::MAX_LATENCY_MS,
                );
                if ms != self.settings.latency_ms {
                    self.settings.latency_ms = ms;
                    self.restart_stream = true;
                }
            }
            Command::SetCapture(selection) => {
                if selection != self.settings.capture {
                    self.settings.capture = selection;
                    self.restart_capture = true;
                }
            }
            Command::SetDeviceIp(ip) => {
                let ip = ip.trim().to_string();
                if ip != self.settings.device_ip {
                    self.settings.device_ip = ip;
                    self.receiver = None;
                    self.discovered_name = None;
                    self.restart_stream = true;
                    self.failures = 0;
                    self.retry_at = None;
                }
            }
            Command::SetVolume(volume) => {
                self.settings.volume = volume;
                if let (Some(pct), Some(stream)) = (volume, self.stream.as_ref())
                    && let Err(err) = stream.session.set_volume(pct).await
                {
                    warn!("volume change failed: {err}");
                }
            }
            Command::SetTiming(timing) => {
                if timing != self.settings.timing {
                    self.settings.timing = timing;
                    self.restart_stream = true;
                }
            }
            Command::SetCodec(codec) => {
                if codec != self.settings.codec {
                    self.settings.codec = codec;
                    self.restart_stream = true;
                }
            }
            Command::RefreshDevices => self.refresh_devices(),
            Command::Scan => self.start_scan(),
            Command::Shutdown => self.shutdown = true,
        }
    }

    fn handle_device_event(&mut self, event: DeviceEvent) {
        match event {
            DeviceEvent::DefaultRenderChanged { id } => {
                debug!(%id, "default render device changed");
                self.pending_default = Some((id, Instant::now() + DEVICE_EVENT_DEBOUNCE));
                self.refresh_devices();
            }
        }
    }

    fn refresh_devices(&mut self) {
        if let Some(watch) = &self.watch {
            match watch.list() {
                Ok(list) => self.devices = Arc::new(list),
                Err(err) => warn!("listing capture devices failed: {err}"),
            }
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
            if self.stream.is_some() || self.capture.is_some() {
                self.stop_stream().await;
                self.stop_capture();
            }
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

        if self.restart_capture {
            self.restart_capture = false;
            self.stop_capture();
            self.capture_retry_at = None;
        }

        // Device lost or capture failed on its own?
        if let Some(exit) = self.capture.as_ref().and_then(|c| c.exit()) {
            warn!(?exit, "capture ended");
            self.last_error = describe_capture_exit(&exit);
            self.stop_capture();
            self.capture_retry_at = Some(Instant::now() + CAPTURE_RETRY);
        }

        if self.capture.is_none() && self.capture_retry_at.is_none_or(|t| Instant::now() >= t) {
            let Some(producer) = self.producer.take() else {
                return;
            };
            let source = self.capture_source();
            match CaptureHandle::start(&source, producer) {
                Ok(capture) => {
                    let info = capture.info();
                    info!(device = %info.device_name, rate = info.sample_rate, format = %info.format, "capture started");
                    self.last_capture_name = info.device_name.clone();
                    self.last_capture_rate = info.sample_rate;
                    self.last_capture_format = info.format.clone();
                    self.capture = Some(capture);
                    self.capture_retry_at = None;
                }
                Err((err, producer)) => {
                    warn!("capture failed to start: {err}");
                    self.last_error = format!("audio capture: {err}");
                    self.producer = Some(producer);
                    self.capture_retry_at = Some(Instant::now() + CAPTURE_RETRY);
                }
            }
        }
    }

    async fn reconcile_stream(&mut self) {
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
        self.state = State::Connecting;
        self.publish();
        match self.connect().await {
            Ok(stream) => {
                info!(
                    receiver = %stream.params.receiver,
                    latency_ms = stream.params.latency_ms,
                    "streaming"
                );
                self.stream = Some(stream);
                self.state = State::Streaming;
                self.last_error.clear();
                self.failures = 0;
                self.retry_at = None;
            }
            Err(message) => {
                error!("connect failed: {message}");
                self.last_error = message;
                self.receiver = None;
                self.schedule_retry();
            }
        }
    }

    fn schedule_retry(&mut self) {
        let delay = BACKOFF_SECS[self.failures.min(BACKOFF_SECS.len() - 1)];
        self.failures += 1;
        self.retry_at = Some(Instant::now() + Duration::from_secs(delay));
        self.state = State::Reconnecting;
        info!("retrying in {delay} s");
    }

    /// Discovery → session → sockets → setup → playback → runtime. On failure all
    /// partial resources are released and the ring consumer is kept.
    async fn connect(&mut self) -> Result<Stream, String> {
        let address: Ipv4Addr = self
            .settings
            .device_ip
            .trim()
            .parse()
            .map_err(|_| format!("'{}' is not an IPv4 address", self.settings.device_ip))?;

        let info = match &self.receiver {
            Some(info) if info.address == address => info.clone(),
            _ => {
                let found = mdns::lookup(address, LOOKUP_TIMEOUT)
                    .await
                    .map_err(|err| format!("discovery failed: {err}"))?
                    .ok_or_else(|| format!("No AirPlay receiver answered at {address}"))?;
                info!(name = %found.name, model = %found.model, "found receiver");
                self.discovered_name = Some(found.name.clone());
                self.receiver = Some(found.clone());
                found
            }
        };
        if !info.is_airplay2() {
            return Err(format!("{} is not an AirPlay 2 receiver", info.name));
        }

        let config = SessionConfig {
            sender_name: self.settings.sender_name.clone(),
            device_id: self.settings.device_id.clone(),
            latency_ms: self.settings.latency_ms,
            codec: self.settings.codec,
            volume_pct: self.settings.volume,
            title: DMAP_TITLE.to_string(),
        };
        let mut session = Session::connect(&info, config)
            .await
            .map_err(|e| e.to_string())?;
        let local_ip = session.local_ip();

        let result = self.setup_stream(&mut session, local_ip).await;
        match result {
            Ok((handle, params, ptp, ntp)) => Ok(Stream {
                session,
                handle,
                params,
                ptp,
                ntp,
                started: Instant::now(),
            }),
            Err(message) => {
                session.teardown().await;
                Err(message)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    async fn setup_stream(
        &mut self,
        session: &mut Session,
        local_ip: Ipv4Addr,
    ) -> Result<
        (
            StreamHandle,
            StreamParams,
            Option<Arc<PtpService>>,
            Option<TimingResponder>,
        ),
        String,
    > {
        let control = ControlSocket::bind(local_ip).map_err(|e| format!("control socket: {e}"))?;
        let mut options = RuntimeOptions {
            drop_every: self.settings.drop_every,
            ..Default::default()
        };
        let (timing, ptp, ntp) = match self.settings.timing {
            TimingProtocol::Ptp => {
                let ptp = PtpService::start(local_ip).map_err(|e| e.to_string())?;
                if !ptp.standard_ports() {
                    let message = "UDP ports 319/320 are in use by another program (is WinPods v0.1 \
                                   or another AirPlay sender running?)";
                    if self.settings.require_standard_ptp_ports {
                        return Err(message.to_string());
                    }
                    warn!("{message}; using port {}", ptp.port());
                }
                options.ptp = Some(ptp.clock());
                (TimingSetup::Ptp(ptp.clone()), Some(ptp), None)
            }
            TimingProtocol::Ntp => {
                let clock = Arc::new(NtpClock::new());
                let responder = TimingResponder::start(local_ip, clock.clone())
                    .map_err(|e| format!("timing socket: {e}"))?;
                options.ntp = Some(clock);
                (
                    TimingSetup::Ntp {
                        timing_port: responder.port(),
                    },
                    None,
                    Some(responder),
                )
            }
        };

        let params = session
            .setup(timing, control.port())
            .await
            .map_err(|e| e.to_string())?;
        session
            .start_playback(&params)
            .await
            .map_err(|e| e.to_string())?;

        let consumer = self
            .consumer
            .take()
            .ok_or("internal: ring consumer missing")?;
        match runtime::start(&params, control, consumer, options) {
            Ok(handle) => Ok((handle, params, ptp, ntp)),
            Err((err, consumer)) => {
                self.consumer = Some(consumer);
                Err(format!("could not start the stream: {err}"))
            }
        }
    }

    async fn stop_stream(&mut self) {
        if let Some(stream) = self.stream.take() {
            self.last_stats = stream.handle.stats();
            let consumer = stream.handle.stop();
            self.consumer = Some(consumer);
            stream.session.teardown().await;
            // PTP follower and NTP responder stop when `stream`'s fields drop here.
        }
    }

    fn stop_capture(&mut self) {
        if let Some(capture) = self.capture.take() {
            let (producer, exit) = capture.stop();
            debug!(?exit, "capture stopped");
            self.producer = Some(producer);
        }
    }

    // ------------------------------------------------------------------ status
    fn publish(&self) {
        let mut snap = Snapshot {
            state: self.state,
            error: self.last_error.clone(),
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
        } else if self.want_connected {
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
                let last = ntp.last_request_ns();
                snap.clock_ok =
                    last != 0 && winpods_rt::mono_ns().saturating_sub(last) < 15_000_000_000;
            }
        }
        self.snapshot.store(Arc::new(snap));
    }
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
