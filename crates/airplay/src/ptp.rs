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
//!
//! Structure: [`PtpTracker`] is the socket-less state machine (it takes the receive
//! time as an argument, so the fixture can drive it); [`PtpService`] owns the sockets
//! and the thread that feeds it; [`PtpClock`] is the lock-free view the tracker
//! publishes after every message, read by the anchor thread.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use tracing::{debug, info, warn};
use winpods_rt::{AutoEvent, ThreadPriority, mono_ns, set_current_thread_priority, wait_any};

use crate::control::EventedSocket;

/// PTP event port (Sync).
pub const PTP_EVENT_PORT: u16 = 319;
/// PTP general port (Follow_Up, Announce).
pub const PTP_GENERAL_PORT: u16 = 320;

/// Message types (`byte0 & 0x0F`).
pub const MSG_SYNC: u8 = 0x0;
pub const MSG_FOLLOW_UP: u8 = 0x8;
pub const MSG_ANNOUNCE: u8 = 0xB;
/// Two-step flag in the flags field: the precise send time follows in a Follow_Up.
pub const FLAG_TWO_STEP: u16 = 0x0200;

/// Common header (34 bytes) plus the 10-byte origin timestamp.
const MIN_MESSAGE_LEN: usize = 44;
/// The offset estimate is the best sample of this window.
pub const WINDOW_NS: u64 = 4_000_000_000;
/// Offset changes larger than this are applied at once; smaller ones are slewed.
pub const STEP_NS: f64 = 5_000_000.0;
/// Fraction of a small offset change applied per sample.
pub const SLEW: f64 = 0.2;
/// Typical minimum one-way delay to a Wi-Fi speaker, added back to the estimate.
pub const ASSUMED_PATH_DELAY_NS: i64 = 1_000_000;
/// Without samples for this long the clock no longer counts as locked.
pub const STALE_AFTER_NS: u64 = 3_000_000_000;
/// Accept any master if the expected one stays silent this long after `follow`.
pub const FALLBACK_AFTER_NS: u64 = 2_000_000_000;
/// Two-step Syncs waiting for their Follow_Up.
pub const MAX_PENDING: usize = 64;
/// Samples kept in the window. A master sends ~8 Syncs/s (32 per window); the cap
/// only bounds memory against a flood and is never reached in normal operation.
const MAX_SAMPLES: usize = 1024;

/// `int(local_ns + offset) + 1_000_000`, masked to 64 bits like the Python anchor.
pub fn master_time_ns(local_ns: u64, offset_ns: f64) -> u64 {
    let master = (local_ns as f64 + offset_ns) as i128;
    master.wrapping_add(i128::from(ASSUMED_PATH_DELAY_NS)) as u64
}

/// Lock-free view of the followed clock, shared with the control (anchor) thread.
pub struct PtpClock {
    /// f64 bits of the offset; NaN = no estimate yet.
    offset_bits: AtomicU64,
    master_clock_id: AtomicU64,
    last_sample_ns: AtomicU64,
    delay_spread_ns: AtomicU64,
    syncs: AtomicU64,
    follow_ups: AtomicU64,
    announces: AtomicU64,
    ignored: AtomicU64,
}

impl PtpClock {
    fn new() -> Self {
        Self {
            offset_bits: AtomicU64::new(f64::NAN.to_bits()),
            master_clock_id: AtomicU64::new(0),
            last_sample_ns: AtomicU64::new(0),
            delay_spread_ns: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
            follow_ups: AtomicU64::new(0),
            announces: AtomicU64::new(0),
            ignored: AtomicU64::new(0),
        }
    }

    /// Convert an `rt::mono_ns()` value to the master's timescale (ns), once locked.
    pub fn to_master_ns(&self, local_ns: u64) -> Option<u64> {
        self.offset_ns()
            .map(|offset| master_time_ns(local_ns, offset))
    }
    /// Clock identity of the master the anchors must name (0 until locked).
    pub fn master_clock_id(&self) -> u64 {
        self.master_clock_id.load(Ordering::Acquire)
    }
    /// True when a sample arrived within the last 3 s.
    pub fn locked(&self) -> bool {
        self.locked_at(mono_ns())
    }
    /// [`locked`](Self::locked) evaluated at `rt::mono_ns()` value `now_ns`.
    pub fn locked_at(&self, now_ns: u64) -> bool {
        self.offset_ns().is_some()
            && now_ns.saturating_sub(self.last_sample_ns.load(Ordering::Acquire)) < STALE_AFTER_NS
    }
    /// Current offset estimate (master − local) in ns.
    pub fn offset_ns(&self) -> Option<f64> {
        let offset = f64::from_bits(self.offset_bits.load(Ordering::Acquire));
        (!offset.is_nan()).then_some(offset)
    }
    /// Spread between best and worst sample in the window (network jitter), ns.
    pub fn delay_spread_ns(&self) -> u64 {
        self.delay_spread_ns.load(Ordering::Relaxed)
    }
    /// Sync messages seen (for diagnostics).
    pub fn syncs(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }
    /// Follow_Up messages seen.
    pub fn follow_ups(&self) -> u64 {
        self.follow_ups.load(Ordering::Relaxed)
    }
    /// Announce messages seen.
    pub fn announces(&self) -> u64 {
        self.announces.load(Ordering::Relaxed)
    }
    /// Syncs ignored because they came from a clock we do not follow.
    pub fn ignored(&self) -> u64 {
        self.ignored.load(Ordering::Relaxed)
    }
    /// `rt::mono_ns()` of the last offset sample (0 = none).
    pub fn last_sample_ns(&self) -> u64 {
        self.last_sample_ns.load(Ordering::Acquire)
    }
}

/// Message counters of a [`PtpTracker`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PtpCounters {
    pub syncs: u64,
    pub follow_ups: u64,
    pub announces: u64,
    pub ignored: u64,
}

#[derive(Clone, Copy)]
struct Pending {
    source: [u8; 10],
    seq: u16,
    t2: u64,
    correction: i64,
    order: u64,
}

/// The socket-less follower state machine (`PtpFollower` minus its sockets).
pub struct PtpTracker {
    clock: Arc<PtpClock>,
    master_ip: Option<Ipv4Addr>,
    expected_clock: Option<u64>,
    master_since_ns: u64,
    master_clock_id: u64,
    master_addr: Option<Ipv4Addr>,
    pending: [Option<Pending>; MAX_PENDING],
    next_order: u64,
    samples: VecDeque<(u64, i128)>,
    offset: Option<f64>,
    last_sample_ns: u64,
    last_delay_spread_ns: u64,
    counters: PtpCounters,
}

impl Default for PtpTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PtpTracker {
    /// A tracker that follows nobody until [`follow`](Self::follow) is called.
    pub fn new() -> Self {
        Self {
            clock: Arc::new(PtpClock::new()),
            master_ip: None,
            expected_clock: None,
            master_since_ns: 0,
            master_clock_id: 0,
            master_addr: None,
            pending: [None; MAX_PENDING],
            next_order: 0,
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            offset: None,
            last_sample_ns: 0,
            last_delay_spread_ns: 0,
            counters: PtpCounters::default(),
        }
    }

    /// The lock-free view this tracker publishes to.
    pub fn clock(&self) -> &Arc<PtpClock> {
        &self.clock
    }

    /// Start tracking the master of the receiver at `ip` (`now_ns` = `rt::mono_ns()`).
    /// An `expected_clock` of 0 counts as none. Any earlier choice of master and all
    /// samples are discarded, so every stream starts from a clean state.
    pub fn follow(&mut self, ip: Ipv4Addr, expected_clock: Option<u64>, now_ns: u64) {
        self.master_ip = Some(ip);
        self.expected_clock = expected_clock.filter(|&clock| clock != 0);
        self.master_since_ns = now_ns;
        self.master_clock_id = 0;
        self.master_addr = None;
        self.pending = [None; MAX_PENDING];
        self.samples.clear();
        self.offset = None;
        self.last_sample_ns = 0;
        self.last_delay_spread_ns = 0;
        self.clock
            .offset_bits
            .store(f64::NAN.to_bits(), Ordering::Release);
        self.clock.master_clock_id.store(0, Ordering::Release);
        self.clock.last_sample_ns.store(0, Ordering::Release);
        self.clock.delay_spread_ns.store(0, Ordering::Relaxed);
    }

    /// Expected master clock (after `follow`), if any.
    pub fn expected_clock(&self) -> Option<u64> {
        self.expected_clock
    }
    /// The clock identity being followed (0 = none chosen yet).
    pub fn master_clock_id(&self) -> u64 {
        self.master_clock_id
    }
    /// Address the chosen master's messages come from.
    pub fn master_addr(&self) -> Option<Ipv4Addr> {
        self.master_addr
    }
    /// Current offset estimate (master − local), ns.
    pub fn offset_ns(&self) -> Option<f64> {
        self.offset
    }
    /// See [`PtpClock::to_master_ns`].
    pub fn to_master_ns(&self, local_ns: u64) -> Option<u64> {
        self.offset.map(|offset| master_time_ns(local_ns, offset))
    }
    /// See [`PtpClock::locked_at`].
    pub fn locked_at(&self, now_ns: u64) -> bool {
        self.offset.is_some() && now_ns.saturating_sub(self.last_sample_ns) < STALE_AFTER_NS
    }
    /// Samples currently in the 4 s window.
    pub fn samples_in_window(&self) -> usize {
        self.samples.len()
    }
    /// Two-step Syncs waiting for their Follow_Up.
    pub fn pending(&self) -> usize {
        self.pending.iter().flatten().count()
    }
    /// Best minus worst sample of the window when the last sample was taken, ns.
    pub fn last_delay_spread_ns(&self) -> u64 {
        self.last_delay_spread_ns
    }
    pub fn counters(&self) -> PtpCounters {
        self.counters
    }

    /// Handle one received datagram (`t2` = `rt::mono_ns()` right after receive).
    /// Returns true when it produced an offset sample.
    pub fn on_datagram(&mut self, data: &[u8], from: Ipv4Addr, t2: u64) -> bool {
        if data.len() < MIN_MESSAGE_LEN || data[1] & 0x0F != 2 {
            return false;
        }
        let msg = data[0] & 0x0F;
        let mut source = [0u8; 10];
        source.copy_from_slice(&data[20..30]);
        let clock = u64::from_be_bytes(be_array(&data[20..28]));
        let seq = u16::from_be_bytes([data[30], data[31]]);
        let correction = i64::from_be_bytes(be_array(&data[8..16])) >> 16;

        match msg {
            MSG_SYNC => {
                self.counters.syncs += 1;
                self.clock
                    .syncs
                    .store(self.counters.syncs, Ordering::Relaxed);
                if !self.accept(clock, from, t2) {
                    self.counters.ignored += 1;
                    self.clock
                        .ignored
                        .store(self.counters.ignored, Ordering::Relaxed);
                    return false;
                }
                let flags = u16::from_be_bytes([data[6], data[7]]);
                if flags & FLAG_TWO_STEP != 0 {
                    self.add_pending(Pending {
                        source,
                        seq,
                        t2,
                        correction,
                        order: 0,
                    });
                    false
                } else {
                    let t1 = timestamp_ns(&data[34..44]);
                    let raw = t1 + i128::from(correction) - i128::from(t2);
                    self.add_sample(t2, raw, t2);
                    true
                }
            }
            MSG_FOLLOW_UP => {
                self.counters.follow_ups += 1;
                self.clock
                    .follow_ups
                    .store(self.counters.follow_ups, Ordering::Relaxed);
                let Some(sync) = self.take_pending(&source, seq) else {
                    return false;
                };
                let t1 = timestamp_ns(&data[34..44]);
                let raw =
                    t1 + i128::from(sync.correction) + i128::from(correction) - i128::from(sync.t2);
                self.add_sample(sync.t2, raw, t2);
                true
            }
            MSG_ANNOUNCE => {
                self.counters.announces += 1;
                self.clock
                    .announces
                    .store(self.counters.announces, Ordering::Relaxed);
                false
            }
            _ => false,
        }
    }

    fn accept(&mut self, clock: u64, from: Ipv4Addr, now_ns: u64) -> bool {
        if self.master_clock_id != 0 {
            return clock == self.master_clock_id;
        }
        let Some(master_ip) = self.master_ip else {
            return false;
        };
        let waited = now_ns.saturating_sub(self.master_since_ns) > FALLBACK_AFTER_NS;
        let chosen = match self.expected_clock {
            Some(expected) if clock == expected => true,
            Some(expected) => {
                if waited {
                    warn!(
                        "PTP: expected clock {expected:016x} is silent; following {clock:016x} \
                         from {from} instead"
                    );
                }
                waited
            }
            None => from == master_ip || waited,
        };
        if chosen {
            self.master_clock_id = clock;
            self.master_addr = Some(from);
            self.clock.master_clock_id.store(clock, Ordering::Release);
            info!("PTP: locked onto clock {clock:016x} at {from}");
        }
        chosen
    }

    fn add_pending(&mut self, mut entry: Pending) {
        // Same (source, seq) again: replace the value, keep its age (dict semantics).
        for slot in self.pending.iter_mut().flatten() {
            if slot.source == entry.source && slot.seq == entry.seq {
                slot.t2 = entry.t2;
                slot.correction = entry.correction;
                return;
            }
        }
        entry.order = self.next_order;
        self.next_order += 1;
        let index = match self.pending.iter().position(Option::is_none) {
            Some(free) => free,
            // Full: drop the oldest entry, like `_pending.pop(next(iter(...)))`.
            None => self
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, slot)| slot.map_or(u64::MAX, |p| p.order))
                .map_or(0, |(i, _)| i),
        };
        self.pending[index] = Some(entry);
    }

    fn take_pending(&mut self, source: &[u8; 10], seq: u16) -> Option<Pending> {
        self.pending
            .iter_mut()
            .find(|slot| slot.is_some_and(|p| p.source == *source && p.seq == seq))
            .and_then(Option::take)
    }

    fn add_sample(&mut self, t2: u64, raw_offset: i128, now_ns: u64) {
        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((t2, raw_offset));
        while let Some(&(oldest, _)) = self.samples.front() {
            if i128::from(t2) - i128::from(oldest) > i128::from(WINDOW_NS) {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        let mut best = raw_offset;
        let mut worst = raw_offset;
        for &(_, value) in &self.samples {
            best = best.max(value);
            worst = worst.min(value);
        }
        self.last_delay_spread_ns = u64::try_from(best - worst).unwrap_or(u64::MAX);
        let best_f = best as f64;
        let offset = match self.offset {
            Some(current) if (best_f - current).abs() <= STEP_NS => {
                current + SLEW * (best_f - current)
            }
            Some(current) => {
                info!(
                    "PTP: clock offset stepped by {:.2} ms",
                    (best_f - current) / 1e6
                );
                best_f
            }
            None => best_f,
        };
        self.offset = Some(offset);
        self.last_sample_ns = now_ns;
        self.clock
            .offset_bits
            .store(offset.to_bits(), Ordering::Release);
        self.clock.last_sample_ns.store(now_ns, Ordering::Release);
        self.clock
            .delay_spread_ns
            .store(self.last_delay_spread_ns, Ordering::Relaxed);
    }
}

fn be_array(bytes: &[u8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

/// 48-bit seconds + 32-bit nanoseconds → ns.
fn timestamp_ns(raw: &[u8]) -> i128 {
    let mut secs = [0u8; 8];
    secs[2..].copy_from_slice(&raw[0..6]);
    let nanos = u32::from_be_bytes([raw[6], raw[7], raw[8], raw[9]]);
    i128::from(u64::from_be_bytes(secs)) * 1_000_000_000 + i128::from(nanos)
}

/// Error starting the follower.
#[derive(Debug, thiserror::Error)]
pub enum PtpError {
    #[error("could not bind PTP sockets: {0}")]
    Bind(#[from] std::io::Error),
}

struct ServiceShared {
    tracker: Mutex<PtpTracker>,
    sampled: Condvar,
    stop: AtomicBool,
    wake: AutoEvent,
}

/// The running follower (sockets + receive thread). Stops when dropped.
pub struct PtpService {
    shared: Arc<ServiceShared>,
    clock: Arc<PtpClock>,
    port: u16,
    standard: bool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl PtpService {
    /// Bind UDP 319/320 on `local_ip` (falling back to one dynamic port when either
    /// is taken) and start the receive thread.
    pub fn start(local_ip: Ipv4Addr) -> Result<Arc<PtpService>, PtpError> {
        let (event, general) = bind_sockets(local_ip)?;
        let standard = general.is_some();
        let port = event.local_addr()?.port();
        let mut evented = vec![EventedSocket::new(event)?];
        if let Some(general) = general {
            evented.push(EventedSocket::new(general)?);
        }
        let tracker = PtpTracker::new();
        let clock = tracker.clock().clone();
        let shared = Arc::new(ServiceShared {
            tracker: Mutex::new(tracker),
            sampled: Condvar::new(),
            stop: AtomicBool::new(false),
            wake: AutoEvent::new().map_err(std::io::Error::other)?,
        });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("winpods-ptp".into())
            .spawn(move || receive_thread(&evented, &thread_shared))
            .map_err(|e| std::io::Error::other(format!("could not start the PTP thread: {e}")))?;
        debug!("PTP: listening on {local_ip}:{port} (standard ports: {standard})");
        Ok(Arc::new(PtpService {
            shared,
            clock,
            port,
            standard,
            thread: Mutex::new(Some(thread)),
        }))
    }
    /// Local port the receiver must send to (319 when the standard ports are ours).
    pub fn port(&self) -> u16 {
        self.port
    }
    /// True when bound to the standard ports 319/320.
    pub fn standard_ports(&self) -> bool {
        self.standard
    }
    /// Start tracking the master of the receiver at `ip`; `expected_clock` is
    /// `timingPeerInfo.ClockID` from the base SETUP response.
    pub fn follow(&self, ip: Ipv4Addr, expected_clock: Option<u64>) {
        self.shared
            .tracker
            .lock()
            .follow(ip, expected_clock, mono_ns());
        match expected_clock.filter(|&c| c != 0) {
            Some(clock) => info!(
                "PTP: following {ip} (expected clock {clock:016x}, listening on port {})",
                self.port
            ),
            None => info!(
                "PTP: following {ip} (expected clock any, listening on port {})",
                self.port
            ),
        }
    }
    /// Block until the first offset sample (or timeout). Returns true when locked.
    pub fn wait_locked(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut tracker = self.shared.tracker.lock();
        while tracker.offset_ns().is_none() {
            match deadline {
                Some(deadline) => {
                    if self
                        .shared
                        .sampled
                        .wait_until(&mut tracker, deadline)
                        .timed_out()
                    {
                        return tracker.offset_ns().is_some();
                    }
                }
                None => self.shared.sampled.wait(&mut tracker),
            }
        }
        true
    }
    /// The shared clock view for the anchor thread.
    pub fn clock(&self) -> Arc<PtpClock> {
        self.clock.clone()
    }
    /// Message counters (diagnostics).
    pub fn counters(&self) -> PtpCounters {
        self.shared.tracker.lock().counters()
    }
}

impl Drop for PtpService {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.set();
        if let Some(thread) = self.thread.lock().take()
            && thread.join().is_err()
        {
            warn!("PTP receive thread panicked");
        }
    }
}

/// Bind 319 (event) + 320 (general), or one dynamic port used for both message
/// classes (then the general socket is `None`).
fn bind_sockets(local_ip: Ipv4Addr) -> std::io::Result<(UdpSocket, Option<UdpSocket>)> {
    let standard = UdpSocket::bind((local_ip, PTP_EVENT_PORT)).and_then(|event| {
        UdpSocket::bind((local_ip, PTP_GENERAL_PORT)).map(|general| (event, general))
    });
    match standard {
        Ok((event, general)) => Ok((event, Some(general))),
        Err(e) => {
            warn!(
                "UDP ports 319/320 are in use ({e}), probably by another WinPods instance or a \
                 PTP program; using a dynamic port, which some receivers reject"
            );
            Ok((UdpSocket::bind((local_ip, 0))?, None))
        }
    }
}

fn receive_thread(sockets: &[EventedSocket], shared: &ServiceShared) {
    set_current_thread_priority(ThreadPriority::TimeCritical);
    let mut handles = [shared.wake.handle(); 3];
    for (slot, socket) in handles[1..].iter_mut().zip(sockets) {
        *slot = socket.handle();
    }
    let handles = &handles[..1 + sockets.len().min(2)];
    let mut buf = [0u8; 2048];
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        while !shared.stop.load(Ordering::Acquire) {
            if let Err(e) = wait_any(handles, Some(Duration::from_millis(500))) {
                warn!("PTP: wait failed: {e}");
                return;
            }
            for socket in sockets {
                drain(socket, &mut buf, shared);
            }
        }
    }));
    if outcome.is_err() {
        warn!("PTP receive thread panicked; the clock will go stale");
    }
}

/// Read every queued datagram from `socket`, timestamping each right after receive.
fn drain(socket: &EventedSocket, buf: &mut [u8], shared: &ServiceShared) {
    loop {
        match socket.socket().recv_from(buf) {
            Ok((len, from)) => {
                let t2 = mono_ns();
                let SocketAddr::V4(from) = from else {
                    continue;
                };
                let mut tracker = shared.tracker.lock();
                if tracker.on_datagram(&buf[..len], *from.ip(), t2) {
                    shared.sampled.notify_all();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionReset
                ) => {}
            // e.g. an oversized datagram; the event fires again if more are queued.
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn ip(value: &Value) -> Ipv4Addr {
        value.as_str().unwrap().parse().unwrap()
    }

    fn check_case(case: &Value) {
        let name = case["name"].as_str().unwrap();
        let mut tracker = PtpTracker::new();
        let clock = tracker.clock().clone();
        for (i, event) in case["events"].as_array().unwrap().iter().enumerate() {
            let ctx = format!("{name} event {i}");
            match event["kind"].as_str().unwrap() {
                "follow" => {
                    let mono_ns = (event["mono"].as_f64().unwrap() * 1e9).round() as u64;
                    tracker.follow(ip(&event["ip"]), event["clock_id"].as_u64(), mono_ns);
                    assert_eq!(
                        tracker.expected_clock(),
                        event["expected_clock"].as_u64(),
                        "{ctx}"
                    );
                }
                "datagram" => {
                    let data = hex::decode(event["datagram"].as_str().unwrap()).unwrap();
                    let t2 = event["t2"].as_u64().unwrap();
                    let note = event["note"].as_str().unwrap();
                    let ctx = format!("{ctx} ({note})");
                    tracker.on_datagram(&data, ip(&event["from"]), t2);

                    match event["offset_after_bits"].as_str() {
                        Some(bits) => {
                            let bits = u64::from_str_radix(bits, 16).unwrap();
                            assert_eq!(tracker.offset_ns().map(f64::to_bits), Some(bits), "{ctx}");
                            assert_eq!(clock.offset_ns().map(f64::to_bits), Some(bits), "{ctx}");
                        }
                        None => {
                            assert_eq!(tracker.offset_ns(), None, "{ctx}");
                            assert_eq!(clock.offset_ns(), None, "{ctx}");
                        }
                    }
                    let master = event["master_clock_id"].as_u64().unwrap();
                    assert_eq!(tracker.master_clock_id(), master, "{ctx}");
                    assert_eq!(clock.master_clock_id(), master, "{ctx}");
                    let addr = event["master_addr"].as_str().unwrap();
                    assert_eq!(
                        tracker
                            .master_addr()
                            .map(|a| a.to_string())
                            .unwrap_or_default(),
                        addr,
                        "{ctx}"
                    );
                    let locked = event["locked"].as_bool().unwrap();
                    assert_eq!(tracker.locked_at(t2), locked, "{ctx}");
                    assert_eq!(clock.locked_at(t2), locked, "{ctx}");
                    let to_master = event["to_master_ns_at_t2"].as_u64();
                    assert_eq!(tracker.to_master_ns(t2), to_master, "{ctx}");
                    assert_eq!(clock.to_master_ns(t2), to_master, "{ctx}");
                    assert_eq!(
                        tracker.samples_in_window() as u64,
                        event["samples_in_window"].as_u64().unwrap(),
                        "{ctx}"
                    );
                    assert_eq!(
                        tracker.pending() as u64,
                        event["pending"].as_u64().unwrap(),
                        "{ctx}"
                    );
                    let spread = event["last_delay_spread_ns"].as_u64().unwrap();
                    assert_eq!(tracker.last_delay_spread_ns(), spread, "{ctx}");
                    assert_eq!(clock.delay_spread_ns(), spread, "{ctx}");
                    let c = &event["counters"];
                    let expected = PtpCounters {
                        syncs: c["syncs"].as_u64().unwrap(),
                        follow_ups: c["follow_ups"].as_u64().unwrap(),
                        announces: c["announces"].as_u64().unwrap(),
                        ignored: c["ignored"].as_u64().unwrap(),
                    };
                    assert_eq!(tracker.counters(), expected, "{ctx}");
                    assert_eq!(clock.syncs(), expected.syncs, "{ctx}");
                    assert_eq!(clock.follow_ups(), expected.follow_ups, "{ctx}");
                    assert_eq!(clock.announces(), expected.announces, "{ctx}");
                    assert_eq!(clock.ignored(), expected.ignored, "{ctx}");
                }
                other => panic!("unknown event kind {other}"),
            }
        }
    }

    #[test]
    fn fixture_cases_match() {
        let fx: Value = serde_json::from_str(include_str!("../tests/fixtures/ptp.json")).unwrap();
        let constants = &fx["constants"];
        assert_eq!(constants["window_ns"].as_u64(), Some(WINDOW_NS));
        assert_eq!(constants["step_ns"].as_f64(), Some(STEP_NS));
        assert_eq!(constants["slew"].as_f64(), Some(SLEW));
        assert_eq!(
            constants["assumed_path_delay_ns"].as_i64(),
            Some(ASSUMED_PATH_DELAY_NS)
        );
        assert_eq!(constants["max_pending"].as_u64(), Some(MAX_PENDING as u64));
        let cases = fx["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 4);
        for case in cases {
            check_case(case);
        }
    }

    fn sync(seq: u16, clock: u64, two_step: bool, origin_ns: u64) -> Vec<u8> {
        let mut data = vec![0u8; 44];
        data[0] = MSG_SYNC;
        data[1] = 2;
        data[2..4].copy_from_slice(&44u16.to_be_bytes());
        if two_step {
            data[6..8].copy_from_slice(&FLAG_TWO_STEP.to_be_bytes());
        }
        data[20..28].copy_from_slice(&clock.to_be_bytes());
        data[28..30].copy_from_slice(&1u16.to_be_bytes());
        data[30..32].copy_from_slice(&seq.to_be_bytes());
        let secs = origin_ns / 1_000_000_000;
        data[34..40].copy_from_slice(&secs.to_be_bytes()[2..]);
        data[40..44].copy_from_slice(&((origin_ns % 1_000_000_000) as u32).to_be_bytes());
        data
    }

    #[test]
    fn pending_table_evicts_oldest_beyond_64() {
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        let mut tracker = PtpTracker::new();
        tracker.follow(ip, Some(7), 0);
        for seq in 0..70u16 {
            tracker.on_datagram(&sync(seq, 7, true, 0), ip, 1_000 + u64::from(seq));
        }
        assert_eq!(tracker.pending(), MAX_PENDING);
        // Seqs 0..6 were evicted; their Follow_Ups find nothing.
        let mut fup = sync(3, 7, false, 5_000_000_000);
        fup[0] = MSG_FOLLOW_UP;
        assert!(!tracker.on_datagram(&fup, ip, 2_000));
        let mut fup = sync(69, 7, false, 5_000_000_000);
        fup[0] = MSG_FOLLOW_UP;
        assert!(tracker.on_datagram(&fup, ip, 2_000));
        assert_eq!(tracker.pending(), MAX_PENDING - 1);
        // A repeated (source, seq) replaces the value without growing the table.
        tracker.on_datagram(&sync(10, 7, true, 0), ip, 3_000);
        tracker.on_datagram(&sync(10, 7, true, 0), ip, 3_001);
        assert_eq!(tracker.pending(), MAX_PENDING - 1);
    }

    #[test]
    fn follow_resets_master_and_samples() {
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        let mut tracker = PtpTracker::new();
        tracker.follow(ip, None, 0);
        assert!(tracker.on_datagram(&sync(1, 9, false, 10_000_000_000), ip, 1_000));
        assert_eq!(tracker.master_clock_id(), 9);
        assert!(tracker.clock().locked_at(1_000));
        tracker.follow(ip, Some(5), 2_000);
        assert_eq!(tracker.master_clock_id(), 0);
        assert_eq!(tracker.clock().offset_ns(), None);
        assert!(!tracker.clock().locked_at(2_000));
        assert_eq!(tracker.counters().syncs, 1, "counters are cumulative");
    }

    #[test]
    fn master_time_masks_like_python() {
        assert_eq!(
            master_time_ns(50_000_000_000, -1_234_567_890.75),
            48_766_432_109
        );
        // Negative master times wrap to u64 like `& U64`.
        assert_eq!(master_time_ns(0, -2_000_000.0), (-1_000_000i64) as u64);
    }
}
