//! winpods-cli.exe: command-line diagnostics. Port of `legacy/python/winpods/cli.py`.
//!
//! Command-line overrides apply to this run only: the CLI never saves its in-memory
//! config (it only persists the discovered name of the saved speaker; the device id
//! is persisted by `Config::load`).
//!
//! Ctrl+C, closing the console window, log-off and shutdown all stop the stream with
//! a TEARDOWN: the console handler holds the close/log-off/shutdown events (which
//! end the process once the handler returns) until the engine has shut down.

use std::future::Future;
use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use winpods_airplay::ptp::PtpService;
use winpods_airplay::runtime::ControlSocket;
use winpods_airplay::session::{Session, SessionConfig, TimingSetup};
use winpods_airplay::{Codec, MAX_LATENCY_MS, MIN_LATENCY_MS, TimingProtocol, mdns};
use winpods_app::Config;
use winpods_app::logging;
use winpods_audio::{DeviceKind, DeviceWatch};
use winpods_engine::connect::{LOOKUP_TIMEOUT, route_local_ip, start_ptp};
use winpods_engine::supervisor::capture_selection;
use winpods_engine::{
    CaptureSelection, Command, EngineApi, EngineHandle, EngineSettings, Snapshot, State,
};

/// `println!` that ignores a console that is going away: closing the window must
/// not panic the thread that still has to shut the stream down.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// Stream Windows audio to a HomePod / AirPlay 2 speaker (diagnostics CLI).
#[derive(Debug, Parser)]
#[command(name = "winpods-cli", version, about)]
pub struct Args {
    /// Receiver IP address (default: the one saved by the app).
    #[arg(long)]
    pub ip: Option<String>,
    /// Receiver latency in milliseconds (75-5000), e.g. 100 for ultra low latency.
    #[arg(long, value_name = "MS")]
    pub latency: Option<u32>,
    /// Capture device display name, or "default" for the Windows default output.
    #[arg(long)]
    pub device: Option<String>,
    /// Set the receiver volume (0-100) on connect.
    #[arg(long, value_name = "PCT")]
    pub volume: Option<f32>,
    /// Clock protocol for AirPlay 2 receivers.
    #[arg(long, value_parser = ["PTP", "NTP", "ptp", "ntp"])]
    pub timing: Option<String>,
    /// AirPlay 2 audio format.
    #[arg(long, value_parser = ["alac", "pcm"])]
    pub codec: Option<String>,
    /// DEBUG, INFO, WARNING (default: the saved log level).
    #[arg(long)]
    pub log_level: Option<String>,
    /// Stop after this many seconds.
    #[arg(long, value_name = "SEC")]
    pub duration: Option<f64>,
    /// Stream until Ctrl+C (the default run mode; kept for v0.1 compatibility).
    #[arg(long)]
    pub headless: bool,
    /// Stream a quiet beep each second instead of Windows audio (plays on the receiver only).
    #[arg(long)]
    pub test_tone: bool,
    /// List capture devices and exit.
    #[arg(long)]
    pub list_devices: bool,
    /// Scan the network for AirPlay receivers and exit.
    #[arg(long)]
    pub scan: bool,
    /// Pair with the receiver, run SETUP, print what was negotiated and tear down.
    #[arg(long)]
    pub pair_test: bool,
    /// Diagnostic: hold back every Nth packet and report retransmit requests.
    #[arg(long, value_name = "N", hide = true)]
    pub drop_every: Option<u32>,
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// Set once the engine (or the pair-test session) has shut down, TEARDOWN included.
static CLEANED_UP: AtomicBool = AtomicBool::new(false);
/// How long the console handler holds a close/log-off/shutdown event for cleanup.
/// Windows ends the process about 5 s after such an event.
const CLOSE_GRACE: Duration = Duration::from_millis(4500);
/// How often a pair-test step checks for Ctrl+C.
const INTERRUPT_POLL: Duration = Duration::from_millis(50);

/// Marks cleanup as finished when dropped. Declare it before the engine/session so
/// it is dropped after them.
struct CleanupSignal;

impl Drop for CleanupSignal {
    fn drop(&mut self) {
        CLEANED_UP.store(true, Ordering::SeqCst);
    }
}

fn main() -> ExitCode {
    winpods_rt::clock::init();
    let args = Args::parse();
    let config = Config::load();
    let level = args
        .log_level
        .clone()
        .unwrap_or_else(|| config.log_level.clone());
    let _log = logging::init(&level, true);
    logging::install_panic_hook(false);

    let result = if args.list_devices {
        list_devices()
    } else if args.scan {
        scan()
    } else if args.pair_test {
        pair_test(&args, &config)
    } else {
        run(&args, &config)
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn settings(args: &Args, config: &Config) -> EngineSettings {
    let capture = match &args.device {
        Some(name) => capture_selection(name, None),
        None => capture_selection(&config.capture_device, config.capture_device_id.clone()),
    };
    EngineSettings {
        device_ip: args.ip.clone().unwrap_or_else(|| config.device_ip.clone()),
        latency_ms: args
            .latency
            .unwrap_or(config.latency_ms)
            .clamp(MIN_LATENCY_MS, MAX_LATENCY_MS),
        capture,
        volume: args.volume.or(config.volume),
        timing: args
            .timing
            .as_deref()
            .and_then(TimingProtocol::parse)
            .or_else(|| TimingProtocol::parse(&config.timing_protocol))
            .unwrap_or_default(),
        codec: args
            .codec
            .as_deref()
            .and_then(Codec::parse)
            .or_else(|| Codec::parse(&config.codec))
            .unwrap_or_default(),
        sender_name: config.sender_name.clone(),
        device_id: config
            .device_id
            .clone()
            .unwrap_or_else(|| "02:00:00:00:00:01".to_string()),
        test_tone: args.test_tone,
        drop_every: args.drop_every.filter(|n| *n > 1),
        // Hardware tests must fail loudly instead of falling back to a dynamic port.
        require_standard_ptp_ports: true,
    }
}

// --------------------------------------------------------------------- commands

fn list_devices() -> Result<ExitCode> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let watch = DeviceWatch::start(tx).context("starting the device watcher")?;
    let default = watch.default_render()?;
    println!(
        "  \"default\"  -> Windows default output (loopback): {}",
        default
            .map(|d| d.display_name)
            .unwrap_or_else(|| "none".into())
    );
    for dev in watch.list()? {
        let kind = match dev.kind {
            DeviceKind::Loopback => "loopback",
            DeviceKind::Input => "input   ",
        };
        println!(
            "  {kind}  {:>6} Hz  {} ch  \"{}\"",
            dev.sample_rate, dev.channels, dev.display_name
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn scan() -> Result<ExitCode> {
    println!("Scanning for AirPlay receivers (5 s)...");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let found = runtime.block_on(winpods_engine::scan::scan(Duration::from_secs(5)))?;
    if found.is_empty() {
        println!("  none found (multicast may be blocked; try --ip directly)");
    }
    for dev in found {
        println!("  {:<16} {}  [{}]", dev.address, dev.name, dev.model);
    }
    Ok(ExitCode::SUCCESS)
}

fn pair_test(args: &Args, config: &Config) -> Result<ExitCode> {
    let settings = settings(args, config);
    let Some(address) = parse_ip(&settings.device_ip) else {
        return Ok(no_address());
    };
    install_console_handler();
    // Dropped last: the console handler waits for it (after the TEARDOWN below).
    let _cleanup = CleanupSignal;
    // Preflight before the receiver is contacted. HomePods reject the dynamic-port
    // fallback (ClockPorts), so busy PTP ports are an error, as in the run mode.
    let route_ip =
        route_local_ip(address).with_context(|| format!("no network route to {address}"))?;
    let mut ptp = Some(start_ptp(route_ip, true).map_err(|err| anyhow!(err))?);
    let persist_name = name_belongs_to_saved_speaker(args.ip.as_deref(), &config.device_ip);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let info = interruptible(mdns::lookup(address, LOOKUP_TIMEOUT))
            .await??
            .ok_or_else(|| anyhow!("no AirPlay receiver answered at {address}"))?;
        say!(
            "receiver: {} [{}] features={:#x} airplay2={} transient={}",
            info.name,
            info.model,
            info.features,
            info.is_airplay2(),
            info.supports_transient_pairing()
        );
        let session_config = SessionConfig {
            sender_name: settings.sender_name.clone(),
            device_id: settings.device_id.clone(),
            latency_ms: settings.latency_ms,
            codec: settings.codec,
            volume_pct: None,
            title: "Windows audio".into(),
        };
        let mut session = interruptible(Session::connect(&info, session_config)).await??;
        say!(
            "paired (retries: {}), local address {}",
            session.pairing_retries(),
            session.local_ip()
        );
        // From here on the session is always torn down, Ctrl+C included.
        let result = setup_test(&mut session, route_ip, &mut ptp).await;
        session.teardown().await;
        if persist_name && let Err(err) = Config::persist_identity(&settings.device_id, &info.name)
        {
            tracing::debug!("could not persist identity: {err}");
        }
        result.map(|()| ExitCode::SUCCESS)
    })
}

/// Bind the sockets and run SETUP on a paired session (pair-test).
async fn setup_test(
    session: &mut Session,
    route_ip: Ipv4Addr,
    ptp: &mut Option<Arc<PtpService>>,
) -> Result<()> {
    let local_ip = session.local_ip();
    if local_ip != route_ip {
        say!("note: the receiver connection uses {local_ip}, not {route_ip}; moving PTP there");
        *ptp = None; // release 319/320 before binding them again
        *ptp = Some(start_ptp(local_ip, true).map_err(|err| anyhow!(err))?);
    }
    let ptp = ptp.as_ref().context("PTP follower missing")?;
    let control = ControlSocket::bind(local_ip)?;
    let result =
        interruptible(session.setup(TimingSetup::Ptp(ptp.clone()), control.port())).await?;
    match &result {
        Ok(params) => {
            say!(
                "SETUP ok: data port {}, control port {}, render {:?} ms, clock {:?}",
                params.data_port,
                params.control_port,
                params.render_ms,
                params.clock_id.map(|c| format!("{c:016x}"))
            );
            let clock = ptp.clock();
            say!(
                "PTP locked={} offset={:.3} ms spread={:.2} ms syncs={}",
                clock.locked(),
                clock.offset_ns().unwrap_or(0.0) / 1e6,
                clock.delay_spread_ns() as f64 / 1e6,
                clock.syncs()
            );
        }
        Err(err) => say!("SETUP failed: {err}"),
    }
    result.map(|_| ()).map_err(anyhow::Error::from)
}

/// Run `step` unless Ctrl+C (or a console close) comes first.
async fn interruptible<F: Future>(step: F) -> Result<F::Output> {
    tokio::select! {
        biased;
        () = async {
            while !INTERRUPTED.load(Ordering::SeqCst) {
                tokio::time::sleep(INTERRUPT_POLL).await;
            }
        } => Err(anyhow!("interrupted")),
        out = step => Ok(out),
    }
}

/// True when the speaker the engine connects to is the saved one, so its
/// discovered name may be saved as the config's speaker name (`--ip` pointing
/// somewhere else must not rename the saved speaker).
fn name_belongs_to_saved_speaker(ip_override: Option<&str>, saved_ip: &str) -> bool {
    match ip_override {
        None => true,
        Some(ip) => matches!(
            (parse_ip(ip), parse_ip(saved_ip)),
            (Some(given), Some(saved)) if given == saved
        ),
    }
}

fn run(args: &Args, config: &Config) -> Result<ExitCode> {
    let settings = settings(args, config);
    if parse_ip(&settings.device_ip).is_none() {
        return Ok(no_address());
    }
    install_console_handler();
    // Dropped after `engine` (declared later): the console handler waits for it.
    let _cleanup = CleanupSignal;
    let source = if settings.test_tone {
        "a test tone"
    } else {
        "Windows audio"
    };
    say!(
        "Streaming {source} to {} with {} ms latency. Ctrl+C to stop.",
        settings.device_ip,
        settings.latency_ms
    );
    let device_id = settings.device_id.clone();
    let drop_test = settings.drop_every.is_some();
    let persist_name = name_belongs_to_saved_speaker(args.ip.as_deref(), &config.device_ip);
    let engine = EngineHandle::start(settings)?;
    engine.send(Command::Connect);

    let started = Instant::now();
    let mut last_state: Option<State> = None;
    let mut last_error = String::new();
    let mut persisted_name = false;
    let mut next_stats = Instant::now() + Duration::from_secs(5);
    let mut next_feedback = Instant::now() + Duration::from_secs(15);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let snap = engine.snapshot();
        if last_state != Some(snap.state) || (!snap.error.is_empty() && snap.error != last_error) {
            last_state = Some(snap.state);
            last_error = snap.error.clone();
            let detail = if snap.error.is_empty() {
                String::new()
            } else {
                format!(" - {}", snap.error)
            };
            say!("[{}]{detail}", snap.state.label());
        }
        if persist_name
            && !persisted_name
            && let Some(name) = &snap.discovered_device_name
        {
            persisted_name = true;
            if let Err(err) = Config::persist_identity(&device_id, name) {
                tracing::debug!("could not persist identity: {err}");
            }
        }
        let now = Instant::now();
        if snap.state == State::Streaming && now >= next_stats {
            next_stats = now + Duration::from_secs(5);
            say!("{}", stats_line(&snap, engine.peak(), drop_test));
        }
        if now >= next_feedback {
            next_feedback = now + Duration::from_secs(15);
            if snap.state == State::Streaming
                && let Some(feedback) = &snap.feedback
            {
                say!("  receiver feedback: {feedback}");
            }
        }
        if snap.state == State::Error {
            engine.shutdown();
            return Ok(ExitCode::FAILURE);
        }
        if INTERRUPTED.load(Ordering::SeqCst)
            || args
                .duration
                .is_some_and(|d| started.elapsed().as_secs_f64() >= d)
        {
            break;
        }
    }
    say!("Stopping...");
    let final_snap = engine.snapshot();
    engine.shutdown();
    if drop_test {
        say!(
            "drop test: withheld={} retransmit_requests={} retransmitted={}",
            final_snap.withheld,
            final_snap.retransmit_requests,
            final_snap.retransmitted
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The 5-second stats line, field-compatible with the Python v0.1 headless output.
fn stats_line(snap: &Snapshot, peak: f32, drop_test: bool) -> String {
    let clock = match snap.timing {
        TimingProtocol::Ptp => format!(
            "PTP {} syncs={} spread={:.2}ms",
            if snap.clock_ok { "locked" } else { "LOST" },
            snap.ptp_syncs,
            snap.ptp_spread_ms
        ),
        TimingProtocol::Ntp => format!("NTP requests={}", snap.timing_requests),
    };
    let mut line = format!(
        "  {:>6.0}s {} pkts={} silence={} stalls={} trimmed={} retx={} anchors={} | {} | ring={:.0}ms peak={:.2}",
        snap.uptime_s,
        snap.codec.as_str().to_ascii_uppercase(),
        snap.packets,
        snap.silence_packets,
        snap.stalls,
        snap.frames_trimmed,
        snap.retransmitted,
        snap.anchors,
        clock,
        snap.ring_ms,
        peak
    );
    if drop_test {
        line.push_str(&format!(
            " withheld={} retx_req={}",
            snap.withheld, snap.retransmit_requests
        ));
    }
    if snap.send_errors > 0 {
        line.push_str(&format!(" send_errors={}", snap.send_errors));
    }
    line
}

fn parse_ip(value: &str) -> Option<Ipv4Addr> {
    value.trim().parse().ok()
}

fn no_address() -> ExitCode {
    eprintln!("No speaker address: pass --ip, or run --scan to find one.");
    ExitCode::from(2)
}

/// Close, log-off and shutdown end the process as soon as the handler returns.
fn ends_process(ctrl_type: u32) -> bool {
    use windows::Win32::System::Console::{
        CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    matches!(
        ctrl_type,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    )
}

/// Wait until `flag` is set or `limit` has passed; true when it was set.
fn wait_for_flag(flag: &AtomicBool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}

fn install_console_handler() {
    use windows::Win32::Foundation::TRUE;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    use windows::core::BOOL;

    /// Runs on a thread Windows creates for the event.
    unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
        INTERRUPTED.store(true, Ordering::SeqCst);
        if ends_process(ctrl_type) {
            // Hold the event until the main thread has sent TEARDOWN and stopped
            // the engine, within the time Windows allows.
            wait_for_flag(&CLEANED_UP, CLOSE_GRACE);
        }
        TRUE
    }
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
}

#[allow(dead_code)]
fn _assert_selection_default() -> CaptureSelection {
    CaptureSelection::default_output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_name_is_saved_only_for_the_saved_address() {
        assert!(name_belongs_to_saved_speaker(None, "192.168.1.57"));
        assert!(name_belongs_to_saved_speaker(None, ""));
        assert!(name_belongs_to_saved_speaker(
            Some(" 192.168.1.57 "),
            "192.168.1.57"
        ));
        assert!(!name_belongs_to_saved_speaker(
            Some("192.168.1.80"),
            "192.168.1.57"
        ));
        // Nothing saved yet, or garbage: do not invent a name for the config.
        assert!(!name_belongs_to_saved_speaker(Some("192.168.1.80"), ""));
        assert!(!name_belongs_to_saved_speaker(Some("kitchen"), "kitchen"));
    }

    #[test]
    fn close_logoff_and_shutdown_are_held_for_cleanup() {
        use windows::Win32::System::Console::{
            CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT,
            CTRL_SHUTDOWN_EVENT,
        };
        assert!(ends_process(CTRL_CLOSE_EVENT));
        assert!(ends_process(CTRL_LOGOFF_EVENT));
        assert!(ends_process(CTRL_SHUTDOWN_EVENT));
        assert!(!ends_process(CTRL_C_EVENT));
        assert!(!ends_process(CTRL_BREAK_EVENT));
    }

    #[test]
    fn handler_wait_ends_when_cleanup_finishes_or_the_grace_runs_out() {
        static DONE: AtomicBool = AtomicBool::new(false);
        let setter = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            DONE.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert!(wait_for_flag(&DONE, Duration::from_secs(5)));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(90), "{waited:?}");
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        setter.join().unwrap();

        let never = AtomicBool::new(false);
        let started = Instant::now();
        assert!(!wait_for_flag(&never, Duration::from_millis(150)));
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(
            CLOSE_GRACE < Duration::from_secs(5),
            "inside Windows' limit"
        );
    }

    #[test]
    fn cleanup_signal_fires_on_drop() {
        // The only test touching CLEANED_UP.
        CLEANED_UP.store(false, Ordering::SeqCst);
        {
            let _cleanup = CleanupSignal;
            assert!(!CLEANED_UP.load(Ordering::SeqCst));
        }
        assert!(CLEANED_UP.load(Ordering::SeqCst));
    }

    #[test]
    fn interrupted_steps_stop_and_others_finish() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            // The only test touching INTERRUPTED.
            INTERRUPTED.store(false, Ordering::SeqCst);
            assert_eq!(interruptible(async { 5 }).await.unwrap(), 5);
            INTERRUPTED.store(true, Ordering::SeqCst);
            let started = Instant::now();
            let err = interruptible(std::future::pending::<()>())
                .await
                .unwrap_err();
            assert!(err.to_string().contains("interrupted"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(1));
            INTERRUPTED.store(false, Ordering::SeqCst);
        });
    }
}
