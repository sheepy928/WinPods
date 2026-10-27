//! `--demo`: a mock engine with plausible, changing data so the UI can be explored and
//! screenshot without a speaker, audio capture or any network access.
//!
//! Usage: `WinPods.exe --demo [--theme light|dark] [--page main|settings] [--update]`.
//! Demo mode never saves config, touches the Run key or checks for updates (see
//! [`UiContext::demo`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use winpods_app::Config;
use winpods_engine::{
    CaptureSelection, Command, DeviceInfo, DeviceKind, EngineApi, ScanResult, ScanState, Snapshot,
    State,
};

use crate::ui::{self, ColorSchemeOverride, Page, RunOptions, UiContext};
use crate::view;

/// Fake receivers returned by a scan: (name, address, model).
const SPEAKERS: [(&str, &str, &str); 2] = [
    ("Living Room", "192.168.1.42", "HomePod mini"),
    ("Kitchen", "192.168.1.57", "HomePod"),
];
/// How long a fake connection attempt takes.
const CONNECT_TIME: Duration = Duration::from_millis(1500);
/// How long a fake stream restart (settings change) takes.
const RESTART_TIME: Duration = Duration::from_millis(800);
/// How long a fake scan takes.
const SCAN_TIME: Duration = Duration::from_secs(2);
/// Stats update period of the mock.
const TICK: Duration = Duration::from_millis(100);
/// AirPlay packets per second (44.1 kHz / 352 frames).
const PACKETS_PER_S: f64 = 44_100.0 / 352.0;

/// Command-line options of demo mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DemoOptions {
    pub theme: ColorSchemeOverride,
    pub page: Page,
    /// Pretend an update is available from the start.
    pub update: bool,
}

impl DemoOptions {
    /// Parse `--theme light|dark|system`, `--page main|settings` and `--update`;
    /// unknown arguments and values are ignored.
    pub fn parse(args: &[String]) -> DemoOptions {
        let mut options = DemoOptions::default();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--theme" => {
                    options.theme = match iter.next().map(|v| v.to_ascii_lowercase()).as_deref() {
                        Some("light") => ColorSchemeOverride::Light,
                        Some("dark") => ColorSchemeOverride::Dark,
                        _ => ColorSchemeOverride::System,
                    }
                }
                "--page" => {
                    options.page = match iter.next().map(|v| v.to_ascii_lowercase()).as_deref() {
                        Some("settings") => Page::Settings,
                        _ => Page::Main,
                    }
                }
                "--update" => options.update = true,
                _ => {}
            }
        }
        options
    }
}

/// Run the UI against a [`MockEngine`] with a default config (`--demo`).
pub fn run_demo(args: &[String]) -> anyhow::Result<()> {
    let options = DemoOptions::parse(args);
    let engine = Arc::new(MockEngine::new());
    let for_shutdown = Arc::clone(&engine);
    let ctx = UiContext {
        engine,
        config: Config::default(),
        start_hidden: false,
        instance: None,
        updater: None,
        version: env!("CARGO_PKG_VERSION").to_string(),
        updated_from: None,
        shutdown: Box::new(move || for_shutdown.send(Command::Shutdown)),
        demo: true,
    };
    ui::run_with(
        ctx,
        RunOptions {
            color_scheme: options.theme,
            start_page: options.page,
            demo_update_available: options.update,
        },
    )
}

/// Something the mock will do later (fake asynchronous engine work).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    StartStreaming,
    FinishScan,
}

struct Inner {
    snap: Snapshot,
    pending: Vec<(Instant, Pending)>,
    streaming_since: Option<Instant>,
    last_tick: Instant,
    rng: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    published: Mutex<Arc<Snapshot>>,
    streaming: AtomicBool,
    stop: AtomicBool,
    epoch: Instant,
}

/// A fake [`EngineApi`]: Connect goes Connecting → Streaming after 1.5 s, Scan returns
/// two speakers after 2 s, stats change every 100 ms while "streaming".
pub struct MockEngine {
    shared: Arc<Shared>,
    ticker: Mutex<Option<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MockEngine {
    /// A disconnected mock with a configured speaker and four capture devices.
    pub fn new() -> MockEngine {
        Self::build(true)
    }

    /// A mock without the background ticker (tests drive `Shared::tick` by hand).
    #[cfg(test)]
    fn manual() -> MockEngine {
        Self::build(false)
    }

    fn build(spawn_ticker: bool) -> MockEngine {
        let now = Instant::now();
        let snap = initial_snapshot();
        let shared = Arc::new(Shared {
            published: Mutex::new(Arc::new(snap.clone())),
            inner: Mutex::new(Inner {
                snap,
                pending: Vec::new(),
                streaming_since: None,
                last_tick: now,
                rng: 0x9E37_79B9_7F4A_7C15,
            }),
            streaming: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            epoch: now,
        });
        let worker = Arc::clone(&shared);
        let ticker = if spawn_ticker {
            std::thread::Builder::new()
                .name("demo-engine".into())
                .spawn(move || {
                    while !worker.stop.load(Ordering::Relaxed) {
                        std::thread::sleep(TICK);
                        worker.tick(Instant::now());
                    }
                })
                .ok()
        } else {
            None
        };
        MockEngine {
            shared,
            ticker: Mutex::new(ticker),
        }
    }

    fn stop_ticker(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = lock(&self.ticker).take() {
            let _ = handle.join();
        }
    }
}

impl Default for MockEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MockEngine {
    fn drop(&mut self) {
        self.stop_ticker();
    }
}

impl EngineApi for MockEngine {
    fn send(&self, command: Command) {
        if command == Command::Shutdown {
            self.stop_ticker();
        }
        self.shared.apply(command, Instant::now());
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&lock(&self.shared.published))
    }

    fn peak(&self) -> f32 {
        if !self.shared.streaming.load(Ordering::Relaxed) {
            return 0.0;
        }
        synthetic_peak(self.shared.epoch.elapsed().as_secs_f32())
    }
}

/// Music-like level: a slow swell plus a decaying hit twice a second.
fn synthetic_peak(t: f32) -> f32 {
    let beat = (t * 2.0).fract();
    let hit = (-beat * 5.0).exp() * (0.7 + 0.3 * (t * 13.0).sin());
    let swell = 0.16 + 0.10 * (t * 0.7).sin() + 0.04 * (t * 3.1).sin();
    (swell + 0.45 * hit).clamp(0.0, 1.0)
}

fn demo_devices() -> Vec<DeviceInfo> {
    let device = |id: &str, name: &str, kind, rate, channels, is_default| DeviceInfo {
        id: format!("{{0.0.0.00000000}}.{{{id}}}"),
        name: name.to_string(),
        display_name: match kind {
            DeviceKind::Loopback => format!("{name} [Loopback]"),
            DeviceKind::Input => name.to_string(),
        },
        kind,
        sample_rate: rate,
        channels,
        is_default,
    };
    vec![
        device(
            "demo-speakers",
            "Speakers (Realtek(R) Audio)",
            DeviceKind::Loopback,
            48_000,
            2,
            true,
        ),
        device(
            "demo-headphones",
            "Headphones (WH-1000XM5 Stereo)",
            DeviceKind::Loopback,
            44_100,
            2,
            false,
        ),
        device(
            "demo-cable",
            "CABLE Input (VB-Audio Virtual Cable)",
            DeviceKind::Loopback,
            48_000,
            2,
            false,
        ),
        device(
            "demo-mic",
            "Microphone (USB Audio Device)",
            DeviceKind::Input,
            48_000,
            1,
            false,
        ),
    ]
}

fn initial_snapshot() -> Snapshot {
    let (name, address, model) = SPEAKERS[0];
    let mut snap = Snapshot {
        device_ip: address.to_string(),
        device_name: name.to_string(),
        device_model: model.to_string(),
        latency_ms: 100,
        devices: Arc::new(demo_devices()),
        capture: CaptureSelection::default_output(),
        ..Snapshot::default()
    };
    resolve_capture(&mut snap);
    snap
}

/// Fill `capture_name/rate/format` for the selected source.
fn resolve_capture(snap: &mut Snapshot) {
    let device = if snap.capture.is_default() {
        snap.devices.iter().find(|d| d.is_default)
    } else {
        snap.devices.iter().find(|d| {
            snap.capture.id.as_deref() == Some(d.id.as_str()) || d.display_name == snap.capture.name
        })
    };
    match device {
        Some(device) => {
            snap.capture_name = device.name.clone();
            snap.capture_rate = device.sample_rate;
            snap.capture_format = "f32".to_string();
        }
        None => {
            snap.capture_name.clear();
            snap.capture_rate = 0;
            snap.capture_format.clear();
        }
    }
}

impl Inner {
    fn next_random(&mut self) -> u64 {
        // xorshift64*: plenty for fake jitter.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn random_unit(&mut self) -> f64 {
        (self.next_random() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn schedule(&mut self, at: Instant, what: Pending) {
        self.pending.retain(|(_, p)| *p != what);
        self.pending.push((at, what));
    }

    fn reset_stream_stats(&mut self) {
        let snap = &mut self.snap;
        snap.uptime_s = 0.0;
        snap.packets = 0;
        snap.silence_packets = 0;
        snap.frames_trimmed = 0;
        snap.stalls = 0;
        snap.retransmit_requests = 0;
        snap.retransmitted = 0;
        snap.anchors = 0;
        snap.send_errors = 0;
        snap.ring_ms = 0.0;
        snap.clock_ok = false;
        snap.ptp_syncs = 0;
        self.streaming_since = None;
    }

    /// Begin a (re)connection that completes after `delay`.
    fn begin_connect(&mut self, now: Instant, delay: Duration) {
        self.reset_stream_stats();
        self.snap.state = State::Connecting;
        self.snap.error.clear();
        self.schedule(now + delay, Pending::StartStreaming);
    }

    /// Restart the stream if one is up or coming up (settings changed).
    fn restart_if_active(&mut self, now: Instant) {
        if matches!(self.snap.state, State::Streaming | State::Connecting) {
            self.begin_connect(now, RESTART_TIME);
        }
    }

    fn run_pending(&mut self, now: Instant) {
        let (due, later): (Vec<_>, Vec<_>) = self.pending.drain(..).partition(|(at, _)| *at <= now);
        self.pending = later;
        for (_, what) in due {
            match what {
                Pending::StartStreaming if self.snap.state == State::Connecting => {
                    self.snap.state = State::Streaming;
                    self.snap.render_ms = Some(69);
                    self.snap.clock_ok = true;
                    self.snap.ptp_offset_ms = 0.4;
                    self.snap.ptp_spread_ms = 0.08;
                    if !self.snap.device_name.is_empty() {
                        self.snap.discovered_device_name = Some(self.snap.device_name.clone());
                    }
                    self.streaming_since = Some(now);
                }
                Pending::StartStreaming => {}
                Pending::FinishScan => {
                    let found = SPEAKERS
                        .iter()
                        .map(|&(name, address, model)| ScanResult {
                            name: name.to_string(),
                            address: address.to_string(),
                            model: model.to_string(),
                        })
                        .collect();
                    self.snap.scan = ScanState::Done(Arc::new(found));
                }
            }
        }
    }

    fn advance_stats(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        let Some(since) = self.streaming_since else {
            return;
        };
        let jitter = self.random_unit();
        let resend = self.random_unit();
        let snap = &mut self.snap;
        snap.uptime_s = now.saturating_duration_since(since).as_secs_f64();
        snap.packets = (snap.uptime_s * PACKETS_PER_S) as u64;
        snap.anchors = snap.uptime_s as u64 + 1;
        snap.ptp_syncs = (snap.uptime_s * 8.0) as u64;
        snap.ring_ms = (2.5 + 3.0 * jitter) as f32;
        snap.ptp_offset_ms = 0.35 + 0.1 * jitter;
        // Wi-Fi hiccup now and then: the HomePod asks for a few packets again.
        if resend < dt / 20.0 {
            snap.retransmit_requests += 1;
            snap.retransmitted += 1 + (jitter * 3.0) as u64;
        }
    }

    fn apply(&mut self, command: Command, now: Instant) {
        match command {
            Command::Connect => {
                if self.snap.device_ip.trim().is_empty() {
                    self.snap.state = State::Error;
                    self.snap.error =
                        "No speaker address set: enter the speaker's IP address or use Find."
                            .to_string();
                } else if !matches!(self.snap.state, State::Streaming | State::Connecting) {
                    self.begin_connect(now, CONNECT_TIME);
                }
            }
            Command::Disconnect | Command::Shutdown => {
                self.pending.retain(|(_, p)| *p != Pending::StartStreaming);
                self.reset_stream_stats();
                self.snap.state = State::Disconnected;
                self.snap.render_ms = None;
            }
            Command::SetLatency(ms) => {
                self.snap.latency_ms = view::clamp_latency(i64::from(ms));
                self.restart_if_active(now);
            }
            Command::SetCapture(selection) => {
                self.snap.capture = selection;
                resolve_capture(&mut self.snap);
            }
            Command::SetDeviceIp(address) => {
                let address = address.trim().to_string();
                let known = SPEAKERS.iter().find(|(_, ip, _)| *ip == address);
                self.snap.device_name = known.map(|s| s.0.to_string()).unwrap_or_default();
                self.snap.device_model = known.map(|s| s.2.to_string()).unwrap_or_default();
                self.snap.device_ip = address;
                self.restart_if_active(now);
            }
            Command::SetVolume(volume) => self.snap.volume = volume,
            Command::SetTiming(timing) => {
                self.snap.timing = timing;
                self.restart_if_active(now);
            }
            Command::SetCodec(codec) => {
                self.snap.codec = codec;
                self.restart_if_active(now);
            }
            Command::RefreshDevices => {
                self.snap.devices = Arc::new(demo_devices());
                resolve_capture(&mut self.snap);
            }
            Command::Scan => {
                self.snap.scan = ScanState::Running;
                self.schedule(now + SCAN_TIME, Pending::FinishScan);
            }
        }
    }
}

impl Shared {
    fn publish(&self, inner: &Inner) {
        self.streaming
            .store(inner.snap.state == State::Streaming, Ordering::Relaxed);
        *lock(&self.published) = Arc::new(inner.snap.clone());
    }

    fn apply(&self, command: Command, now: Instant) {
        let mut inner = lock(&self.inner);
        inner.apply(command, now);
        self.publish(&inner);
    }

    fn tick(&self, now: Instant) {
        let mut inner = lock(&self.inner);
        inner.run_pending(now);
        inner.advance_stats(now);
        self.publish(&inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_demo_options() {
        let default = DemoOptions::parse(&args(&["--demo"]));
        assert_eq!(default, DemoOptions::default());
        let dark = DemoOptions::parse(&args(&[
            "--demo", "--theme", "dark", "--page", "settings", "--update",
        ]));
        assert_eq!(dark.theme, ColorSchemeOverride::Dark);
        assert_eq!(dark.page, Page::Settings);
        assert!(dark.update);
        let light = DemoOptions::parse(&args(&["--theme", "LIGHT"]));
        assert_eq!(light.theme, ColorSchemeOverride::Light);
        let bad = DemoOptions::parse(&args(&["--theme", "purple", "--page"]));
        assert_eq!(bad.theme, ColorSchemeOverride::System);
        assert_eq!(bad.page, Page::Main);
    }

    #[test]
    fn connect_goes_connecting_then_streaming() {
        let shared = MockEngine::manual().shared.clone();
        let t0 = Instant::now();
        shared.apply(Command::Connect, t0);
        assert_eq!(lock(&shared.published).state, State::Connecting);
        shared.tick(t0 + Duration::from_millis(500));
        assert_eq!(lock(&shared.published).state, State::Connecting);
        shared.tick(t0 + CONNECT_TIME);
        let snap = Arc::clone(&lock(&shared.published));
        assert_eq!(snap.state, State::Streaming);
        assert!(snap.clock_ok);
        assert_eq!(snap.render_ms, Some(69));
        assert_eq!(snap.discovered_device_name.as_deref(), Some("Living Room"));
        shared.tick(t0 + CONNECT_TIME + Duration::from_secs(10));
        let snap = Arc::clone(&lock(&shared.published));
        assert!(snap.uptime_s >= 9.9);
        assert!(snap.packets > 1000);
        assert!(snap.ring_ms > 0.0);
    }

    #[test]
    fn scan_finishes_with_two_speakers() {
        let engine = MockEngine::manual();
        let t0 = Instant::now();
        engine.shared.apply(Command::Scan, t0);
        assert_eq!(engine.snapshot().scan, ScanState::Running);
        engine.shared.tick(t0 + SCAN_TIME);
        match &engine.snapshot().scan {
            ScanState::Done(found) => {
                assert_eq!(found.len(), 2);
                assert_eq!(found[0].name, "Living Room");
            }
            other => panic!("unexpected scan state {other:?}"),
        }
    }

    #[test]
    fn disconnect_and_errors() {
        let engine = MockEngine::manual();
        let t0 = Instant::now();
        engine.shared.apply(Command::Connect, t0);
        engine.shared.apply(Command::Disconnect, t0);
        engine.shared.tick(t0 + CONNECT_TIME);
        assert_eq!(engine.snapshot().state, State::Disconnected);
        engine.shared.apply(Command::SetDeviceIp(String::new()), t0);
        engine.shared.apply(Command::Connect, t0);
        let snap = engine.snapshot();
        assert_eq!(snap.state, State::Error);
        assert!(!snap.error.is_empty());
    }

    #[test]
    fn settings_changes_are_reflected() {
        let engine = MockEngine::manual();
        let t0 = Instant::now();
        engine.shared.apply(Command::SetLatency(20), t0);
        assert_eq!(engine.snapshot().latency_ms, 75);
        engine.shared.apply(Command::SetVolume(Some(35.0)), t0);
        assert_eq!(engine.snapshot().volume, Some(35.0));
        engine
            .shared
            .apply(Command::SetDeviceIp("192.168.1.57".into()), t0);
        assert_eq!(engine.snapshot().device_name, "Kitchen");
        let devices = engine.snapshot().devices.clone();
        let cable = &devices[2];
        engine.shared.apply(
            Command::SetCapture(CaptureSelection {
                name: cable.display_name.clone(),
                id: Some(cable.id.clone()),
            }),
            t0,
        );
        assert_eq!(engine.snapshot().capture_name, cable.name);
        // Restart while streaming.
        engine.shared.apply(Command::Connect, t0);
        engine.shared.tick(t0 + CONNECT_TIME);
        assert_eq!(engine.snapshot().state, State::Streaming);
        engine
            .shared
            .apply(Command::SetLatency(250), t0 + CONNECT_TIME);
        assert_eq!(engine.snapshot().state, State::Connecting);
        engine.shared.tick(t0 + CONNECT_TIME + RESTART_TIME);
        assert_eq!(engine.snapshot().state, State::Streaming);
    }

    #[test]
    fn peak_is_zero_unless_streaming_and_bounded() {
        let engine = MockEngine::new();
        assert_eq!(engine.peak(), 0.0);
        for i in 0..200 {
            let p = synthetic_peak(i as f32 * 0.037);
            assert!((0.0..=1.0).contains(&p));
        }
        engine.send(Command::Shutdown);
        assert_eq!(engine.snapshot().state, State::Disconnected);
    }
}
