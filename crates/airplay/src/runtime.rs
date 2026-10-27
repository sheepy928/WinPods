//! Starts and stops the real-time part of a stream (workstream: real-time).
//! CONTRACT: used by the engine; keep the public signatures.
//!
//! `start` (step 15 of the session sequence): clear the ring, start the timeline at
//! `(rt::mono_ns(), params.first_ts)`, spawn the control thread (sends the first
//! anchor immediately: 0xD7 with PTP, 0xD4 with NTP), then spawn the sender thread
//! whose first packet uses `params.first_seq` / `params.first_ts`. While a stream
//! runs, hold an `rt::ThrottleGuard`. `stop` joins both threads and hands the ring
//! consumer back so the next session reuses it.
//!
//! Threads:
//!
//! * `winpods-control` (above normal): anchors every second, retransmit replies
//!   ([`crate::control`]). The first anchor is sent by `start` itself, before the
//!   sender exists, so the receiver always has it before the first audio packet.
//! * `winpods-sender` (MMCSS "Pro Audio"): ring → packets ([`crate::sender`]).
//!
//! Panics and fatal errors in either thread are caught and reported through
//! [`StreamHandle::end_reason`], as is a followed PTP clock that stays unlocked for
//! longer than [`crate::control::PTP_LOST_AFTER_NS`]; the supervisor then stops the
//! stream and reconnects.

use std::any::Any;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::thread::JoinHandle;

use parking_lot::Mutex;
use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, info, warn};
use winpods_rt::power::ThrottleOptOut;
use winpods_rt::{
    AutoEvent, HiResTimer, MmcssGuard, RingConsumer, ThreadPriority, ThrottleGuard, mono_ns,
    set_current_thread_priority,
};

use crate::backlog::Backlog;
use crate::control::{AnchorClock, ControlLoop, EventedSocket};
use crate::ntp::NtpClock;
use crate::ptp::PtpClock;
use crate::sender::{Sender, SenderConfig, StallPolicy};
use crate::timeline::{SharedTimeline, Timeline};
use crate::types::StreamParams;

/// Send buffer of the audio data socket.
pub const DATA_SNDBUF_BYTES: usize = 1 << 20;

/// The local control socket. Bound before SETUP because its port goes into the
/// stream SETUP request (`controlPort`).
pub struct ControlSocket {
    socket: UdpSocket,
    port: u16,
}

impl ControlSocket {
    pub fn bind(local_ip: Ipv4Addr) -> std::io::Result<ControlSocket> {
        let socket = UdpSocket::bind((local_ip, 0))?;
        let port = socket.local_addr()?.port();
        Ok(ControlSocket { socket, port })
    }
    pub fn port(&self) -> u16 {
        self.port
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
    /// Frames waiting in the ring (sampled by the sender each loop).
    pub ring_frames: u64,
    /// `rt::mono_ns()` when the stream started.
    pub started_ns: u64,
}

/// Lock-free counters behind [`StreamStats`].
#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) packets: AtomicU64,
    pub(crate) silence_packets: AtomicU64,
    pub(crate) frames_trimmed: AtomicU64,
    pub(crate) stalls: AtomicU64,
    pub(crate) retransmit_requests: AtomicU64,
    pub(crate) retransmitted: AtomicU64,
    pub(crate) anchors: AtomicU64,
    pub(crate) send_errors: AtomicU64,
    pub(crate) withheld: AtomicU64,
    pub(crate) ring_frames: AtomicU64,
    pub(crate) started_ns: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> StreamStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        StreamStats {
            packets: get(&self.packets),
            silence_packets: get(&self.silence_packets),
            frames_trimmed: get(&self.frames_trimmed),
            stalls: get(&self.stalls),
            retransmit_requests: get(&self.retransmit_requests),
            retransmitted: get(&self.retransmitted),
            anchors: get(&self.anchors),
            send_errors: get(&self.send_errors),
            withheld: get(&self.withheld),
            ring_frames: get(&self.ring_frames),
            started_ns: get(&self.started_ns),
        }
    }
}

/// State shared by the stream's threads and its handle.
pub(crate) struct Shared {
    pub(crate) stop: AtomicBool,
    pub(crate) stats: Counters,
    pub(crate) timeline: SharedTimeline,
    pub(crate) backlog: Backlog,
    pub(crate) sender_wake: AutoEvent,
    pub(crate) control_wake: AutoEvent,
    end_reason: Mutex<Option<String>>,
}

impl Shared {
    pub(crate) fn new(sender_wake: AutoEvent, control_wake: AutoEvent) -> Self {
        Self {
            stop: AtomicBool::new(false),
            stats: Counters::default(),
            timeline: SharedTimeline::new(),
            backlog: Backlog::new(),
            sender_wake,
            control_wake,
            end_reason: Mutex::new(None),
        }
    }

    /// Record why the stream cannot continue (the first reason wins).
    pub(crate) fn fail(&self, reason: String) {
        let mut slot = self.end_reason.lock();
        if slot.is_none() {
            *slot = Some(reason);
        }
    }

    pub(crate) fn end_reason(&self) -> Option<String> {
        self.end_reason.lock().clone()
    }
}

type ConsumerSlot = Arc<Mutex<Option<RingConsumer>>>;

/// A running stream. Dropping it without `stop` also stops the threads.
pub struct StreamHandle {
    shared: Arc<Shared>,
    sender: Option<JoinHandle<()>>,
    control: Option<JoinHandle<()>>,
    consumer: ConsumerSlot,
    _throttle: ThrottleGuard,
}

impl StreamHandle {
    pub fn stats(&self) -> StreamStats {
        self.shared.stats.snapshot()
    }
    /// Set when a thread failed or panicked, or the followed PTP clock was lost (the
    /// supervisor then reconnects).
    pub fn end_reason(&self) -> Option<String> {
        self.shared.end_reason()
    }
    /// Stop both threads and return the ring consumer.
    pub fn stop(mut self) -> RingConsumer {
        self.shutdown();
        reclaim(&self.consumer)
    }

    fn shutdown(&mut self) {
        stop_threads(&self.shared, self.control.take(), self.sender.take());
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn stop_threads(shared: &Shared, control: Option<JoinHandle<()>>, sender: Option<JoinHandle<()>>) {
    shared.stop.store(true, Ordering::Release);
    shared.control_wake.set();
    shared.sender_wake.set();
    // Both thread bodies catch their own panics; a join error is still reported.
    if let Some(control) = control
        && control.join().is_err()
    {
        shared.fail("control thread panicked".into());
    }
    if let Some(sender) = sender
        && sender.join().is_err()
    {
        shared.fail("sender thread panicked".into());
    }
}

/// Take the consumer back out of its slot.
fn reclaim(slot: &Mutex<Option<RingConsumer>>) -> RingConsumer {
    // Invariant: the slot is filled before the sender thread is spawned, the thread
    // only borrows it, and it is emptied exactly once — here, by `stop` (which
    // consumes the handle) or by `start` after a failed spawn.
    slot.lock()
        .take()
        .expect("ring consumer slot is filled until the stream is stopped")
}

/// Start the control and sender threads for a negotiated stream. On failure the ring
/// consumer is handed back so the engine can retry with the same ring.
pub fn start(
    params: &StreamParams,
    control: ControlSocket,
    consumer: RingConsumer,
    options: RuntimeOptions,
) -> Result<StreamHandle, (std::io::Error, RingConsumer)> {
    let resources = match Resources::open(params, control) {
        Ok(resources) => resources,
        Err(e) => return Err((e, consumer)),
    };
    let Resources {
        data,
        control,
        sender_wake,
        control_wake,
        timer,
    } = resources;

    let shared = Arc::new(Shared::new(sender_wake, control_wake));
    let clock = match (options.ptp, options.ntp) {
        (Some(ptp), _) => AnchorClock::Ptp(ptp),
        (None, Some(ntp)) => AnchorClock::Ntp(ntp),
        (None, None) => AnchorClock::Ntp(Arc::new(NtpClock::new())),
    };
    let dest = SocketAddr::from((params.receiver, params.control_port));
    let mut control_loop =
        ControlLoop::new(control, dest, clock, params.latency_frames, params.first_ts);
    let config = SenderConfig {
        codec: params.codec,
        ssrc: params.ssrc,
        first_seq: params.first_seq,
        first_ts: params.first_ts,
        shk: params.shk,
        policy: StallPolicy::new(params.latency_ms, params.render_ms),
        drop_every: options.drop_every.unwrap_or(0),
    };
    let throttle = ThrottleGuard::streaming();
    log_throttle_opt_out(&throttle);

    // Audio captured while connecting is stale; starting with it would add its
    // duration to the latency for the whole session.
    let mut consumer = consumer;
    consumer.clear();
    let started_ns = mono_ns();
    let mut timeline = Timeline::new();
    timeline.start(started_ns, params.first_ts);
    shared.timeline.publish(&timeline);
    shared.stats.started_ns.store(started_ns, Ordering::Relaxed);

    // First anchor before any audio (0x90 marker); retried by the thread if the
    // PTP master is not known yet.
    control_loop.send_anchor(&shared);

    let control_shared = shared.clone();
    let control_thread = match std::thread::Builder::new()
        .name("winpods-control".into())
        .spawn(move || control_thread(control_loop, &control_shared))
    {
        Ok(thread) => thread,
        Err(e) => return Err((e, consumer)),
    };

    let sender = Sender::new(data, &config, timeline);
    let slot: ConsumerSlot = Arc::new(Mutex::new(Some(consumer)));
    let sender_slot = slot.clone();
    let sender_shared = shared.clone();
    let sender_thread = match std::thread::Builder::new()
        .name("winpods-sender".into())
        .spawn(move || sender_thread(sender, &sender_slot, &sender_shared, &timer))
    {
        Ok(thread) => thread,
        Err(e) => {
            stop_threads(&shared, Some(control_thread), None);
            return Err((e, reclaim(&slot)));
        }
    };

    info!(
        "Streaming to {}:{} with {} ms latency ({}, seq {}, ts {})",
        params.receiver,
        params.data_port,
        params.latency_ms,
        params.codec.as_str(),
        params.first_seq,
        params.first_ts
    );
    Ok(StreamHandle {
        shared,
        sender: Some(sender_thread),
        control: Some(control_thread),
        consumer: slot,
        _throttle: throttle,
    })
}

/// Sockets, events and the timer a stream needs, created before anything starts so
/// a failure leaves nothing behind.
struct Resources {
    data: UdpSocket,
    control: EventedSocket,
    sender_wake: AutoEvent,
    control_wake: AutoEvent,
    timer: HiResTimer,
}

impl Resources {
    fn open(params: &StreamParams, control: ControlSocket) -> io::Result<Self> {
        Ok(Self {
            data: open_data_socket(params)?,
            control: EventedSocket::new(control.socket)?,
            sender_wake: AutoEvent::new().map_err(io::Error::other)?,
            control_wake: AutoEvent::new().map_err(io::Error::other)?,
            timer: HiResTimer::new().map_err(io::Error::other)?,
        })
    }
}

/// UDP socket bound to the local IP, connected to the receiver's data port, with a
/// 1 MiB send buffer.
fn open_data_socket(params: &StreamParams) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.bind(&SocketAddr::from((params.local_ip, 0)).into())?;
    socket.set_send_buffer_size(DATA_SNDBUF_BYTES)?;
    socket.connect(&SocketAddr::from((params.receiver, params.data_port)).into())?;
    Ok(socket.into())
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(text) = payload.downcast_ref::<&str>() {
        text
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text
    } else {
        "unknown panic"
    }
}

/// Log once per process what Windows accepted of the power-throttling opt-out.
fn log_throttle_opt_out(guard: &ThrottleGuard) {
    static LOGGED: Once = Once::new();
    // `None` only while another thread is still applying it; a later stream logs.
    let Some(opt_out) = guard.opt_out() else {
        return;
    };
    LOGGED.call_once(|| match opt_out {
        ThrottleOptOut::Full => {
            debug!("power throttling: EcoQoS off and timer resolution honoured while streaming");
        }
        ThrottleOptOut::ExecutionSpeedOnly { hresult } => info!(
            "power throttling: EcoQoS off while streaming; this Windows has no \
             timer-resolution policy ({hresult:#010x})"
        ),
        ThrottleOptOut::Failed { hresult } => warn!(
            "Windows refused the power-throttling opt-out ({hresult:#010x}); audio may \
             stutter while WinPods runs in the background"
        ),
    });
}

/// Join the MMCSS "Pro Audio" task for the calling thread. A refusal is logged here,
/// before the thread's loop starts: as a warning the first time, then at debug level.
fn join_mmcss(thread: &str) -> Option<MmcssGuard> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let guard = MmcssGuard::pro_audio();
    if guard.is_none() {
        // AvSetMmThreadCharacteristicsW leaves the reason in the thread's last error.
        let error = io::Error::last_os_error();
        let detail = match error.raw_os_error() {
            Some(0) | None => String::new(),
            Some(_) => format!(" ({error})"),
        };
        if WARNED.swap(true, Ordering::Relaxed) {
            debug!("{thread} thread: MMCSS \"Pro Audio\" registration failed{detail}");
        } else {
            warn!(
                "{thread} thread: MMCSS \"Pro Audio\" registration failed{detail}; it runs at \
                 normal priority"
            );
        }
    }
    guard
}

fn control_thread(mut control: ControlLoop, shared: &Shared) {
    set_current_thread_priority(ThreadPriority::AboveNormal);
    match catch_unwind(AssertUnwindSafe(|| {
        control.run(shared, &shared.control_wake)
    })) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => shared.fail(format!("control thread failed: {e}")),
        Err(payload) => shared.fail(format!(
            "control thread panicked: {}",
            panic_message(payload.as_ref())
        )),
    }
    debug!("control thread stopped");
}

fn sender_thread(
    mut sender: Sender,
    slot: &Mutex<Option<RingConsumer>>,
    shared: &Shared,
    timer: &HiResTimer,
) {
    let _mmcss = join_mmcss("sender");
    // Held for the whole stream; nobody else locks the slot until the thread exits
    // (and parking_lot does not poison it if the loop panics).
    let mut guard = slot.lock();
    let Some(consumer) = guard.as_mut() else {
        shared.fail("sender thread started without the ring".into());
        return;
    };
    match catch_unwind(AssertUnwindSafe(|| sender.run(consumer, shared, timer))) {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => shared.fail(reason),
        Err(payload) => shared.fail(format!(
            "sender panicked: {}",
            panic_message(payload.as_ref())
        )),
    }
    drop(guard);
    debug!("sender thread stopped");
}
