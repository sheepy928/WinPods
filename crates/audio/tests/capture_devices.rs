//! Real-device capture checks, ignored by default (CI never runs them). Run with
//!
//! ```text
//! cargo test -p winpods-audio --test capture_devices -- --ignored --nocapture
//! ```
//!
//! They only capture (loopback of a playback device, or a recording device) and
//! never play anything. `WINPODS_TEST_DEVICE` picks the endpoint of
//! `named_device_delivers_44k1` (default: the VB-Audio cable loopback) and
//! `WINPODS_TEST_SECONDS` the duration (default 3).

use std::time::{Duration, Instant};

use winpods_audio::{CaptureExit, CaptureHandle, CaptureInfo, CaptureSource};
use winpods_rt::{Wait, mono_ns, ring, wait_any};

const DEFAULT_DEVICE: &str = "CABLE Input (VB-Audio Virtual Cable) [Loopback]";

/// What the consumer side saw.
#[derive(Debug, Default)]
struct Report {
    frames: u64,
    pushes: u64,
    first_write_ns: u64,
    last_write_ns: u64,
    max_gap_ns: u64,
    max_burst: usize,
    max_peak: f32,
}

impl Report {
    /// Frames per second between the first and the last write.
    fn rate(&self, frames_after_first: u64) -> Option<f64> {
        let span = self.last_write_ns.checked_sub(self.first_write_ns)?;
        (span > 0).then(|| frames_after_first as f64 * 1e9 / span as f64)
    }
}

fn seconds() -> f64 {
    std::env::var("WINPODS_TEST_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0)
}

/// Capture `source` for `seconds`, draining the ring like the sender would.
fn measure(source: &CaptureSource, seconds: f64) -> (CaptureInfo, Report, u64, u64, CaptureExit) {
    let (producer, mut consumer) = ring::ring(ring::DEFAULT_CAPACITY).expect("ring");
    let handle = match CaptureHandle::start(source, producer) {
        Ok(handle) => handle,
        Err((error, _)) => panic!("capture did not start: {error}"),
    };
    let info = handle.info().clone();
    let event = consumer.shared().event_handle();
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut report = Report::default();
    let mut first_chunk = 0u64;
    let mut previous_write = 0u64;
    while Instant::now() < deadline {
        if let Ok(Wait::Signaled(_)) = wait_any(&[event], Some(Duration::from_millis(50))) {
            let available = consumer.available();
            let written_at = consumer.shared().last_write_ns();
            assert!(written_at <= mono_ns(), "write stamped in the future");
            assert!(written_at >= previous_write, "write stamps went backwards");
            if available == 0 {
                continue;
            }
            if report.pushes == 0 {
                report.first_write_ns = written_at;
                first_chunk = available as u64;
            } else {
                report.max_gap_ns = report.max_gap_ns.max(written_at - previous_write);
            }
            previous_write = written_at;
            report.last_write_ns = written_at;
            report.pushes += 1;
            report.frames += available as u64;
            report.max_burst = report.max_burst.max(available);
            report.max_peak = report.max_peak.max(consumer.shared().peak());
            consumer.discard(available);
        }
    }
    assert!(
        handle.exit().is_none(),
        "capture ended early: {:?}",
        handle.exit()
    );
    let discontinuities = handle.discontinuities();
    let (producer, exit) = handle.stop();
    assert_eq!(producer.shared().dropped(), 0, "ring overflowed");
    (info, report, first_chunk, discontinuities, exit)
}

fn print_and_check(label: &str, source: &CaptureSource) {
    let secs = seconds();
    let (info, report, first_chunk, discontinuities, exit) = measure(source, secs);
    eprintln!("{label}: {info:?}");
    eprintln!(
        "  {secs} s: {} frames in {} pushes, largest push {} frames, max gap {:.1} ms, \
         peak {:.3}, discontinuities {discontinuities}, exit {exit:?}",
        report.frames,
        report.pushes,
        report.max_burst,
        report.max_gap_ns as f64 / 1e6,
        report.max_peak,
    );
    assert_eq!(exit, CaptureExit::Stopped);
    eprintln!(
        "  overall {:.1} frames/s of wall time",
        report.frames as f64 / secs
    );
    match report.rate(report.frames - first_chunk.min(report.frames)) {
        Some(rate) => {
            eprintln!("  measured rate {rate:.1} frames/s");
            assert!((rate - 44_100.0).abs() < 44_100.0 * 0.02, "rate {rate}");
        }
        None => eprintln!(
            "  no audio packets: the endpoint is idle (WASAPI loopback delivers nothing \
             while no stream renders to it)"
        ),
    }
}

#[test]
#[ignore = "captures from a real audio endpoint"]
fn named_device_delivers_44k1() {
    let name = std::env::var("WINPODS_TEST_DEVICE").unwrap_or_else(|_| DEFAULT_DEVICE.into());
    let source = CaptureSource::Device { id: None, name };
    print_and_check("named device", &source);
}

#[test]
#[ignore = "captures from a real audio endpoint"]
fn default_render_loopback_delivers_44k1() {
    print_and_check("default render loopback", &CaptureSource::DefaultRender);
}
