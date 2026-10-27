//! winpods-cli.exe: command-line diagnostics (owned by the lead). Port of
//! `legacy/python/winpods/cli.py`; the run loop is wired in the integration step.

use clap::Parser;

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
    /// DEBUG, INFO, WARNING (default INFO).
    #[arg(long)]
    pub log_level: Option<String>,
    /// Stop after this many seconds.
    #[arg(long, value_name = "SEC")]
    pub duration: Option<f64>,
    /// Stream until Ctrl+C (the only run mode of the CLI; kept for v0.1 compatibility).
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
    /// Pair with the receiver, run SETUP, print the responses and tear down.
    #[arg(long)]
    pub pair_test: bool,
    /// Diagnostic: hold back every Nth packet and report retransmit requests.
    #[arg(long, value_name = "N", hide = true)]
    pub drop_every: Option<u32>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _ = args;
    Ok(())
}
