//! Passive PTPv2 follower (workstream: real-time). CONTRACT: the public items below
//! are used by `session` and the engine; keep their signatures.
//!
//! Port of `legacy/python/winpods/ptp.py`. The receiver (or its group leader) is the
//! PTP master and sends Sync/Follow_Up to every timing peer named in SETUP; WinPods
//! only listens and never sends PTP. One dedicated thread (time-critical priority)
//! waits on both sockets (UDP 319 event + 320 general bound to the local IP, or one
//! dynamic port if those are taken) and timestamps each datagram with
//! `rt::mono_ns()` immediately after it is received.
//!
//! Parsing: accept only versionPTP (byte1 & 0x0F) == 2 and length ≥ 44; message type
//! = byte0 & 0x0F (0 Sync, 8 Follow_Up, 0xB Announce); flags u16 at 6 (two-step =
//! 0x0200); correction i64 at 8, `>> 16` → ns; clock identity = bytes 20..28; port
//! id 28..30; sequence u16 at 30; timestamp at 34 (48-bit seconds + 32-bit ns).
//! Clock choice: the expected ClockID from SETUP (or, without one, any Sync from the
//! receiver's IP), falling back to any clock after 2 s; the choice sticks.
//! Offset = max(T1 + corrections − t2) over a 4 s window; jumps > 5 ms are applied at
//! once, smaller changes slew 20 % toward the new value; master time =
//! local + offset + 1 ms path allowance. Fixture: `tests/fixtures/ptp.json`.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

/// Lock-free view of the followed clock, shared with the control (anchor) thread.
pub struct PtpClock {
    _private: (),
}

impl PtpClock {
    /// Convert an `rt::mono_ns()` value to the master's timescale (ns), once locked.
    pub fn to_master_ns(&self, local_ns: u64) -> Option<u64> {
        let _ = local_ns;
        todo!("ptp: to_master_ns")
    }
    /// Clock identity of the master the anchors must name (0 until locked).
    pub fn master_clock_id(&self) -> u64 {
        todo!("ptp: master_clock_id")
    }
    /// True when a sample arrived within the last 3 s.
    pub fn locked(&self) -> bool {
        todo!("ptp: locked")
    }
    /// Current offset estimate (master − local) in ns.
    pub fn offset_ns(&self) -> Option<f64> {
        todo!("ptp: offset_ns")
    }
    /// Spread between best and worst sample in the window (network jitter), ns.
    pub fn delay_spread_ns(&self) -> u64 {
        todo!("ptp: delay_spread_ns")
    }
    /// Sync messages seen (for diagnostics).
    pub fn syncs(&self) -> u64 {
        todo!("ptp: syncs")
    }
}

/// Error starting the follower.
#[derive(Debug, thiserror::Error)]
pub enum PtpError {
    #[error("could not bind PTP sockets: {0}")]
    Bind(#[from] std::io::Error),
}

/// The running follower (sockets + receive thread). Stops when dropped.
pub struct PtpService {
    _private: (),
}

impl PtpService {
    /// Bind UDP 319/320 on `local_ip` (falling back to one dynamic port when either
    /// is taken) and start the receive thread.
    pub fn start(local_ip: Ipv4Addr) -> Result<Arc<PtpService>, PtpError> {
        let _ = local_ip;
        todo!("ptp: start")
    }
    /// Local port the receiver must send to (319 when the standard ports are ours).
    pub fn port(&self) -> u16 {
        todo!("ptp: port")
    }
    /// True when bound to the standard ports 319/320.
    pub fn standard_ports(&self) -> bool {
        todo!("ptp: standard_ports")
    }
    /// Start tracking the master of the receiver at `ip`; `expected_clock` is
    /// `timingPeerInfo.ClockID` from the base SETUP response.
    pub fn follow(&self, ip: Ipv4Addr, expected_clock: Option<u64>) {
        let _ = (ip, expected_clock);
        todo!("ptp: follow")
    }
    /// Block until the first offset sample (or timeout). Returns true when locked.
    pub fn wait_locked(&self, timeout: Duration) -> bool {
        let _ = timeout;
        todo!("ptp: wait_locked")
    }
    /// The shared clock view for the anchor thread.
    pub fn clock(&self) -> Arc<PtpClock> {
        todo!("ptp: clock")
    }
}
