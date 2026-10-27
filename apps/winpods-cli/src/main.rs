//! winpods-cli.exe: command-line diagnostics. Port of `legacy/python/winpods/cli.py`.
//!
//! Command-line overrides apply to this run only: the CLI never saves its in-memory
//! config (it only persists the generated device id and the discovered speaker name).

use std::net::Ipv4Addr;
use std::process::ExitCode;
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
use winpods_engine::supervisor::capture_selection;
use winpods_engine::{
    CaptureSelection, Command, EngineApi, EngineHandle, EngineSettings, Snapshot, State,
};

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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let info = mdns::lookup(address, Duration::from_secs(5))
            .await?
            .ok_or_else(|| anyhow!("no AirPlay receiver answered at {address}"))?;
        println!(
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
        let mut session = Session::connect(&info, session_config).await?;
        println!(
            "paired (retries: {}), local address {}",
            session.pairing_retries(),
            session.local_ip()
        );
        let control = ControlSocket::bind(session.local_ip())?;
        let ptp = PtpService::start(session.local_ip())?;
        if !ptp.standard_ports() {
            println!("warning: UDP 319/320 are in use; using port {}", ptp.port());
        }
        let result = session
            .setup(TimingSetup::Ptp(ptp.clone()), control.port())
            .await;
        match &result {
            Ok(params) => {
                println!(
                    "SETUP ok: data port {}, control port {}, render {:?} ms, clock {:?}",
                    params.data_port,
                    params.control_port,
                    params.render_ms,
                    params.clock_id.map(|c| format!("{c:016x}"))
                );
                let clock = ptp.clock();
                println!(
                    "PTP locked={} offset={:.3} ms spread={:.2} ms syncs={}",
                    clock.locked(),
                    clock.offset_ns().unwrap_or(0.0) / 1e6,
                    clock.delay_spread_ns() as f64 / 1e6,
                    clock.syncs()
                );
            }
            Err(err) => println!("SETUP failed: {err}"),
        }
        session.teardown().await;
        if let Err(err) = Config::persist_identity(&settings.device_id, &info.name) {
            tracing::debug!("could not persist identity: {err}");
        }
        result
            .map(|_| ExitCode::SUCCESS)
            .map_err(anyhow::Error::from)
    })
}

fn run(args: &Args, config: &Config) -> Result<ExitCode> {
    let settings = settings(args, config);
    if parse_ip(&settings.device_ip).is_none() {
        return Ok(no_address());
    }
    install_ctrl_c();
    let source = if settings.test_tone {
        "a test tone"
    } else {
        "Windows audio"
    };
    println!(
        "Streaming {source} to {} with {} ms latency. Ctrl+C to stop.",
        settings.device_ip, settings.latency_ms
    );
    let device_id = settings.device_id.clone();
    let drop_test = settings.drop_every.is_some();
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
            println!("[{}]{detail}", snap.state.label());
        }
        if !persisted_name && let Some(name) = &snap.discovered_device_name {
            persisted_name = true;
            if let Err(err) = Config::persist_identity(&device_id, name) {
                tracing::debug!("could not persist identity: {err}");
            }
        }
        let now = Instant::now();
        if snap.state == State::Streaming && now >= next_stats {
            next_stats = now + Duration::from_secs(5);
            println!("{}", stats_line(&snap, engine.peak(), drop_test));
        }
        if now >= next_feedback {
            next_feedback = now + Duration::from_secs(15);
            if snap.state == State::Streaming
                && let Some(feedback) = &snap.feedback
            {
                println!("  receiver feedback: {feedback}");
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
    println!("Stopping...");
    let final_snap = engine.snapshot();
    engine.shutdown();
    if drop_test {
        println!(
            "drop test: withheld={} retransmit_requests={} retransmitted={}",
            final_snap.withheld, final_snap.retransmit_requests, final_snap.retransmitted
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

fn install_ctrl_c() {
    use windows::Win32::Foundation::TRUE;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    use windows::core::BOOL;

    unsafe extern "system" fn handler(_ctrl_type: u32) -> BOOL {
        INTERRUPTED.store(true, Ordering::SeqCst);
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
