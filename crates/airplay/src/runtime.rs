//! Starts and stops the real-time part of a stream (workstream: real-time).
//! CONTRACT: used by the engine; keep the public signatures.
//!
//! `start` (step 15 of the session sequence): clear the ring, start the timeline at
//! `(rt::mono_ns(), params.first_ts)`, spawn the control thread (sends the first
//! anchor immediately: 0xD7 with PTP, 0xD4 with NTP), then spawn the sender thread
//! whose first packet uses `params.first_seq` / `params.first_ts`. While a stream
//! runs, hold an `rt::ThrottleGuard`. `stop` joins both threads and hands the ring
//! consumer back so the next session reuses it.

use std::net::Ipv4Addr;
use std::sync::Arc;

use winpods_rt::RingConsumer;

use crate::ntp::NtpClock;
use crate::ptp::PtpClock;
use crate::types::StreamParams;

/// The local control socket. Bound before SETUP because its port goes into the
/// stream SETUP request (`controlPort`).
pub struct ControlSocket {
    _private: (),
}

impl ControlSocket {
    pub fn bind(local_ip: Ipv4Addr) -> std::io::Result<ControlSocket> {
        let _ = local_ip;
        todo!("runtime: ControlSocket::bind")
    }
    pub fn port(&self) -> u16 {
        todo!("runtime: ControlSocket::port")
    }
}

/// Clock source for anchors plus diagnostics.
#[derive(Clone, Default)]
pub struct RuntimeOptions {
    /// Followed PTP clock (PTP timing). Anchors are 0xD7 packets.
    pub ptp: Option<Arc<PtpClock>>,
    /// NTP clock (NTP timing). Anchors are 0xD4 sync packets.
    pub ntp: Option<Arc<NtpClock>>,
    /// Diagnostic: do not put every Nth audio packet on the wire (it is still stored
    /// in the backlog so the receiver can request it). Used by `--drop-every`.
    pub drop_every: Option<u32>,
}

/// Counters for status displays and the CLI stats line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub packets: u64,
    pub silence_packets: u64,
    pub frames_trimmed: u64,
    pub stalls: u64,
    pub retransmit_requests: u64,
    pub retransmitted: u64,
    pub anchors: u64,
    pub send_errors: u64,
    /// Packets held back by the `drop_every` diagnostic.
    pub withheld: u64,
    /// `rt::mono_ns()` when the stream started.
    pub started_ns: u64,
}

/// A running stream. Dropping it without `stop` also stops the threads.
pub struct StreamHandle {
    _private: (),
}

impl StreamHandle {
    pub fn stats(&self) -> StreamStats {
        todo!("runtime: stats")
    }
    /// Set when a thread failed or panicked (the supervisor then reconnects).
    pub fn end_reason(&self) -> Option<String> {
        todo!("runtime: end_reason")
    }
    /// Stop both threads and return the ring consumer.
    pub fn stop(self) -> RingConsumer {
        todo!("runtime: stop")
    }
}

/// Start the control and sender threads for a negotiated stream.
pub fn start(
    params: &StreamParams,
    control: ControlSocket,
    consumer: RingConsumer,
    options: RuntimeOptions,
) -> std::io::Result<StreamHandle> {
    let _ = (params, control, consumer, options);
    todo!("runtime: start")
}
