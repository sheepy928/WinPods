//! Pure presentation helpers: turn engine snapshots and config values into the text,
//! indices and decisions the window and the tray show. No Slint or Windows types here,
//! so everything is unit tested.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winpods_airplay::types::{DEFAULT_RENDER_MS, MAX_LATENCY_MS, MIN_LATENCY_MS};
use winpods_engine::{
    CaptureSelection, Codec, DeviceInfo, DeviceKind, ScanResult, ScanState, Snapshot, State,
    TimingProtocol,
};

/// Application name used in titles and the tray tooltip.
pub const APP_NAME: &str = "WinPods";

/// Latency presets offered by the segmented control and the tray submenu (ms).
pub const LATENCY_PRESETS: [u32; 5] = [100, 250, 500, 1000, 2000];

/// Safety margin on top of the receiver's render time below which we warn (ms).
pub const LATENCY_SAFETY_MS: u32 = 5;

/// Windows tooltips hold 128 UTF-16 units; we stay well below (plan: < 100 chars).
pub const TOOLTIP_MAX_CHARS: usize = 99;

/// Label of the first source entry (follows the Windows default output device).
pub const DEFAULT_SOURCE_LABEL: &str = "Windows default output";

/// Clamp a requested latency to the supported range.
pub fn clamp_latency(ms: i64) -> u32 {
    ms.clamp(i64::from(MIN_LATENCY_MS), i64::from(MAX_LATENCY_MS)) as u32
}

/// Index of `ms` in [`LATENCY_PRESETS`], if it is a preset.
pub fn preset_index(ms: u32) -> Option<usize> {
    LATENCY_PRESETS.iter().position(|&preset| preset == ms)
}

/// Short latency label: "100 ms", "1 s", "1.5 s".
pub fn latency_label(ms: u32) -> String {
    if ms >= 1000 && ms.is_multiple_of(100) {
        let seconds = f64::from(ms) / 1000.0;
        if ms.is_multiple_of(1000) {
            format!("{seconds:.0} s")
        } else {
            format!("{seconds:.1} s")
        }
    } else {
        format!("{ms} ms")
    }
}

/// Tray submenu label for a preset, e.g. "100 ms - Ultra low".
pub fn tray_latency_label(ms: u32) -> String {
    let name = match ms {
        100 => "Ultra low",
        250 => "Low",
        500 => "Normal",
        1000 => "Safe",
        2000 => "AirPlay default",
        _ => return latency_label(ms),
    };
    format!("{} - {name}", latency_label(ms))
}

/// Latency versus the receiver's render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyHint {
    /// Render time used for the comparison (reported, else [`DEFAULT_RENDER_MS`]).
    pub render_ms: u32,
    /// Latency minus render time (negative when the latency is too small).
    pub headroom_ms: i64,
    /// Latency is below render time + [`LATENCY_SAFETY_MS`]: expect dropouts.
    pub warning: bool,
}

impl LatencyHint {
    /// One-line hint shown under the latency control.
    pub fn text(&self) -> String {
        if self.warning {
            format!(
                "Below the speaker's {} ms render time + {} ms: expect dropouts",
                self.render_ms, LATENCY_SAFETY_MS
            )
        } else {
            format!(
                "Headroom {} ms over the speaker's {} ms render time",
                self.headroom_ms, self.render_ms
            )
        }
    }
}

/// Compare a latency with the receiver render time (`render_ms` from the snapshot,
/// defaulting to 70 ms when the receiver has not reported one).
pub fn latency_hint(latency_ms: u32, render_ms: Option<u32>) -> LatencyHint {
    let render_ms = render_ms.unwrap_or(DEFAULT_RENDER_MS);
    LatencyHint {
        render_ms,
        headroom_ms: i64::from(latency_ms) - i64::from(render_ms),
        warning: latency_ms < render_ms.saturating_add(LATENCY_SAFETY_MS),
    }
}

/// Colour family of the status pill and the tray icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Idle,
    Busy,
    Ok,
    Error,
}

impl From<State> for StatusKind {
    fn from(state: State) -> Self {
        match state {
            State::Disconnected => StatusKind::Idle,
            State::Connecting | State::Reconnecting => StatusKind::Busy,
            State::Streaming => StatusKind::Ok,
            State::Error => StatusKind::Error,
        }
    }
}

/// Text of the status pill: "Streaming · 100 ms", "Reconnecting in 3 s", ...
pub fn status_text(snap: &Snapshot, has_address: bool) -> String {
    match snap.state {
        State::Streaming => format!("Streaming · {}", latency_label(snap.latency_ms)),
        State::Connecting => "Connecting…".to_string(),
        State::Reconnecting => {
            let seconds = snap.retry_in_s.max(0.0).ceil() as u32;
            if seconds > 0 {
                format!("Reconnecting in {seconds} s")
            } else {
                "Reconnecting…".to_string()
            }
        }
        State::Error => "Error".to_string(),
        State::Disconnected if !has_address => "Not set up".to_string(),
        State::Disconnected => "Disconnected".to_string(),
    }
}

/// "m:ss", or "h:mm:ss" from one hour on.
pub fn format_uptime(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Clock state for the stats line: "PTP locked", "NTP waiting", ...
pub fn clock_label(timing: TimingProtocol, ok: bool) -> String {
    let state = match (timing, ok) {
        (TimingProtocol::Ptp, true) => "locked",
        (TimingProtocol::Ntp, true) => "ok",
        (_, false) => "waiting",
    };
    format!("{} {state}", timing.as_str())
}

/// Display name of a codec.
pub fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::Alac => "ALAC",
        Codec::Pcm => "PCM",
    }
}

fn plural(count: u64, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Compact stats line shown while streaming (empty otherwise):
/// "PTP locked · ALAC · 2:05 · 0 resent · 1 pause · ring 4 ms".
pub fn stats_line(snap: &Snapshot) -> String {
    if snap.state != State::Streaming {
        return String::new();
    }
    [
        clock_label(snap.timing, snap.clock_ok),
        codec_label(snap.codec).to_string(),
        format_uptime(snap.uptime_s),
        format!("{} resent", snap.retransmitted),
        plural(snap.stalls, "pause", "pauses"),
        format!("ring {:.0} ms", snap.ring_ms.max(0.0)),
    ]
    .join(" · ")
}

/// Receiver address in effect: the engine's, else the saved one.
pub fn effective_address<'a>(snap: &'a Snapshot, saved_ip: &'a str) -> &'a str {
    if snap.device_ip.trim().is_empty() {
        saved_ip.trim()
    } else {
        snap.device_ip.trim()
    }
}

/// Shown under the speaker card when Connect is pressed before an address is set.
pub const NO_ADDRESS_HINT: &str =
    "No speaker address set: enter the HomePod's IP address or use Find in Settings.";

/// The line under the speaker card: the engine's error while it is in `Error` or
/// `Reconnecting`, else [`NO_ADDRESS_HINT`] when Connect was pressed without an
/// address, else nothing.
pub fn error_line(snap: &Snapshot, no_address_hint: bool) -> String {
    let engine_error = match snap.state {
        State::Error | State::Reconnecting => snap.error.trim(),
        _ => "",
    };
    if !engine_error.is_empty() {
        engine_error.to_string()
    } else if no_address_hint {
        NO_ADDRESS_HINT.to_string()
    } else {
        String::new()
    }
}

/// The snapshot as the card and the tray show it. The engine's `Error` state only means
/// "no speaker address" (it gives up until the next Connect), so once an address is
/// known and no connection is wanted, that error is stale and is shown as
/// `Disconnected`. While a connection is wanted the engine's state is shown as is.
pub fn presented(snap: &Arc<Snapshot>, saved_ip: &str, wants: bool) -> Arc<Snapshot> {
    let stale_error =
        snap.state == State::Error && !wants && !effective_address(snap, saved_ip).is_empty();
    if !stale_error {
        return Arc::clone(snap);
    }
    Arc::new(Snapshot {
        state: State::Disconnected,
        error: String::new(),
        ..Snapshot::clone(snap)
    })
}

/// True when `latest` lists other capture devices than `known` (compared by value:
/// the engine publishes a new list on every refresh, even an unchanged one).
pub fn devices_changed(
    known: Option<&Arc<Vec<DeviceInfo>>>,
    latest: &Arc<Vec<DeviceInfo>>,
) -> bool {
    match known {
        None => true,
        Some(known) => !Arc::ptr_eq(known, latest) && **known != **latest,
    }
}

/// The engine has given up connecting because no receiver address is set anywhere.
///
/// Evaluated on every snapshot (not only on state transitions): the engine stays in
/// `Error` across repeated Connect attempts without an address, so a transition-only
/// check would leave the Connect switch on after the second attempt.
pub fn engine_gave_up(snap: &Snapshot, saved_ip: &str) -> bool {
    snap.state == State::Error && effective_address(snap, saved_ip).is_empty()
}

/// The engine-discovered receiver name to persist in config, if any.
///
/// Only a name reported for the configured address counts: after the user changes the
/// address, snapshots can still carry the previous receiver's name (until the engine
/// handles `SetDeviceIp` and rediscovers), and saving it would label the new address
/// with the old speaker's name.
pub fn name_to_persist<'a>(
    snap: &'a Snapshot,
    saved_ip: &str,
    saved_name: &str,
) -> Option<&'a str> {
    let name = snap.discovered_device_name.as_deref()?.trim();
    let engine_ip = snap.device_ip.trim();
    let for_saved_address = !engine_ip.is_empty() && engine_ip == saved_ip.trim();
    (for_saved_address && !name.is_empty() && name != saved_name.trim()).then_some(name)
}

/// Title of the speaker card.
pub fn speaker_title(snap: &Snapshot, saved_name: &str, saved_ip: &str) -> String {
    if !snap.device_name.trim().is_empty() {
        snap.device_name.trim().to_string()
    } else if !saved_name.trim().is_empty() {
        saved_name.trim().to_string()
    } else if !effective_address(snap, saved_ip).is_empty() {
        "AirPlay speaker".to_string()
    } else {
        "No speaker selected".to_string()
    }
}

/// Second line of the speaker card: "HomePod mini · 192.168.1.20".
pub fn speaker_detail(snap: &Snapshot, saved_ip: &str) -> String {
    let address = effective_address(snap, saved_ip);
    if address.is_empty() {
        return "Choose your speaker in Settings".to_string();
    }
    let model = snap.device_model.trim();
    if model.is_empty() {
        address.to_string()
    } else {
        format!("{model} · {address}")
    }
}

/// What is being captured: "Capturing Speakers (Realtek(R) Audio) · 48 kHz".
pub fn capture_detail(snap: &Snapshot) -> String {
    if snap.capture_name.trim().is_empty() {
        return "Not capturing".to_string();
    }
    if snap.capture_rate == 0 {
        return format!("Capturing {}", snap.capture_name.trim());
    }
    let khz = f64::from(snap.capture_rate) / 1000.0;
    let rate = if snap.capture_rate.is_multiple_of(1000) {
        format!("{khz:.0} kHz")
    } else {
        format!("{khz:.1} kHz")
    };
    format!("Capturing {} · {rate}", snap.capture_name.trim())
}

/// Truncate to at most `max` characters, ending with "…" when shortened.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

/// Tray tooltip (always below [`TOOLTIP_MAX_CHARS`] characters).
pub fn tray_tooltip(snap: &Snapshot, speaker: &str) -> String {
    let detail = match snap.state {
        State::Streaming => format!(
            "Streaming to {speaker} ({})",
            latency_label(snap.latency_ms)
        ),
        State::Connecting => format!("Connecting to {speaker}…"),
        State::Reconnecting => status_text(snap, true),
        State::Error if !snap.error.trim().is_empty() => {
            format!("Error: {}", snap.error.trim())
        }
        State::Error => "Error".to_string(),
        State::Disconnected => "Disconnected".to_string(),
    };
    // Tooltips are single-line in the notification area.
    let line = format!("{APP_NAME} - {detail}").replace(['\r', '\n'], " ");
    truncate_chars(&line, TOOLTIP_MAX_CHARS)
}

/// The source combo box: labels plus a parallel list of selections.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceChoices {
    pub labels: Vec<String>,
    pub selections: Vec<CaptureSelection>,
    /// Index of the configured selection.
    pub current: usize,
}

/// Build the source list: default output first, then loopbacks, then inputs. A
/// configured device that is currently missing stays selectable as "(not connected)".
pub fn source_choices(devices: &[DeviceInfo], current: &CaptureSelection) -> SourceChoices {
    let mut labels = vec![DEFAULT_SOURCE_LABEL.to_string()];
    let mut selections = vec![CaptureSelection::default_output()];
    let ordered = devices
        .iter()
        .filter(|d| d.kind == DeviceKind::Loopback)
        .chain(devices.iter().filter(|d| d.kind == DeviceKind::Input));
    for device in ordered {
        labels.push(match device.kind {
            DeviceKind::Loopback => device.name.clone(),
            DeviceKind::Input => format!("{} · input", device.name),
        });
        selections.push(CaptureSelection {
            name: device.display_name.clone(),
            id: Some(device.id.clone()),
        });
    }

    let current_index = if current.is_default() {
        Some(0)
    } else {
        let by_id = current
            .id
            .as_deref()
            .and_then(|id| selections.iter().position(|s| s.id.as_deref() == Some(id)));
        by_id.or_else(|| {
            selections
                .iter()
                .skip(1)
                .position(|s| s.name == current.name)
                .map(|i| i + 1)
        })
    };
    let current_index = current_index.unwrap_or_else(|| {
        labels.push(format!("{} (not connected)", current.name));
        selections.push(current.clone());
        selections.len() - 1
    });
    SourceChoices {
        labels,
        selections,
        current: current_index,
    }
}

/// Parse a user-typed IPv4 address (surrounding whitespace allowed).
pub fn parse_ipv4(text: &str) -> Option<Ipv4Addr> {
    text.trim().parse().ok()
}

/// Volume slider value rounded to a whole percent in 0..=100.
pub fn volume_percent(value: f32) -> f32 {
    if value.is_finite() {
        value.round().clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// Sends the volume on release, and at most every [`VolumeThrottle::INTERVAL`] while
/// the slider is being dragged.
#[derive(Debug, Clone, Default)]
pub struct VolumeThrottle {
    last_sent: Option<f32>,
    last_sent_at: Option<Instant>,
}

impl VolumeThrottle {
    /// Minimum spacing of sends while dragging.
    pub const INTERVAL: Duration = Duration::from_millis(100);

    /// Forget what was sent (e.g. after the control toggle changed).
    pub fn reset(&mut self, sent: Option<f32>) {
        self.last_sent = sent.map(volume_percent);
        self.last_sent_at = None;
    }

    /// Slider moved: the value to send now, if any.
    pub fn on_drag(&mut self, value: f32, now: Instant) -> Option<f32> {
        let value = volume_percent(value);
        let due = self
            .last_sent_at
            .is_none_or(|at| now.saturating_duration_since(at) >= Self::INTERVAL);
        if due && self.last_sent != Some(value) {
            self.mark(value, now)
        } else {
            None
        }
    }

    /// Slider released: always delivers the final value unless it was already sent.
    pub fn on_release(&mut self, value: f32, now: Instant) -> Option<f32> {
        let value = volume_percent(value);
        if self.last_sent != Some(value) {
            self.mark(value, now)
        } else {
            None
        }
    }

    fn mark(&mut self, value: f32, now: Instant) -> Option<f32> {
        self.last_sent = Some(value);
        self.last_sent_at = Some(now);
        Some(value)
    }
}

/// Level meter ballistics: perceptual (square-root) scale, instant attack and a
/// release of [`METER_RELEASE_PER_S`] full scales per second.
pub fn meter_level(previous: f32, peak: f32, dt: Duration) -> f32 {
    let target = if peak.is_finite() {
        peak.clamp(0.0, 1.0).sqrt()
    } else {
        0.0
    };
    if target >= previous {
        target
    } else {
        (previous - METER_RELEASE_PER_S * dt.as_secs_f32()).max(target)
    }
}

/// Meter release speed (full scale per second).
pub const METER_RELEASE_PER_S: f32 = 1.6;

/// What the Find popup shows for a scan state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanView {
    pub running: bool,
    pub message: String,
    pub results: Vec<ScanResult>,
}

/// Map the engine's scan progress to the popup contents.
pub fn scan_view(state: &ScanState) -> ScanView {
    match state {
        ScanState::Idle => ScanView {
            running: false,
            message: String::new(),
            results: Vec::new(),
        },
        ScanState::Running => ScanView {
            running: true,
            message: "Looking for AirPlay speakers…".to_string(),
            results: Vec::new(),
        },
        ScanState::Done(found) if found.is_empty() => ScanView {
            running: false,
            message: "No speakers found. Multicast may be blocked on this network: type the \
                      speaker's IP address instead."
                .to_string(),
            results: Vec::new(),
        },
        ScanState::Done(found) => ScanView {
            running: false,
            message: match found.len() {
                1 => "Found 1 speaker".to_string(),
                n => format!("Found {n} speakers"),
            },
            results: found.as_ref().clone(),
        },
        ScanState::Failed(error) => ScanView {
            running: false,
            message: format!("Scan failed: {error}"),
            results: Vec::new(),
        },
    }
}

/// How long a requested scan may take to show up as `Running` before the engine's
/// current scan state is shown again.
pub const SCAN_START_GRACE: Duration = Duration::from_millis(1500);

/// Decides when the Find popup must be refreshed from the engine's scan state.
///
/// The engine publishes a new snapshot whenever any statistic changes, but the popup's
/// rows must only be rebuilt when the scan state itself changes: replacing the rows'
/// model destroys the row being clicked between press and release, so the click is lost.
#[derive(Debug, Clone, Default)]
pub struct ScanTracker {
    /// Scan state currently shown (`None`: nothing shown yet, or rows were cleared).
    applied: Option<ScanState>,
    /// When the user asked for a scan that the engine has not reported as running yet.
    requested: Option<Instant>,
}

impl ScanTracker {
    /// The user asked for a scan at `now` and the popup was cleared to "searching": the
    /// previous result is not shown again until the engine reports the new scan (or
    /// [`SCAN_START_GRACE`] passes), and the next state is always applied.
    pub fn request(&mut self, now: Instant) {
        self.requested = Some(now);
        self.applied = None;
    }

    /// The popup contents to show for `scan`, or `None` when the popup is already up to
    /// date (or still waiting for a requested scan to start).
    pub fn update(&mut self, scan: &ScanState, now: Instant) -> Option<ScanView> {
        if let Some(requested) = self.requested {
            let started = matches!(scan, ScanState::Running);
            if started || now.saturating_duration_since(requested) > SCAN_START_GRACE {
                self.requested = None;
            } else {
                // Still the previous scan's state; keep showing "searching".
                return None;
            }
        }
        if self.applied.as_ref() == Some(scan) {
            return None;
        }
        self.applied = Some(scan.clone());
        Some(scan_view(scan))
    }
}

/// Detail line of a scan result row: "HomePod mini · 192.168.1.20".
pub fn scan_row_detail(result: &ScanResult) -> String {
    if result.model.trim().is_empty() {
        result.address.clone()
    } else {
        format!("{} · {}", result.model.trim(), result.address)
    }
}

/// Clock combo box index of a config value ("PTP" = 0, "NTP" = 1).
pub fn timing_index(value: &str) -> i32 {
    match TimingProtocol::parse(value) {
        Some(TimingProtocol::Ntp) => 1,
        _ => 0,
    }
}

/// Clock protocol for a combo box index.
pub fn timing_from_index(index: i32) -> TimingProtocol {
    if index == 1 {
        TimingProtocol::Ntp
    } else {
        TimingProtocol::Ptp
    }
}

/// Codec combo box index of a config value ("alac" = 0, "pcm" = 1).
pub fn codec_index(value: &str) -> i32 {
    match Codec::parse(value) {
        Some(Codec::Pcm) => 1,
        _ => 0,
    }
}

/// Codec for a combo box index.
pub fn codec_from_index(index: i32) -> Codec {
    if index == 1 { Codec::Pcm } else { Codec::Alac }
}

/// Config values of the renderer combo box, in order.
pub const RENDERERS: [&str; 2] = ["auto", "software"];
/// Config values of the log level combo box, in order.
pub const LOG_LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARNING", "ERROR"];

/// Index of `value` in `options` (case-insensitive), else `default`.
pub fn option_index(options: &[&str], value: &str, default: usize) -> i32 {
    let index = options
        .iter()
        .position(|option| option.eq_ignore_ascii_case(value.trim()))
        .unwrap_or(default);
    i32::try_from(index).unwrap_or(0)
}

/// Option for a combo box index, falling back to `options[default]`.
pub fn option_value<'a>(options: &[&'a str], index: i32, default: usize) -> &'a str {
    usize::try_from(index)
        .ok()
        .and_then(|i| options.get(i))
        .or_else(|| options.get(default))
        .copied()
        .unwrap_or_default()
}

/// Time between automatic update checks.
pub const UPDATE_PERIOD: Duration = Duration::from_secs(24 * 3600);
/// Delay of the check that runs at every start (keeps start-up quiet). Checking at
/// start (plan) also brings back an update that was found before a restart.
pub const UPDATE_STARTUP_DELAY: Duration = Duration::from_secs(5);

/// True when the last successful check (unix seconds) is a full period ago, or there
/// was none. A clock that went backwards does not count as due.
pub fn update_check_due(now_unix: u64, last_check: Option<u64>) -> bool {
    last_check.is_none_or(|last| {
        Duration::from_secs(now_unix.saturating_sub(last)) >= UPDATE_PERIOD && now_unix >= last
    })
}

/// Back-off after `failures` consecutive failed checks: 1, 5, then 30 minutes.
pub fn update_retry_delay(failures: usize) -> Duration {
    const STEPS: [u64; 3] = [60, 5 * 60, 30 * 60];
    Duration::from_secs(STEPS[failures.min(STEPS.len() - 1)])
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn format_ago(now_unix: u64, then_unix: u64) -> String {
    let seconds = now_unix.saturating_sub(then_unix);
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => match seconds / 86_400 {
            1 => "1 day ago".to_string(),
            days => format!("{days} days ago"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str, kind: DeviceKind) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            display_name: match kind {
                DeviceKind::Loopback => format!("{name} [Loopback]"),
                DeviceKind::Input => name.to_string(),
            },
            kind,
            sample_rate: 48_000,
            channels: 2,
            is_default: false,
        }
    }

    fn streaming() -> Snapshot {
        Snapshot {
            state: State::Streaming,
            latency_ms: 100,
            uptime_s: 125.4,
            retransmitted: 3,
            stalls: 1,
            ring_ms: 4.4,
            clock_ok: true,
            ..Snapshot::default()
        }
    }

    #[test]
    fn latency_is_clamped_to_supported_range() {
        assert_eq!(clamp_latency(10), 75);
        assert_eq!(clamp_latency(-5), 75);
        assert_eq!(clamp_latency(100), 100);
        assert_eq!(clamp_latency(9000), 5000);
    }

    #[test]
    fn presets_are_found() {
        assert_eq!(preset_index(100), Some(0));
        assert_eq!(preset_index(2000), Some(4));
        assert_eq!(preset_index(120), None);
    }

    #[test]
    fn latency_labels() {
        assert_eq!(latency_label(100), "100 ms");
        assert_eq!(latency_label(1000), "1 s");
        assert_eq!(latency_label(1500), "1.5 s");
        assert_eq!(latency_label(1234), "1234 ms");
        assert_eq!(tray_latency_label(100), "100 ms - Ultra low");
        assert_eq!(tray_latency_label(2000), "2 s - AirPlay default");
        assert_eq!(tray_latency_label(130), "130 ms");
    }

    #[test]
    fn hint_uses_default_render_time() {
        let hint = latency_hint(100, None);
        assert_eq!(hint.render_ms, 70);
        assert_eq!(hint.headroom_ms, 30);
        assert!(!hint.warning);
        assert_eq!(
            hint.text(),
            "Headroom 30 ms over the speaker's 70 ms render time"
        );
    }

    #[test]
    fn hint_warns_below_render_plus_margin() {
        // 69 ms reported by a HomePod mini: 74 ms is the limit.
        assert!(!latency_hint(74, Some(69)).warning);
        assert!(latency_hint(73, Some(69)).warning);
        // Default render time 70: 75 is fine, below is not.
        assert!(!latency_hint(75, None).warning);
        let low = latency_hint(60, None);
        assert!(low.warning);
        assert_eq!(low.headroom_ms, -10);
        assert!(low.text().contains("expect dropouts"));
    }

    #[test]
    fn status_texts() {
        let mut snap = streaming();
        assert_eq!(status_text(&snap, true), "Streaming · 100 ms");
        snap.state = State::Reconnecting;
        snap.retry_in_s = 2.2;
        assert_eq!(status_text(&snap, true), "Reconnecting in 3 s");
        snap.retry_in_s = 0.0;
        assert_eq!(status_text(&snap, true), "Reconnecting…");
        snap.state = State::Disconnected;
        assert_eq!(status_text(&snap, true), "Disconnected");
        assert_eq!(status_text(&snap, false), "Not set up");
        snap.state = State::Error;
        assert_eq!(status_text(&snap, true), "Error");
        assert_eq!(StatusKind::from(State::Reconnecting), StatusKind::Busy);
        assert_eq!(StatusKind::from(State::Streaming), StatusKind::Ok);
    }

    #[test]
    fn uptime_formatting() {
        assert_eq!(format_uptime(0.0), "0:00");
        assert_eq!(format_uptime(59.9), "0:59");
        assert_eq!(format_uptime(125.0), "2:05");
        assert_eq!(format_uptime(3725.0), "1:02:05");
        assert_eq!(format_uptime(-3.0), "0:00");
        assert_eq!(format_uptime(f64::NAN), "0:00");
    }

    #[test]
    fn stats_line_while_streaming() {
        let snap = streaming();
        assert_eq!(
            stats_line(&snap),
            "PTP locked · ALAC · 2:05 · 3 resent · 1 pause · ring 4 ms"
        );
        let mut snap = streaming();
        snap.timing = TimingProtocol::Ntp;
        snap.codec = Codec::Pcm;
        snap.clock_ok = false;
        snap.stalls = 2;
        assert_eq!(
            stats_line(&snap),
            "NTP waiting · PCM · 2:05 · 3 resent · 2 pauses · ring 4 ms"
        );
        snap.state = State::Connecting;
        assert_eq!(stats_line(&snap), "");
    }

    #[test]
    fn speaker_card_texts() {
        let mut snap = Snapshot::default();
        assert_eq!(speaker_title(&snap, "", ""), "No speaker selected");
        assert_eq!(speaker_detail(&snap, ""), "Choose your speaker in Settings");
        assert_eq!(speaker_title(&snap, "", "10.0.0.2"), "AirPlay speaker");
        assert_eq!(speaker_title(&snap, "Kitchen", "10.0.0.2"), "Kitchen");
        assert_eq!(speaker_detail(&snap, "10.0.0.2"), "10.0.0.2");
        snap.device_name = "Living Room".into();
        snap.device_model = "HomePod mini".into();
        snap.device_ip = "10.0.0.3".into();
        assert_eq!(speaker_title(&snap, "Kitchen", "10.0.0.2"), "Living Room");
        assert_eq!(speaker_detail(&snap, "10.0.0.2"), "HomePod mini · 10.0.0.3");
    }

    #[test]
    fn capture_detail_texts() {
        let mut snap = Snapshot::default();
        assert_eq!(capture_detail(&snap), "Not capturing");
        snap.capture_name = "Speakers".into();
        assert_eq!(capture_detail(&snap), "Capturing Speakers");
        snap.capture_rate = 48_000;
        assert_eq!(capture_detail(&snap), "Capturing Speakers · 48 kHz");
        snap.capture_rate = 44_100;
        assert_eq!(capture_detail(&snap), "Capturing Speakers · 44.1 kHz");
    }

    #[test]
    fn tooltip_is_short_and_single_line() {
        let mut snap = streaming();
        assert_eq!(
            tray_tooltip(&snap, "Living Room"),
            "WinPods - Streaming to Living Room (100 ms)"
        );
        snap.state = State::Error;
        snap.error = format!("connect failed:\n{}", "x".repeat(300));
        let tip = tray_tooltip(&snap, "Living Room");
        assert!(tip.chars().count() <= TOOLTIP_MAX_CHARS);
        assert!(tip.chars().count() < 100);
        assert!(tip.ends_with('…'));
        assert!(!tip.contains('\n'));
        snap.state = State::Disconnected;
        assert_eq!(tray_tooltip(&snap, "x"), "WinPods - Disconnected");
    }

    #[test]
    fn truncation_counts_characters() {
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abcd", 3), "ab…");
        assert_eq!(truncate_chars("ääää", 3), "ää…");
    }

    #[test]
    fn source_list_orders_default_loopbacks_inputs() {
        let devices = vec![
            device("in1", "Microphone", DeviceKind::Input),
            device("lb1", "Speakers", DeviceKind::Loopback),
            device("lb2", "CABLE Input", DeviceKind::Loopback),
        ];
        let choices = source_choices(&devices, &CaptureSelection::default_output());
        assert_eq!(
            choices.labels,
            vec![
                DEFAULT_SOURCE_LABEL,
                "Speakers",
                "CABLE Input",
                "Microphone · input"
            ]
        );
        assert_eq!(choices.current, 0);
        assert_eq!(choices.selections.len(), choices.labels.len());
        assert!(choices.selections[0].is_default());
        assert_eq!(choices.selections[1].name, "Speakers [Loopback]");
        assert_eq!(choices.selections[1].id.as_deref(), Some("lb1"));
    }

    #[test]
    fn source_selection_matches_id_then_name() {
        let devices = vec![
            device("lb1", "Speakers", DeviceKind::Loopback),
            device("lb2", "CABLE Input", DeviceKind::Loopback),
        ];
        let by_id = CaptureSelection {
            name: "renamed".into(),
            id: Some("lb2".into()),
        };
        assert_eq!(source_choices(&devices, &by_id).current, 2);
        let by_name = CaptureSelection {
            name: "Speakers [Loopback]".into(),
            id: None,
        };
        assert_eq!(source_choices(&devices, &by_name).current, 1);
    }

    #[test]
    fn missing_source_stays_selectable() {
        let devices = vec![device("lb1", "Speakers", DeviceKind::Loopback)];
        let gone = CaptureSelection {
            name: "USB DAC [Loopback]".into(),
            id: Some("usb".into()),
        };
        let choices = source_choices(&devices, &gone);
        assert_eq!(choices.current, 2);
        assert_eq!(choices.labels[2], "USB DAC [Loopback] (not connected)");
        assert_eq!(choices.selections[2], gone);
    }

    #[test]
    fn ipv4_parsing() {
        assert_eq!(
            parse_ipv4(" 192.168.1.20 "),
            Some(Ipv4Addr::new(192, 168, 1, 20))
        );
        assert_eq!(parse_ipv4("192.168.1"), None);
        assert_eq!(parse_ipv4("homepod.local"), None);
        assert_eq!(parse_ipv4(""), None);
    }

    #[test]
    fn volume_throttle_sends_every_100ms_and_on_release() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut throttle = VolumeThrottle::default();
        assert_eq!(throttle.on_drag(40.2, t0), Some(40.0));
        assert_eq!(throttle.on_drag(45.0, t0 + ms(50)), None);
        assert_eq!(throttle.on_drag(47.0, t0 + ms(99)), None);
        assert_eq!(throttle.on_drag(48.0, t0 + ms(100)), Some(48.0));
        // Same value later: nothing new to send.
        assert_eq!(throttle.on_drag(48.0, t0 + ms(300)), None);
        assert_eq!(throttle.on_drag(50.0, t0 + ms(320)), Some(50.0));
        // Release always delivers the final value...
        assert_eq!(throttle.on_release(51.0, t0 + ms(330)), Some(51.0));
        // ...but not twice.
        assert_eq!(throttle.on_release(51.0, t0 + ms(400)), None);
        throttle.reset(None);
        assert_eq!(throttle.on_release(51.0, t0 + ms(410)), Some(51.0));
    }

    #[test]
    fn volume_percent_rounds_and_clamps() {
        assert_eq!(volume_percent(40.4), 40.0);
        assert_eq!(volume_percent(40.6), 41.0);
        assert_eq!(volume_percent(-3.0), 0.0);
        assert_eq!(volume_percent(130.0), 100.0);
        assert_eq!(volume_percent(f32::NAN), 0.0);
    }

    #[test]
    fn meter_attacks_instantly_and_releases_slowly() {
        let dt = Duration::from_millis(33);
        assert_eq!(meter_level(0.0, 0.25, dt), 0.5);
        let after = meter_level(0.5, 0.0, dt);
        assert!(after < 0.5 && after > 0.4, "{after}");
        assert_eq!(meter_level(0.1, 0.0, Duration::from_secs(1)), 0.0);
        assert_eq!(meter_level(0.0, 4.0, dt), 1.0);
        assert_eq!(meter_level(0.3, f32::NAN, Duration::from_secs(1)), 0.0);
    }

    #[test]
    fn scan_views() {
        assert!(!scan_view(&ScanState::Idle).running);
        assert!(scan_view(&ScanState::Running).running);
        let empty = scan_view(&ScanState::Done(Arc::new(Vec::new())));
        assert!(empty.message.starts_with("No speakers found"));
        let found = vec![
            ScanResult {
                name: "Living Room".into(),
                address: "10.0.0.2".into(),
                model: "HomePod mini".into(),
            },
            ScanResult {
                name: "Kitchen".into(),
                address: "10.0.0.3".into(),
                model: String::new(),
            },
        ];
        let view = scan_view(&ScanState::Done(Arc::new(found.clone())));
        assert_eq!(view.message, "Found 2 speakers");
        assert_eq!(view.results, found);
        assert_eq!(scan_row_detail(&found[0]), "HomePod mini · 10.0.0.2");
        assert_eq!(scan_row_detail(&found[1]), "10.0.0.3");
        let failed = scan_view(&ScanState::Failed("no network".into()));
        assert_eq!(failed.message, "Scan failed: no network");
    }

    fn found(names: &[&str]) -> ScanState {
        ScanState::Done(Arc::new(
            names
                .iter()
                .enumerate()
                .map(|(i, name)| ScanResult {
                    name: (*name).to_string(),
                    address: format!("10.0.0.{}", i + 2),
                    model: String::new(),
                })
                .collect(),
        ))
    }

    #[test]
    fn scan_tracker_only_reports_changes() {
        let t0 = Instant::now();
        let mut tracker = ScanTracker::default();
        assert!(tracker.update(&ScanState::Idle, t0).is_some());
        // Republished snapshots with the same scan state leave the rows alone.
        assert!(tracker.update(&ScanState::Idle, t0).is_none());
        let done = found(&["Living Room", "Kitchen"]);
        let view = tracker.update(&done, t0).expect("new result");
        assert_eq!(view.results.len(), 2);
        assert!(tracker.update(&done, t0).is_none());
        // An equal list in a fresh Arc is not a change either.
        assert!(
            tracker
                .update(&found(&["Living Room", "Kitchen"]), t0)
                .is_none()
        );
        assert!(tracker.update(&found(&["Kitchen"]), t0).is_some());
    }

    #[test]
    fn scan_tracker_waits_for_a_requested_scan() {
        let t0 = Instant::now();
        let mut tracker = ScanTracker::default();
        let old = found(&["Living Room"]);
        assert!(tracker.update(&old, t0).is_some());
        tracker.request(t0);
        // The previous result is not shown again while the new scan starts.
        assert!(
            tracker
                .update(&old, t0 + Duration::from_millis(100))
                .is_none()
        );
        let running = tracker
            .update(&ScanState::Running, t0 + Duration::from_millis(200))
            .expect("scan started");
        assert!(running.running);
        assert!(
            tracker
                .update(&ScanState::Running, t0 + Duration::from_millis(300))
                .is_none()
        );
        assert!(tracker.update(&old, t0 + Duration::from_secs(3)).is_some());

        // An engine that never reports Running: after the grace period the (cleared)
        // popup shows its state again, even though it equals the one shown before.
        tracker.request(t0);
        assert!(
            tracker
                .update(&old, t0 + Duration::from_millis(10))
                .is_none()
        );
        let shown = tracker.update(&old, t0 + SCAN_START_GRACE + Duration::from_millis(1));
        assert_eq!(shown.map(|v| v.results.len()), Some(1));
    }

    #[test]
    fn engine_gives_up_only_without_any_address() {
        let error = Snapshot {
            state: State::Error,
            ..Snapshot::default()
        };
        assert!(engine_gave_up(&error, ""));
        assert!(engine_gave_up(&error, "  "));
        // The UI already knows an address the engine has not received yet.
        assert!(!engine_gave_up(&error, "10.0.0.2"));
        let with_ip = Snapshot {
            device_ip: "10.0.0.2".into(),
            ..error.clone()
        };
        assert!(!engine_gave_up(&with_ip, ""));
        let disconnected = Snapshot::default();
        assert!(!engine_gave_up(&disconnected, ""));
    }

    #[test]
    fn error_line_prefers_the_engine_then_the_address_hint() {
        let mut snap = Snapshot::default();
        assert_eq!(error_line(&snap, false), "");
        assert_eq!(error_line(&snap, true), NO_ADDRESS_HINT);
        snap.state = State::Reconnecting;
        snap.error = " connect timed out ".into();
        assert_eq!(error_line(&snap, true), "connect timed out");
        snap.state = State::Error;
        assert_eq!(error_line(&snap, false), "connect timed out");
        // A stale error text outside Error/Reconnecting is not shown.
        snap.state = State::Disconnected;
        assert_eq!(error_line(&snap, false), "");
        snap.state = State::Streaming;
        assert_eq!(error_line(&snap, true), NO_ADDRESS_HINT);
    }

    #[test]
    fn a_no_address_error_is_stale_once_an_address_is_set() {
        let error = Arc::new(Snapshot {
            state: State::Error,
            error: "No speaker address set: enter the HomePod's IP or use Find...".into(),
            ..Snapshot::default()
        });
        // Still no address: the error stays.
        assert_eq!(presented(&error, "", false).state, State::Error);
        // The user set one (the engine keeps Error until the next Connect): shown as
        // Disconnected, with no error line and a neutral tray icon.
        let shown = presented(&error, "192.168.1.20", false);
        assert_eq!(shown.state, State::Disconnected);
        assert_eq!(error_line(&shown, false), "");
        assert_eq!(tray_tooltip(&shown, "x"), "WinPods - Disconnected");
        assert_eq!(status_text(&shown, true), "Disconnected");
        // While a connection is wanted the engine's state is shown unchanged.
        assert_eq!(presented(&error, "192.168.1.20", true).state, State::Error);
        // Other states are passed through untouched (same Arc).
        let streaming = Arc::new(streaming());
        assert!(Arc::ptr_eq(
            &presented(&streaming, "192.168.1.20", false),
            &streaming
        ));
    }

    #[test]
    fn device_lists_are_compared_by_value() {
        let list = Arc::new(vec![device("lb1", "Speakers", DeviceKind::Loopback)]);
        assert!(devices_changed(None, &list));
        assert!(!devices_changed(Some(&list), &list));
        // A refresh publishes an equal list in a new Arc: not a change.
        let same = Arc::new(list.as_ref().clone());
        assert!(!devices_changed(Some(&list), &same));
        let plugged = Arc::new(vec![
            device("lb1", "Speakers", DeviceKind::Loopback),
            device("usb", "USB DAC", DeviceKind::Loopback),
        ]);
        assert!(devices_changed(Some(&list), &plugged));
        assert!(devices_changed(Some(&plugged), &list));
    }

    #[test]
    fn discovered_names_are_persisted_only_for_the_saved_address() {
        let snap = Snapshot {
            device_ip: "10.0.0.2".into(),
            discovered_device_name: Some("Living Room".into()),
            ..Snapshot::default()
        };
        assert_eq!(name_to_persist(&snap, "10.0.0.2", ""), Some("Living Room"));
        assert_eq!(
            name_to_persist(&snap, " 10.0.0.2 ", "Old"),
            Some("Living Room")
        );
        // Already saved.
        assert_eq!(name_to_persist(&snap, "10.0.0.2", "Living Room"), None);
        // The user switched to another address: the old receiver's name is stale.
        assert_eq!(name_to_persist(&snap, "10.0.0.9", ""), None);
        let no_engine_ip = Snapshot {
            device_ip: String::new(),
            ..snap.clone()
        };
        assert_eq!(name_to_persist(&no_engine_ip, "", ""), None);
        let blank = Snapshot {
            discovered_device_name: Some("  ".into()),
            ..snap.clone()
        };
        assert_eq!(name_to_persist(&blank, "10.0.0.2", ""), None);
        let none = Snapshot {
            discovered_device_name: None,
            ..snap
        };
        assert_eq!(name_to_persist(&none, "10.0.0.2", ""), None);
    }

    #[test]
    fn combo_box_index_mappings() {
        assert_eq!(timing_index("PTP"), 0);
        assert_eq!(timing_index("ntp"), 1);
        assert_eq!(timing_index("garbage"), 0);
        assert_eq!(timing_from_index(1), TimingProtocol::Ntp);
        assert_eq!(timing_from_index(0), TimingProtocol::Ptp);
        assert_eq!(timing_from_index(-1), TimingProtocol::Ptp);
        assert_eq!(codec_index("alac"), 0);
        assert_eq!(codec_index("PCM"), 1);
        assert_eq!(codec_from_index(1), Codec::Pcm);
        assert_eq!(codec_from_index(7), Codec::Alac);
        assert_eq!(option_index(&RENDERERS, "Software", 0), 1);
        assert_eq!(option_index(&RENDERERS, "weird", 0), 0);
        assert_eq!(option_index(&LOG_LEVELS, "warning", 1), 2);
        assert_eq!(option_index(&LOG_LEVELS, "", 1), 1);
        assert_eq!(option_value(&LOG_LEVELS, 0, 1), "DEBUG");
        assert_eq!(option_value(&LOG_LEVELS, 9, 1), "INFO");
        assert_eq!(option_value(&RENDERERS, -1, 0), "auto");
    }

    #[test]
    fn update_schedule() {
        let day = 24 * 3600;
        assert!(UPDATE_STARTUP_DELAY < Duration::from_secs(60));
        assert!(update_check_due(1_000_000, None));
        assert!(update_check_due(1_000_000, Some(1_000_000 - day)));
        assert!(update_check_due(1_000_000, Some(1_000_000 - day - 5)));
        assert!(!update_check_due(1_000_000, Some(1_000_000 - 3600)));
        // A clock that went backwards is not "due".
        assert!(!update_check_due(1_000_000, Some(2_000_000)));
        assert_eq!(update_retry_delay(0), Duration::from_secs(60));
        assert_eq!(update_retry_delay(1), Duration::from_secs(300));
        assert_eq!(update_retry_delay(2), Duration::from_secs(1800));
        assert_eq!(update_retry_delay(50), Duration::from_secs(1800));
    }

    #[test]
    fn relative_times() {
        assert_eq!(format_ago(100, 90), "just now");
        assert_eq!(format_ago(1000, 400), "10 min ago");
        assert_eq!(format_ago(10_000, 2_000), "2 h ago");
        assert_eq!(format_ago(200_000, 100_000), "1 day ago");
        assert_eq!(format_ago(1_000_000, 100_000), "10 days ago");
        assert_eq!(format_ago(5, 10), "just now");
    }

    #[test]
    fn clock_and_codec_labels() {
        assert_eq!(clock_label(TimingProtocol::Ptp, true), "PTP locked");
        assert_eq!(clock_label(TimingProtocol::Ptp, false), "PTP waiting");
        assert_eq!(clock_label(TimingProtocol::Ntp, true), "NTP ok");
        assert_eq!(codec_label(Codec::Alac), "ALAC");
        assert_eq!(codec_label(Codec::Pcm), "PCM");
    }
}
