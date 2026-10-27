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
//!
//! pyatv's `ntp_now()` goes through float microseconds; here the fraction is computed
//! exactly from nanoseconds (`frac = ns·2³² / 1e9`), which only adds precision.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::warn;
use winpods_rt::{AutoEvent, ThreadPriority, mono_ns, set_current_thread_priority, wait_any};

use crate::control::EventedSocket;

/// Seconds from 1900-01-01 (NTP era 0) to 1970-01-01.
pub const NTP_UNIX_OFFSET_SECS: u64 = 0x83AA_7E80;
/// Size of a timing request or reply.
pub const TIMING_PACKET_BYTES: usize = 32;

const NS_PER_SEC: u128 = 1_000_000_000;

/// Convert nanoseconds since the Unix epoch to NTP 32.32 (seconds wrap per NTP era).
pub fn unix_ns_to_ntp(unix_ns: u128) -> u64 {
    let secs = (unix_ns / NS_PER_SEC) as u64 + NTP_UNIX_OFFSET_SECS;
    let frac = ((unix_ns % NS_PER_SEC) << 32) / NS_PER_SEC;
    (secs << 32) | frac as u64
}

/// NTP time anchored to the monotonic clock.
pub struct NtpClock {
    unix_ns: u128,
    mono_ns: u64,
}

impl NtpClock {
    /// Capture the wall clock now and anchor it to `rt::mono_ns()`.
    pub fn new() -> NtpClock {
        let mono = mono_ns();
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self::with_anchor(unix, mono)
    }
    /// A clock that reads `unix_ns` (ns since 1970) at `rt::mono_ns()` value `mono_ns`.
    pub fn with_anchor(unix_ns: u128, mono_ns: u64) -> NtpClock {
        NtpClock { unix_ns, mono_ns }
    }
    /// NTP timestamp (seconds since 1900 << 32 | fraction) for an `rt::mono_ns()` value.
    pub fn ntp_at(&self, mono_ns: u64) -> u64 {
        let unix = if mono_ns >= self.mono_ns {
            self.unix_ns + u128::from(mono_ns - self.mono_ns)
        } else {
            self.unix_ns
                .saturating_sub(u128::from(self.mono_ns - mono_ns))
        };
        unix_ns_to_ntp(unix)
    }
}

impl Default for NtpClock {
    fn default() -> Self {
        Self::new()
    }
}

/// The 0xD3 reply to a timing request received when the clock read `now_ntp`, or
/// `None` if `request` is not a timing request.
pub fn timing_reply(request: &[u8], now_ntp: u64) -> Option<[u8; TIMING_PACKET_BYTES]> {
    if request.len() < TIMING_PACKET_BYTES || request[1] & 0x7F != 0x52 {
        return None;
    }
    let mut reply = [0u8; TIMING_PACKET_BYTES];
    reply[0] = request[0];
    reply[1] = 0xD3;
    reply[2..4].copy_from_slice(&7u16.to_be_bytes());
    // padding (4..8) stays 0; reference time = the request's send time.
    reply[8..16].copy_from_slice(&request[24..32]);
    let now = now_ntp.to_be_bytes();
    reply[16..24].copy_from_slice(&now);
    reply[24..32].copy_from_slice(&now);
    Some(reply)
}

struct ResponderShared {
    stop: AtomicBool,
    wake: AutoEvent,
    requests: AtomicU64,
    last_request_ns: AtomicU64,
}

/// Answers the receiver's NTP timing requests.
pub struct TimingResponder {
    shared: Arc<ResponderShared>,
    port: u16,
    thread: Option<JoinHandle<()>>,
}

impl TimingResponder {
    pub fn start(local_ip: Ipv4Addr, clock: Arc<NtpClock>) -> std::io::Result<TimingResponder> {
        let socket = UdpSocket::bind((local_ip, 0))?;
        let port = socket.local_addr()?.port();
        let socket = EventedSocket::new(socket)?;
        let shared = Arc::new(ResponderShared {
            stop: AtomicBool::new(false),
            wake: AutoEvent::new().map_err(std::io::Error::other)?,
            requests: AtomicU64::new(0),
            last_request_ns: AtomicU64::new(0),
        });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("winpods-ntp".into())
            .spawn(move || responder_thread(&socket, &clock, &thread_shared))?;
        Ok(TimingResponder {
            shared,
            port,
            thread: Some(thread),
        })
    }
    /// Local UDP port to put in SETUP `timingPort`.
    pub fn port(&self) -> u16 {
        self.port
    }
    /// Timing requests answered so far.
    pub fn requests(&self) -> u64 {
        self.shared.requests.load(Ordering::Relaxed)
    }
    /// `rt::mono_ns()` of the last request (0 = none yet).
    pub fn last_request_ns(&self) -> u64 {
        self.shared.last_request_ns.load(Ordering::Relaxed)
    }
}

impl Drop for TimingResponder {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.set();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("NTP timing thread panicked");
        }
    }
}

fn responder_thread(socket: &EventedSocket, clock: &NtpClock, shared: &ResponderShared) {
    set_current_thread_priority(ThreadPriority::AboveNormal);
    let handles = [shared.wake.handle(), socket.handle()];
    let mut buf = [0u8; 512];
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        while !shared.stop.load(Ordering::Acquire) {
            if let Err(e) = wait_any(&handles, Some(Duration::from_millis(500))) {
                warn!("NTP timing: wait failed: {e}");
                return;
            }
            loop {
                match socket.socket().recv_from(&mut buf) {
                    Ok((len, from)) => answer(socket, clock, shared, &buf[..len], from),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => break,
                }
            }
        }
    }));
    if outcome.is_err() {
        warn!("NTP timing thread panicked");
    }
}

fn answer(
    socket: &EventedSocket,
    clock: &NtpClock,
    shared: &ResponderShared,
    request: &[u8],
    from: SocketAddr,
) {
    let now = mono_ns();
    let Some(reply) = timing_reply(request, clock.ntp_at(now)) else {
        return;
    };
    if socket.socket().send_to(&reply, from).is_ok() {
        shared.requests.fetch_add(1, Ordering::Relaxed);
        shared.last_request_ns.store(now.max(1), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_to_ntp_conversion() {
        assert_eq!(unix_ns_to_ntp(0), NTP_UNIX_OFFSET_SECS << 32);
        // 0.5 s → fraction 2^31.
        assert_eq!(
            unix_ns_to_ntp(1_500_000_000),
            ((NTP_UNIX_OFFSET_SECS + 1) << 32) | 0x8000_0000
        );
        // Matches pyatv's ntp_now() for a whole-microsecond time.
        let unix_ns: u128 = 1_761_000_000_123_456_000;
        let us = unix_ns / 1000;
        let secs = (us / 1_000_000) as u64;
        let frac_us = (us % 1_000_000) as u64;
        let python = ((secs + NTP_UNIX_OFFSET_SECS) << 32) | ((frac_us << 32) / 1_000_000);
        assert_eq!(unix_ns_to_ntp(unix_ns), python);
    }

    #[test]
    fn clock_advances_with_mono_and_handles_earlier_values() {
        let clock = NtpClock::with_anchor(10_000_000_000, 5_000);
        assert_eq!(clock.ntp_at(5_000), unix_ns_to_ntp(10_000_000_000));
        assert_eq!(
            clock.ntp_at(5_000 + 250_000_000),
            unix_ns_to_ntp(10_250_000_000)
        );
        assert_eq!(clock.ntp_at(0), unix_ns_to_ntp(10_000_000_000 - 5_000));
        let live = NtpClock::new();
        let a = live.ntp_at(mono_ns());
        let b = live.ntp_at(mono_ns() + 1_000_000_000);
        assert!(b > a);
    }

    #[test]
    fn timing_reply_layout() {
        let mut request = [0u8; 32];
        request[0] = 0x80;
        request[1] = 0xD2;
        request[2..4].copy_from_slice(&7u16.to_be_bytes());
        request[24..32].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        let reply = timing_reply(&request, 0xAABB_CCDD_EEFF_0011).unwrap();
        assert_eq!(
            hex::encode(reply),
            "80d3000700000000010203040506070\
             8aabbccddeeff0011aabbccddeeff0011"
        );
        assert!(timing_reply(&request[..31], 0).is_none());
        request[1] = 0xD4;
        assert!(timing_reply(&request, 0).is_none());
        request[1] = 0x52;
        assert!(timing_reply(&request, 0).is_some());
    }
}
