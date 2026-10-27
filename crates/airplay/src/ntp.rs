//! NTP-style timing for AirPlay 2 receivers that do not use PTP (workstream:
//! real-time). CONTRACT: public items used by `session`, `runtime` and the engine.
//!
//! * [`NtpClock`]: NTP 32.32 time derived from the system clock read once per session
//!   and advanced with `rt::mono_ns()`, so wall-clock jumps never disturb a stream.
//! * [`TimingResponder`]: UDP socket on the local IP answering timing requests
//!   (`byte1 & 0x7F == 0x52`, 32-byte packets) with 0xD3 replies exactly like
//!   `LiveTimingServer` in `legacy/python/winpods/raop_live.py` (reference time =
//!   the request's send time; receive and send time = now). Dedicated thread;
//!   tolerates WSAECONNRESET; stops when dropped.

use std::net::Ipv4Addr;
use std::sync::Arc;

/// NTP time anchored to the monotonic clock.
pub struct NtpClock {
    _private: (),
}

impl NtpClock {
    /// Capture the wall clock now and anchor it to `rt::mono_ns()`.
    pub fn new() -> NtpClock {
        todo!("ntp: NtpClock::new")
    }
    /// NTP timestamp (seconds since 1900 << 32 | fraction) for an `rt::mono_ns()` value.
    pub fn ntp_at(&self, mono_ns: u64) -> u64 {
        let _ = mono_ns;
        todo!("ntp: ntp_at")
    }
}

impl Default for NtpClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Answers the receiver's NTP timing requests.
pub struct TimingResponder {
    _private: (),
}

impl TimingResponder {
    pub fn start(local_ip: Ipv4Addr, clock: Arc<NtpClock>) -> std::io::Result<TimingResponder> {
        let _ = (local_ip, clock);
        todo!("ntp: TimingResponder::start")
    }
    /// Local UDP port to put in SETUP `timingPort`.
    pub fn port(&self) -> u16 {
        todo!("ntp: port")
    }
    /// Timing requests answered so far.
    pub fn requests(&self) -> u64 {
        todo!("ntp: requests")
    }
    /// `rt::mono_ns()` of the last request (0 = none yet).
    pub fn last_request_ns(&self) -> u64 {
        todo!("ntp: last_request_ns")
    }
}
