//! Control channel: timing anchors and retransmissions (workstream: real-time).
//!
//! * PTP anchor 0xD7, 28 bytes `>BBHIQIQ`: 0x90 (first) / 0x80, 0xD7, 0x0006,
//!   (head − latency) as u32, master time ns u64, head as u32, master clock id u64.
//! * NTP sync 0xD4, 20 bytes: 0x90/0x80, 0xD4, 0x0007, (head − latency) u32,
//!   ntp seconds u32, ntp fraction u32, head u32.
//! * Retransmit request: `byte1 & 0x7F == 0x55`, first lost seq at offset 4 (u16),
//!   count at offset 6 (u16); reply `0x80 0xD6` + seq (2 bytes) + stored packet.
//!
//! Runs on a dedicated above-normal thread: anchor every 1 s (first one before any
//! audio), retransmit replies as requests arrive; tolerates WSAECONNRESET.
//! Fixture: `anchors.json`.
//!
//! With PTP, anchors are held back while the followed clock is not locked (no sample
//! for 3 s), with one warning per outage. If it stays unlocked for more than
//! [`PTP_LOST_AFTER_NS`], the stream's end reason becomes [`PTP_LOST_REASON`] so the
//! supervisor reconnects (a new session follows the receiver's clock afresh). Python
//! kept sending anchors from the frozen offset and never noticed.
//!
//! Port of `LiveControl` in `legacy/python/winpods/raop_live.py`. The anchor head is
//! the smoothed timeline at the moment the anchor is built, rounded half to even.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tracing::{info, warn};
use windows::Win32::Networking::WinSock::{
    FD_READ, SOCKET, WSAEVENT, WSAEventSelect, WSAGetLastError,
};
use winpods_rt::{AutoEvent, HANDLE, HiResTimer, mono_ns, wait_any};

use crate::backlog::{Backlog, MAX_STORED_PACKET};
use crate::ntp::NtpClock;
use crate::ptp::{PtpClock, STALE_AFTER_NS};
use crate::runtime::Shared;
use crate::timeline::{Timeline, round_head};

/// Size of a 0xD7 PTP anchor.
pub const ANCHOR_D7_BYTES: usize = 28;
/// Size of a 0xD4 NTP sync packet.
pub const SYNC_D4_BYTES: usize = 20;
/// Largest retransmit reply: 4-byte prefix + a stored packet.
pub const MAX_REPLY_BYTES: usize = 4 + MAX_STORED_PACKET;
/// Interval between anchors.
pub const ANCHOR_INTERVAL_NS: u64 = 1_000_000_000;
/// How long the followed PTP clock may stay unlocked while a stream runs before the
/// stream ends with [`PTP_LOST_REASON`].
pub const PTP_LOST_AFTER_NS: u64 = 10_000_000_000;
/// End reason of a stream whose PTP clock stayed unlocked for [`PTP_LOST_AFTER_NS`].
pub const PTP_LOST_REASON: &str = "lost the receiver's clock (PTP)";

fn marker(first: bool) -> u8 {
    if first { 0x90 } else { 0x80 }
}

/// Build a 0xD7 AirPlay 2 timing anchor: "frame `head − latency` plays at master
/// time `master_ns`; frame `head` was just sent". `head` is unwrapped.
pub fn anchor_d7(
    first: bool,
    head: u64,
    latency: u32,
    master_ns: u64,
    clock_id: u64,
) -> [u8; ANCHOR_D7_BYTES] {
    let mut packet = [0u8; ANCHOR_D7_BYTES];
    packet[0] = marker(first);
    packet[1] = 0xD7;
    packet[2..4].copy_from_slice(&0x0006u16.to_be_bytes());
    let played = head.wrapping_sub(u64::from(latency)) as u32;
    packet[4..8].copy_from_slice(&played.to_be_bytes());
    packet[8..16].copy_from_slice(&master_ns.to_be_bytes());
    packet[16..20].copy_from_slice(&(head as u32).to_be_bytes());
    packet[20..28].copy_from_slice(&clock_id.to_be_bytes());
    packet
}

/// Build a 0xD4 NTP sync packet (`ntp` = seconds since 1900 << 32 | fraction).
pub fn sync_d4(first: bool, head: u64, latency: u32, ntp: u64) -> [u8; SYNC_D4_BYTES] {
    let mut packet = [0u8; SYNC_D4_BYTES];
    packet[0] = marker(first);
    packet[1] = 0xD4;
    packet[2..4].copy_from_slice(&0x0007u16.to_be_bytes());
    let played = head.wrapping_sub(u64::from(latency)) as u32;
    packet[4..8].copy_from_slice(&played.to_be_bytes());
    packet[8..12].copy_from_slice(&((ntp >> 32) as u32).to_be_bytes());
    packet[12..16].copy_from_slice(&(ntp as u32).to_be_bytes());
    packet[16..20].copy_from_slice(&(head as u32).to_be_bytes());
    packet
}

/// A parsed retransmit request (`RetransmitReqeust` in pyatv).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetransmitRequest {
    /// First missing sequence number.
    pub first_seq: u16,
    /// Number of consecutive packets requested.
    pub count: u16,
}

impl RetransmitRequest {
    /// Parse a control datagram; `None` unless it is an ≥8-byte retransmit request
    /// (`byte1 & 0x7F == 0x55`; trailing bytes are ignored).
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 8 || data[1] & 0x7F != 0x55 {
            return None;
        }
        Some(Self {
            first_seq: u16::from_be_bytes([data[4], data[5]]),
            count: u16::from_be_bytes([data[6], data[7]]),
        })
    }

    /// The requested sequence numbers, wrapping at 65536.
    pub fn seqs(self) -> impl Iterator<Item = u16> {
        (0..self.count).map(move |i| self.first_seq.wrapping_add(i))
    }
}

/// Answer a control datagram from the backlog: for every requested packet still
/// stored, build `80 D6 <its seq> <packet>` in `scratch` and pass it to `send`.
/// Returns `None` if `request` is not a retransmit request, otherwise the number of
/// replies produced. Never allocates.
pub fn answer_retransmit(
    request: &[u8],
    backlog: &Backlog,
    scratch: &mut [u8; MAX_REPLY_BYTES],
    mut send: impl FnMut(&[u8]),
) -> Option<usize> {
    let request = RetransmitRequest::parse(request)?;
    let mut replies = 0;
    for seq in request.seqs() {
        let Some(len) = backlog.copy_out(seq, &mut scratch[4..]) else {
            continue;
        };
        if len < 4 {
            continue;
        }
        scratch[0] = 0x80;
        scratch[1] = 0xD6;
        scratch[2] = scratch[6];
        scratch[3] = scratch[7];
        send(&scratch[..4 + len]);
        replies += 1;
    }
    Some(replies)
}

/// A UDP socket whose readability signals a Win32 event, so a thread can wait for it
/// together with other handles in [`wait_any`]. The socket is non-blocking.
pub(crate) struct EventedSocket {
    socket: UdpSocket,
    event: AutoEvent,
}

impl EventedSocket {
    pub(crate) fn new(socket: UdpSocket) -> io::Result<Self> {
        let event = AutoEvent::new().map_err(io::Error::other)?;
        let raw = SOCKET(socket.as_raw_socket() as usize);
        let wsa_event = WSAEVENT(event.handle().0 as isize);
        // SAFETY: both the socket and the event are valid for the lifetime of `self`;
        // the association is removed in `drop` before either is closed.
        let rc = unsafe { WSAEventSelect(raw, Some(wsa_event), FD_READ as i32) };
        if rc != 0 {
            let code = unsafe { WSAGetLastError() };
            return Err(io::Error::from_raw_os_error(code.0));
        }
        Ok(Self { socket, event })
    }

    pub(crate) fn socket(&self) -> &UdpSocket {
        &self.socket
    }

    /// Event signaled when a datagram is waiting.
    pub(crate) fn handle(&self) -> HANDLE {
        self.event.handle()
    }
}

impl Drop for EventedSocket {
    fn drop(&mut self) {
        let raw = SOCKET(self.socket.as_raw_socket() as usize);
        // SAFETY: the socket is still open; this only cancels the event association.
        unsafe {
            let _ = WSAEventSelect(raw, None, 0);
        }
    }
}

/// Where anchor times come from.
#[derive(Clone)]
pub(crate) enum AnchorClock {
    /// 0xD7 anchors in the followed PTP master's timescale.
    Ptp(Arc<PtpClock>),
    /// 0xD4 sync packets with NTP wall-clock time.
    Ntp(Arc<NtpClock>),
}

/// State of the control thread: the socket plus everything needed for anchors and
/// retransmit replies.
pub(crate) struct ControlLoop {
    socket: EventedSocket,
    dest: SocketAddr,
    clock: AnchorClock,
    latency: u32,
    first_ts: u64,
    first: bool,
    scratch: Box<[u8; MAX_REPLY_BYTES]>,
    /// `rt::mono_ns()` at creation: a PTP clock without any sample counts as
    /// unlocked since then.
    started_ns: u64,
    /// True while a PTP outage is in progress and has been logged.
    clock_warned: bool,
    /// True once the stream was ended for a lost PTP clock.
    clock_lost: bool,
}

impl ControlLoop {
    pub(crate) fn new(
        socket: EventedSocket,
        dest: SocketAddr,
        clock: AnchorClock,
        latency: u32,
        first_ts: u64,
    ) -> Self {
        Self {
            socket,
            dest,
            clock,
            latency,
            first_ts,
            first: true,
            scratch: Box::new([0u8; MAX_REPLY_BYTES]),
            started_ns: mono_ns(),
            clock_warned: false,
            clock_lost: false,
        }
    }

    /// Whether an anchor may be built at `t_ns`: always with NTP; with PTP only while
    /// the followed clock is locked, because a stale offset no longer tracks the
    /// receiver. Logs once per outage, and ends the stream (through `shared`) once
    /// the clock has been unlocked for longer than [`PTP_LOST_AFTER_NS`].
    fn clock_ready(&mut self, t_ns: u64, shared: &Shared) -> bool {
        let AnchorClock::Ptp(ptp) = &self.clock else {
            return true;
        };
        if ptp.locked_at(t_ns) {
            if self.clock_warned {
                self.clock_warned = false;
                info!("PTP: the receiver's clock is locked again; anchors resume");
            }
            return true;
        }
        let unlocked_ns = ptp
            .unlocked_for_ns(t_ns)
            .unwrap_or_else(|| t_ns.saturating_sub(self.started_ns));
        if !self.clock_warned {
            self.clock_warned = true;
            warn!(
                "PTP: the receiver's clock is not locked (no sync in the last {} s); \
                 holding anchors until it is",
                STALE_AFTER_NS / 1_000_000_000
            );
        }
        if unlocked_ns > PTP_LOST_AFTER_NS && !self.clock_lost {
            self.clock_lost = true;
            shared.fail(PTP_LOST_REASON.into());
        }
        false
    }

    /// The anchor for `t_ns` given the current timeline, or `None` while the PTP
    /// master is unknown (the next attempt follows a second later).
    fn build_anchor(&self, t_ns: u64, timeline: &Timeline) -> Option<([u8; 28], usize)> {
        let head = if timeline.is_started() {
            round_head(timeline.head_at(t_ns))
        } else {
            self.first_ts
        };
        match &self.clock {
            AnchorClock::Ptp(ptp) => {
                let master_ns = ptp.to_master_ns(t_ns)?;
                let clock_id = ptp.master_clock_id();
                if clock_id == 0 {
                    return None;
                }
                let anchor = anchor_d7(self.first, head, self.latency, master_ns, clock_id);
                Some((anchor, ANCHOR_D7_BYTES))
            }
            AnchorClock::Ntp(ntp) => {
                let mut out = [0u8; ANCHOR_D7_BYTES];
                let sync = sync_d4(self.first, head, self.latency, ntp.ntp_at(t_ns));
                out[..SYNC_D4_BYTES].copy_from_slice(&sync);
                Some((out, SYNC_D4_BYTES))
            }
        }
    }

    /// Build and send the anchor for "now". Returns true when one was sent.
    pub(crate) fn send_anchor(&mut self, shared: &Shared) -> bool {
        self.send_anchor_at(mono_ns(), shared)
    }

    /// [`send_anchor`](Self::send_anchor) for `rt::mono_ns()` value `t_ns`.
    fn send_anchor_at(&mut self, t_ns: u64, shared: &Shared) -> bool {
        if !self.clock_ready(t_ns, shared) {
            return false;
        }
        let timeline = shared.timeline.load();
        let Some((packet, len)) = self.build_anchor(t_ns, &timeline) else {
            return false;
        };
        match self.socket.socket().send_to(&packet[..len], self.dest) {
            Ok(_) => {
                self.first = false;
                shared.stats.anchors.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                shared.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Read and answer every queued control datagram.
    fn drain(&mut self, shared: &Shared, buf: &mut [u8]) {
        loop {
            match self.socket.socket().recv_from(buf) {
                Ok((len, from)) => self.answer(&buf[..len], from, shared),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // WSAECONNRESET: an ICMP "port unreachable" for an earlier send.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return,
            }
        }
    }

    fn answer(&mut self, data: &[u8], from: SocketAddr, shared: &Shared) {
        let socket = &self.socket.socket;
        let stats = &shared.stats;
        let answered = answer_retransmit(data, &shared.backlog, &mut self.scratch, |reply| {
            match socket.send_to(reply, from) {
                Ok(_) => stats.retransmitted.fetch_add(1, Ordering::Relaxed),
                Err(_) => stats.send_errors.fetch_add(1, Ordering::Relaxed),
            };
        });
        if answered.is_some() {
            stats.retransmit_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The thread body: anchors every second (the first one was already sent by
    /// `runtime::start`), retransmit replies whenever requests arrive, until `stop`.
    pub(crate) fn run(&mut self, shared: &Shared, wake: &AutoEvent) -> io::Result<()> {
        let timer = HiResTimer::new().map_err(io::Error::other)?;
        let handles = [wake.handle(), self.socket.handle(), timer.handle()];
        let mut buf = [0u8; 2048];
        let mut next_anchor = mono_ns() + ANCHOR_INTERVAL_NS;
        loop {
            if shared.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let now = mono_ns();
            if now >= next_anchor {
                self.send_anchor(shared);
                next_anchor += ANCHOR_INTERVAL_NS;
                if next_anchor <= now {
                    // We were held up for more than a second; do not burst.
                    next_anchor = now + ANCHOR_INTERVAL_NS;
                }
            }
            let due_in = next_anchor.saturating_sub(mono_ns()).max(1);
            timer
                .arm(Duration::from_nanos(due_in))
                .map_err(io::Error::other)?;
            // The timer bounds the wait; the timeout is only a safety net.
            wait_any(&handles, Some(Duration::from_secs(2)))?;
            self.drain(shared, &mut buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::master_time_ns;
    use serde_json::Value;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/anchors.json")).unwrap()
    }

    #[test]
    fn d7_anchors_match_fixture() {
        let fx = fixture();
        let cases = fx["d7"].as_array().unwrap();
        assert_eq!(cases.len(), 11);
        for (i, case) in cases.iter().enumerate() {
            let t_ns = case["t_ns"].as_u64().unwrap();
            let head = case["head"].as_u64().unwrap();
            if let Some(bits) = case["head_at_bits"].as_str() {
                // Rebuild head_at exactly like the generator: start at (t, head) and
                // set err to the fractional part, then round half to even.
                let head_at = f64::from_bits(u64::from_str_radix(bits, 16).unwrap());
                let mut timeline = Timeline::new();
                let base = head_at.round();
                timeline.start(t_ns, base as u64);
                timeline.set_err(head_at - base);
                assert_eq!(
                    timeline.head_at(t_ns).to_bits(),
                    head_at.to_bits(),
                    "case {i}"
                );
                assert_eq!(round_head(head_at), head, "case {i}");
            }
            let offset = case["ptp_offset_ns"].as_f64().unwrap();
            let master_ns = master_time_ns(t_ns, offset);
            assert_eq!(master_ns, case["master_ns"].as_u64().unwrap(), "case {i}");
            let packet = anchor_d7(
                case["first"].as_bool().unwrap(),
                head,
                case["latency"].as_u64().unwrap() as u32,
                master_ns,
                case["clock_id"].as_u64().unwrap(),
            );
            assert_eq!(
                hex::encode(packet),
                case["bytes"].as_str().unwrap(),
                "case {i}"
            );
        }
    }

    #[test]
    fn d4_syncs_match_fixture() {
        let fx = fixture();
        for (i, case) in fx["d4"].as_array().unwrap().iter().enumerate() {
            let ntp = case["ntp"].as_u64().unwrap();
            assert_eq!(ntp >> 32, case["ntp_sec"].as_u64().unwrap());
            assert_eq!(ntp & 0xFFFF_FFFF, case["ntp_frac"].as_u64().unwrap());
            let packet = sync_d4(
                case["first"].as_bool().unwrap(),
                case["head"].as_u64().unwrap(),
                case["latency"].as_u64().unwrap() as u32,
                ntp,
            );
            assert_eq!(
                hex::encode(packet),
                case["bytes"].as_str().unwrap(),
                "case {i}"
            );
        }
    }

    #[test]
    fn retransmit_replies_match_fixture() {
        let fx = fixture();
        let retransmit = &fx["retransmit"];
        let backlog = Backlog::new();
        for (seq, packet) in retransmit["backlog"].as_object().unwrap() {
            let seq: u16 = seq.parse().unwrap();
            assert!(backlog.store(seq, &hex::decode(packet.as_str().unwrap()).unwrap()));
        }
        let mut scratch = [0u8; MAX_REPLY_BYTES];
        for case in retransmit["requests"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let request = hex::decode(case["request"].as_str().unwrap()).unwrap();
            let mut replies = Vec::new();
            let answered = answer_retransmit(&request, &backlog, &mut scratch, |r| {
                replies.push(hex::encode(r))
            });
            let expected: Vec<String> = case["replies"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect();
            assert_eq!(replies, expected, "{name}");
            let is_request = !matches!(name, "other_type_ignored" | "too_short");
            assert_eq!(answered.is_some(), is_request, "{name}");
        }
    }

    #[test]
    fn retransmit_request_parsing() {
        assert_eq!(
            RetransmitRequest::parse(&[0x80, 0xD5, 0, 1, 0xFF, 0xFE, 0, 4]),
            Some(RetransmitRequest {
                first_seq: 0xFFFE,
                count: 4
            })
        );
        let seqs: Vec<u16> = RetransmitRequest {
            first_seq: 0xFFFE,
            count: 4,
        }
        .seqs()
        .collect();
        assert_eq!(seqs, [0xFFFE, 0xFFFF, 0, 1]);
        assert_eq!(RetransmitRequest::parse(&[0x80, 0xD5, 0, 1, 0, 0, 0]), None);
        assert_eq!(
            RetransmitRequest::parse(&[0x80, 0xD4, 0, 1, 0, 0, 0, 1]),
            None
        );
    }

    #[test]
    fn anchor_builder_uses_timeline_and_ptp_state() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = socket.local_addr().unwrap();
        let evented = EventedSocket::new(socket).unwrap();
        let mut tracker = crate::ptp::PtpTracker::new();
        let ptp = tracker.clock().clone();
        let control = ControlLoop::new(evented, dest, AnchorClock::Ptp(ptp), 4410, 4410);
        let mut timeline = Timeline::new();
        timeline.start(1_000_000_000, 4410);
        // No master yet: nothing to send.
        assert!(control.build_anchor(1_000_000_000, &timeline).is_none());

        let ip = std::net::Ipv4Addr::LOCALHOST;
        tracker.follow(ip, Some(0x1122), 0);
        let mut sync = vec![0u8; 44];
        sync[1] = 2;
        sync[20..28].copy_from_slice(&0x1122u64.to_be_bytes());
        sync[34..40].copy_from_slice(&[0, 0, 0, 0, 0, 100]);
        assert!(tracker.on_datagram(&sync, ip, 1_000_000_000));
        let (packet, len) = control.build_anchor(2_000_000_000, &timeline).unwrap();
        assert_eq!(len, ANCHOR_D7_BYTES);
        // offset = T1 − t2 = 100 s − 1 s; master = 2 s + 99 s + 1 ms path allowance.
        let expected = anchor_d7(true, 4410 + 44100, 4410, 101_001_000_000, 0x1122);
        assert_eq!(packet, expected);
    }

    const SEC: u64 = 1_000_000_000;

    /// A control loop sending to a local socket, its PTP tracker, and stream state.
    fn ptp_control(started_ns: u64) -> (ControlLoop, crate::ptp::PtpTracker, Shared, UdpSocket) {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = receiver.local_addr().unwrap();
        let evented = EventedSocket::new(UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap();
        let mut tracker = crate::ptp::PtpTracker::new();
        tracker.follow(std::net::Ipv4Addr::LOCALHOST, Some(0x1122), 0);
        let clock = AnchorClock::Ptp(tracker.clock().clone());
        let mut control = ControlLoop::new(evented, dest, clock, 4410, 4410);
        control.started_ns = started_ns;
        let shared = Shared::new(AutoEvent::new().unwrap(), AutoEvent::new().unwrap());
        (control, tracker, shared, receiver)
    }

    fn ptp_sample(tracker: &mut crate::ptp::PtpTracker, t2: u64) {
        let mut sync = vec![0u8; 44];
        sync[1] = 2;
        sync[20..28].copy_from_slice(&0x1122u64.to_be_bytes());
        sync[34..40].copy_from_slice(&[0, 0, 0, 0, 0, 100]);
        assert!(tracker.on_datagram(&sync, std::net::Ipv4Addr::LOCALHOST, t2));
    }

    #[test]
    fn stale_ptp_clock_holds_anchors_then_ends_the_stream() {
        let (mut control, mut tracker, shared, _receiver) = ptp_control(SEC);
        let anchors = || shared.stats.anchors.load(Ordering::Relaxed);

        ptp_sample(&mut tracker, 2 * SEC);
        assert!(control.send_anchor_at(3 * SEC, &shared));
        assert_eq!(anchors(), 1);

        // No sample for 3 s: the frozen offset is not used for anchors any more.
        let stale = 2 * SEC + STALE_AFTER_NS;
        assert!(!control.send_anchor_at(stale, &shared));
        assert!(control.clock_warned);
        assert!(!control.send_anchor_at(stale + 5 * SEC, &shared));
        assert_eq!(anchors(), 1);
        assert_eq!(shared.end_reason(), None, "a short outage is ridden out");

        // Still unlocked 10 s after going stale: not yet...
        assert!(!control.send_anchor_at(stale + PTP_LOST_AFTER_NS, &shared));
        assert_eq!(shared.end_reason(), None);
        // ...but past that, the stream ends so the supervisor reconnects.
        assert!(!control.send_anchor_at(stale + PTP_LOST_AFTER_NS + 1, &shared));
        assert_eq!(shared.end_reason().as_deref(), Some(PTP_LOST_REASON));

        // Samples again: anchors resume (the end reason stays until the restart).
        let later = stale + PTP_LOST_AFTER_NS + 2 * SEC;
        ptp_sample(&mut tracker, later);
        assert!(control.send_anchor_at(later + SEC, &shared));
        assert!(!control.clock_warned);
        assert_eq!(anchors(), 2);
        assert_eq!(shared.end_reason().as_deref(), Some(PTP_LOST_REASON));
    }

    #[test]
    fn short_ptp_outages_are_ridden_out() {
        let (mut control, mut tracker, shared, _receiver) = ptp_control(0);
        // Outages of 9 s each, with a sample in between: never 10 s unlocked in a row.
        let mut t = SEC;
        for _ in 0..3 {
            ptp_sample(&mut tracker, t);
            assert!(control.send_anchor_at(t + SEC, &shared));
            t += STALE_AFTER_NS + 9 * SEC;
            assert!(!control.send_anchor_at(t, &shared));
            assert!(control.clock_warned);
        }
        assert_eq!(shared.end_reason(), None);
        assert_eq!(shared.stats.anchors.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn ptp_clock_that_never_locks_ends_the_stream() {
        let start = 5 * SEC;
        let (mut control, _tracker, shared, _receiver) = ptp_control(start);
        assert!(!control.send_anchor_at(start, &shared));
        assert!(!control.send_anchor_at(start + PTP_LOST_AFTER_NS, &shared));
        assert_eq!(shared.end_reason(), None);
        assert!(!control.send_anchor_at(start + PTP_LOST_AFTER_NS + SEC, &shared));
        assert_eq!(shared.end_reason().as_deref(), Some(PTP_LOST_REASON));
        assert_eq!(shared.stats.anchors.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn ntp_anchors_do_not_depend_on_ptp() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = receiver.local_addr().unwrap();
        let evented = EventedSocket::new(UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap();
        let clock = AnchorClock::Ntp(Arc::new(NtpClock::new()));
        let mut control = ControlLoop::new(evented, dest, clock, 4410, 4410);
        let shared = Shared::new(AutoEvent::new().unwrap(), AutoEvent::new().unwrap());
        assert!(control.send_anchor_at(control.started_ns + 60 * SEC, &shared));
        assert_eq!(shared.end_reason(), None);
    }
}
