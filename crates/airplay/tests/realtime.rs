//! Integration tests for the AirPlay real-time path (workstream b): the runtime's
//! sender and control threads, the PTP follower service and the NTP timing responder,
//! all against loopback mock peers on 127.0.0.1. Nothing here contacts a real device.
//!
//! Streaming tests are timing sensitive, so they run one at a time (`SERIAL`).

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use winpods_airplay::alac::decode_alac;
use winpods_airplay::ntp::{NtpClock, TimingResponder, unix_ns_to_ntp};
use winpods_airplay::packet::open_packet;
use winpods_airplay::ptp::{
    FLAG_TWO_STEP, MSG_FOLLOW_UP, MSG_SYNC, PtpClock, PtpService, PtpTracker,
};
use winpods_airplay::runtime::{self, ControlSocket, RuntimeOptions, StreamHandle, StreamStats};
use winpods_airplay::{Codec, FRAMES_PER_PACKET, StreamParams, TimingProtocol, latency_frames};
use winpods_rt::{Frame, RingConsumer, RingProducer, mono_ns, ring};

const LOCAL: Ipv4Addr = Ipv4Addr::LOCALHOST;
const SHK: [u8; 32] = [
    0x54, 0x31, 0x70, 0xdf, 0xa8, 0x02, 0x44, 0x2a, 0xe8, 0x50, 0x58, 0x9e, 0xfe, 0x62, 0xd9, 0x84,
    0xa1, 0xcd, 0x45, 0xb0, 0x93, 0xbf, 0x0a, 0xe2, 0x1d, 0xd6, 0x26, 0xbf, 0x78, 0x52, 0xe2, 0x31,
];
const CLOCK_ID: u64 = 0x02AA_BBFF_FECC_DDEE;
/// Master clock = local monotonic clock + ~11.6 days.
const PTP_OFFSET_NS: u64 = 1_000_000_000_000_000;
const SSRC: u32 = 0x1234_5678;
/// Close to the u16 wrap so continuity checks cross it.
const FIRST_SEQ: u16 = 65_500;
const CHUNK: usize = 220;
const MS: u64 = 1_000_000;

static SERIAL: Mutex<()> = Mutex::new(());
/// The PTP tests compete for UDP 319/320.
static PTP_SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn ptp_serial() -> MutexGuard<'static, ()> {
    PTP_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ------------------------------------------------------------------ mock receiver

/// Datagrams with their `mono_ns` arrival times.
type Log = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

/// Records every datagram arriving on a socket with its `mono_ns` arrival time.
struct Capture {
    log: Log,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    fn new(socket: UdpSocket) -> Self {
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (log, stop) = (log.clone(), stop.clone());
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while !stop.load(Ordering::Relaxed) {
                    // Timeouts and ICMP-induced resets are expected; keep listening.
                    if let Ok((len, _)) = socket.recv_from(&mut buf) {
                        let arrival = mono_ns();
                        log.lock().unwrap().push((arrival, buf[..len].to_vec()));
                    }
                }
            })
        };
        Self {
            log,
            stop,
            thread: Some(thread),
        }
    }

    fn snapshot(&self) -> Vec<(u64, Vec<u8>)> {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The receiver end of a stream: a data socket and a control socket.
struct MockReceiver {
    data: Capture,
    control: Capture,
    control_tx: UdpSocket,
    data_port: u16,
    control_port: u16,
}

impl MockReceiver {
    fn new() -> Self {
        let data = UdpSocket::bind((LOCAL, 0)).unwrap();
        let control = UdpSocket::bind((LOCAL, 0)).unwrap();
        let data_port = data.local_addr().unwrap().port();
        let control_port = control.local_addr().unwrap().port();
        let control_tx = control.try_clone().unwrap();
        Self {
            data: Capture::new(data),
            control: Capture::new(control),
            control_tx,
            data_port,
            control_port,
        }
    }

    fn params(&self, codec: Codec, latency_ms: u32) -> StreamParams {
        let frames = latency_frames(latency_ms);
        StreamParams {
            receiver: LOCAL,
            local_ip: LOCAL,
            data_port: self.data_port,
            control_port: self.control_port,
            shk: SHK,
            codec,
            timing: TimingProtocol::Ptp,
            latency_ms,
            latency_frames: frames,
            render_ms: Some(69),
            clock_id: Some(CLOCK_ID),
            ssrc: SSRC,
            first_seq: FIRST_SEQ,
            first_ts: u64::from(frames),
        }
    }

    fn audio(&self, codec: Codec) -> Vec<AudioPacket> {
        self.data
            .snapshot()
            .into_iter()
            .map(|(arrival, raw)| AudioPacket::decode(arrival, raw, codec))
            .collect()
    }

    /// Control datagrams whose second byte is `kind` (0xD7, 0xD4, 0xD6).
    fn control_packets(&self, kind: u8) -> Vec<(u64, Vec<u8>)> {
        self.control
            .snapshot()
            .into_iter()
            .filter(|(_, raw)| raw.len() >= 2 && raw[1] == kind)
            .collect()
    }

    fn request_retransmit(&self, our_control_port: u16, first: u16, count: u16) {
        let mut request = vec![0x80, 0xD5, 0x00, 0x01];
        request.extend_from_slice(&first.to_be_bytes());
        request.extend_from_slice(&count.to_be_bytes());
        self.control_tx
            .send_to(&request, (LOCAL, our_control_port))
            .unwrap();
    }
}

/// A decrypted, decoded audio packet as the receiver saw it.
struct AudioPacket {
    arrival: u64,
    marker: u8,
    seq: u16,
    ts: u32,
    ssrc: u32,
    counter: u64,
    frames: Vec<Frame>,
    raw: Vec<u8>,
}

impl AudioPacket {
    fn decode(arrival: u64, raw: Vec<u8>, codec: Codec) -> Self {
        let mut plain = [0u8; 2048];
        let len = open_packet(&SHK, &raw, &mut plain).expect("packet decrypts with shk");
        let frames = match codec {
            Codec::Alac => {
                assert_eq!(len, 1416);
                let mut frames = vec![[0i16; 2]; FRAMES_PER_PACKET];
                assert_eq!(
                    decode_alac(&plain[..len], &mut frames),
                    Some(FRAMES_PER_PACKET)
                );
                frames
            }
            Codec::Pcm => {
                assert_eq!(len, 1408);
                plain[..len]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&[l0, l1, r0, r1]| {
                        [i16::from_be_bytes([l0, l1]), i16::from_be_bytes([r0, r1])]
                    })
                    .collect()
            }
        };
        Self {
            arrival,
            marker: raw[1],
            seq: u16::from_be_bytes([raw[2], raw[3]]),
            ts: u32::from_be_bytes(raw[4..8].try_into().unwrap()),
            ssrc: u32::from_be_bytes(raw[8..12].try_into().unwrap()),
            counter: u64::from_le_bytes(raw[raw.len() - 8..].try_into().unwrap()),
            frames,
            raw,
        }
    }

    fn is_silence(&self) -> bool {
        self.frames.iter().all(|f| *f == [0, 0])
    }

    /// Unwrapped head after this packet, given the stream's first timestamp.
    fn head_after(&self, index: usize, first_ts: u64) -> u64 {
        first_ts + (index as u64 + 1) * FRAMES_PER_PACKET as u64
    }
}

/// Check header continuity of a run of received packets with nothing missing.
fn assert_continuous(packets: &[AudioPacket], params: &StreamParams) {
    assert!(!packets.is_empty(), "no packets received");
    for (k, p) in packets.iter().enumerate() {
        assert_eq!(
            p.seq,
            params.first_seq.wrapping_add(k as u16),
            "seq of packet {k}"
        );
        assert_eq!(
            p.ts,
            (params.first_ts + (k * FRAMES_PER_PACKET) as u64) as u32,
            "ts of packet {k}"
        );
        assert_eq!(
            p.marker,
            if k == 0 { 0xE0 } else { 0x60 },
            "marker of packet {k}"
        );
        assert_eq!(p.ssrc, params.ssrc);
        assert_eq!(p.counter, k as u64, "nonce counter of packet {k}");
        assert_eq!(p.raw[0], 0x80);
    }
}

// --------------------------------------------------------------------- audio feed

/// Non-zero test signal (so silence is recognisable): frame i = [v, -v].
fn signal(i: u64) -> Frame {
    let v = (i % 30_000) as i16 + 1;
    [v, -v]
}

/// Pushes the test signal into the ring in 220-frame chunks.
struct Feeder {
    producer: RingProducer,
    next: u64,
}

impl Feeder {
    fn new(producer: RingProducer) -> Self {
        Self { producer, next: 0 }
    }

    fn push(&mut self, frames: usize) {
        let chunk: Vec<Frame> = (0..frames as u64).map(|k| signal(self.next + k)).collect();
        assert_eq!(self.producer.push(&chunk), frames, "ring overflow");
        self.next += frames as u64;
    }

    /// Deliver audio at exactly 44.1 kHz on average for `duration`, in 220-frame
    /// chunks, sleeping `gaps_ms[i]` between deliveries (Windows loopback is bursty).
    fn feed(&mut self, duration: Duration, gaps_ms: &[u64]) {
        let t0 = mono_ns();
        let base = self.next;
        let total = duration.as_nanos() as u64;
        let mut i = 0;
        loop {
            let elapsed = (mono_ns() - t0).min(total);
            let due = (u128::from(elapsed) * 44_100 / 1_000_000_000) as u64;
            while self.next - base + CHUNK as u64 <= due {
                self.push(CHUNK);
            }
            if elapsed >= total {
                return;
            }
            thread::sleep(Duration::from_millis(gaps_ms[i % gaps_ms.len()]));
            i += 1;
        }
    }

    /// Push `count` packets of 352 frames, one at a time at the stream rate (the
    /// first one immediately).
    fn push_packets_paced(&mut self, count: usize) {
        let t0 = mono_ns();
        for k in 0..count as u64 {
            let due = t0 + k * FRAMES_PER_PACKET as u64 * 1_000_000_000 / 44_100;
            while mono_ns() < due {
                thread::sleep(Duration::from_micros(500));
            }
            self.push(FRAMES_PER_PACKET);
        }
    }

    fn last_write_ns(&self) -> u64 {
        self.producer.shared().last_write_ns()
    }
}

fn new_ring() -> (Feeder, RingConsumer) {
    let (producer, consumer) = ring(winpods_rt::ring::DEFAULT_CAPACITY).unwrap();
    (Feeder::new(producer), consumer)
}

// ---------------------------------------------------------------------- PTP clock

/// A PTP clock locked onto `CLOCK_ID` with offset exactly `PTP_OFFSET_NS`.
fn locked_ptp_clock() -> Arc<PtpClock> {
    let mut tracker = PtpTracker::new();
    tracker.follow(LOCAL, Some(CLOCK_ID), mono_ns());
    let t2 = mono_ns();
    let sync = ptp_message(MSG_SYNC, 1, CLOCK_ID, false, t2 + PTP_OFFSET_NS);
    assert!(tracker.on_datagram(&sync, LOCAL, t2));
    tracker.clock().clone()
}

/// A PTPv2 message (common header + origin timestamp).
fn ptp_message(msg: u8, seq: u16, clock: u64, two_step: bool, origin_ns: u64) -> Vec<u8> {
    let mut data = vec![0u8; 44];
    data[0] = msg;
    data[1] = 0x02;
    data[2..4].copy_from_slice(&44u16.to_be_bytes());
    if two_step {
        data[6..8].copy_from_slice(&FLAG_TWO_STEP.to_be_bytes());
    }
    data[20..28].copy_from_slice(&clock.to_be_bytes());
    data[28..30].copy_from_slice(&1u16.to_be_bytes());
    data[30..32].copy_from_slice(&seq.to_be_bytes());
    data[32] = if msg == MSG_SYNC { 0 } else { 2 };
    let secs = origin_ns / 1_000_000_000;
    data[34..40].copy_from_slice(&secs.to_be_bytes()[2..]);
    data[40..44].copy_from_slice(&((origin_ns % 1_000_000_000) as u32).to_be_bytes());
    data
}

fn start_stream(
    params: &StreamParams,
    consumer: RingConsumer,
    options: RuntimeOptions,
) -> (StreamHandle, u16) {
    let control = ControlSocket::bind(LOCAL).unwrap();
    let port = control.port();
    let handle = match runtime::start(params, control, consumer, options) {
        Ok(handle) => handle,
        Err((e, _)) => panic!("runtime::start failed: {e}"),
    };
    (handle, port)
}

fn ptp_options() -> RuntimeOptions {
    RuntimeOptions {
        ptp: Some(locked_ptp_clock()),
        ..Default::default()
    }
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = mono_ns() + timeout.as_nanos() as u64;
    while mono_ns() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    done()
}

/// Let the receiver's capture thread pick up the last datagrams.
fn settle() {
    thread::sleep(Duration::from_millis(60));
}

/// Stop a stream and return its final counters. While audio is flowing the sender
/// is idle 10 ms after the last push (silence needs 40 ms of quiet), so the snapshot
/// matches what was sent; during a stall one or two more silence packets may follow.
fn stop(handle: StreamHandle) -> (StreamStats, RingConsumer) {
    thread::sleep(Duration::from_millis(10));
    let stats = handle.stats();
    assert_eq!(handle.end_reason(), None);
    let consumer = handle.stop();
    settle();
    (stats, consumer)
}

// ------------------------------------------------------------------------ tests

#[test]
fn bursty_capture_streams_without_silence() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());

    // Windows loopback: bursts with gaps of up to 22 ms, correct average rate.
    feeder.feed(
        Duration::from_millis(1500),
        &[5, 22, 3, 10, 22, 15, 8, 1, 22],
    );
    let (stats, _) = stop(handle);

    assert_eq!(stats.silence_packets, 0, "{stats:?}");
    assert_eq!(stats.stalls, 0, "{stats:?}");
    assert_eq!(stats.send_errors, 0);
    let packets = rx.audio(Codec::Alac);
    assert_eq!(packets.len() as u64, stats.packets);
    assert!(packets.len() >= 170, "only {} packets", packets.len());
    assert_continuous(&packets, &params);
    // The audio is exactly what was captured, in order, nothing dropped or repeated.
    for (k, p) in packets.iter().enumerate() {
        for (j, frame) in p.frames.iter().enumerate() {
            let index = (k * FRAMES_PER_PACKET + j) as u64;
            assert_eq!(*frame, signal(index), "packet {k} frame {j}");
        }
    }
    // Only the partial packet is left in the ring.
    assert!(feeder.next - ((packets.len() * FRAMES_PER_PACKET) as u64) < 352);
}

#[test]
fn stalled_capture_inserts_silence_up_to_the_timeline() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100); // slack 26 ms → silence after 40 ms
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());

    feeder.feed(Duration::from_millis(500), &[5]);
    let last_write = feeder.last_write_ns();
    thread::sleep(Duration::from_millis(400));
    let stats_before_stop = handle.stats();
    let (stats, _) = stop(handle);

    let packets = rx.audio(Codec::Alac);
    assert_continuous(&packets, &params);
    let first_silence = packets
        .iter()
        .position(AudioPacket::is_silence)
        .expect("silence was inserted");
    assert!(packets[first_silence..].iter().all(AudioPacket::is_silence));
    assert!(packets[..first_silence].iter().all(|p| !p.is_silence()));
    // The feed ended mid-packet; that remainder went out zero-padded just before.
    let rest = (feeder.next % FRAMES_PER_PACKET as u64) as usize;
    if rest > 0 {
        let padded = &packets[first_silence - 1];
        assert_signal(
            &padded.frames[..rest],
            feeder.next - rest as u64,
            "remainder",
        );
        assert!(padded.frames[rest..].iter().all(|f| *f == [0, 0]));
    }

    let delay = packets[first_silence].arrival - last_write;
    println!(
        "first silence {:.1} ms after the last capture write",
        delay as f64 / 1e6
    );
    assert!(delay >= 40 * MS, "silence after only {delay} ns");
    assert!(delay <= 100 * MS, "silence only after {delay} ns");
    assert_eq!(stats.stalls, 1);
    let silent = (packets.len() - first_silence) as u64;
    assert!(
        (stats.silence_packets..=stats.silence_packets + 2).contains(&silent),
        "{silent} silent packets received, {} counted",
        stats.silence_packets
    );

    // Silence keeps the head on the timeline: (400 - 40) ms ≈ 45 packets, and the
    // last packet's head is within a packet or two of the nominal clock.
    assert!(
        (35..=60).contains(&stats.silence_packets),
        "{} silence packets",
        stats.silence_packets
    );
    let last_index = packets.len() - 1;
    let last = &packets[last_index];
    let nominal = params.first_ts as f64
        + (last.arrival - stats_before_stop.started_ns) as f64 * 44_100.0 / 1e9;
    let head = last.head_after(last_index, params.first_ts) as f64;
    assert!(
        (head - nominal).abs() < 1500.0,
        "head {head} vs nominal {nominal}"
    );
}

/// `frames` are the signal from frame `start` on.
fn assert_signal(frames: &[Frame], start: u64, context: &str) {
    for (j, frame) in frames.iter().enumerate() {
        assert_eq!(*frame, signal(start + j as u64), "{context}: frame {j}");
    }
}

#[test]
fn audio_ending_mid_packet_is_sent_zero_padded_when_capture_stops() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());

    // The sound ends 300 frames into its 41st packet, then Windows goes quiet.
    const FULL: usize = 40;
    const REST: usize = 300;
    feeder.push_packets_paced(FULL);
    feeder.push(REST);
    let last_write = feeder.last_write_ns();
    thread::sleep(Duration::from_millis(300));
    let (stats, _) = stop(handle);

    let packets = rx.audio(Codec::Alac);
    // Seq, RTP time and nonce counter run on without a gap.
    assert_continuous(&packets, &params);
    assert!(packets.len() > FULL + 20, "{} packets", packets.len());
    for (k, packet) in packets[..FULL].iter().enumerate() {
        let start = (k * FRAMES_PER_PACKET) as u64;
        assert_signal(&packet.frames, start, &format!("packet {k}"));
    }
    // The first packet after the stall, in the next RTP slot: the last 300 frames of
    // the sound, then zeros.
    let tail = &packets[FULL];
    let waited = tail.arrival - last_write;
    assert!(
        (40 * MS..100 * MS).contains(&waited),
        "remainder sent {waited} ns after the last write"
    );
    for (j, frame) in tail.frames.iter().enumerate() {
        let expected = if j < REST {
            signal((FULL * FRAMES_PER_PACKET + j) as u64)
        } else {
            [0, 0]
        };
        assert_eq!(*frame, expected, "remainder packet frame {j}");
    }
    // Nothing but silence afterwards: no audio is left behind in the ring.
    assert!(packets[FULL + 1..].iter().all(AudioPacket::is_silence));
    assert_eq!(stats.ring_frames, 0, "{stats:?}");
    assert_eq!(stats.stalls, 1, "{stats:?}");
    assert_eq!(stats.frames_trimmed, 0, "{stats:?}");
    // The padded packet carries audio, so it is not counted as a silence packet.
    let silent = (packets.len() - FULL - 1) as u64;
    assert!(
        (stats.silence_packets..=stats.silence_packets + 2).contains(&silent),
        "{silent} silent packets received, {} counted",
        stats.silence_packets
    );
}

/// A write shorter than a packet during a stall (what the capture's late resampler
/// flush used to produce) neither ends the stall nor waits for the next sound: it is
/// sent zero-padded in its own slot once capture is quiet again.
#[test]
fn short_write_during_a_stall_is_not_prepended_to_the_next_sound() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());

    feeder.push_packets_paced(20);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(handle.stats().stalls, 1);
    const SHORT: usize = 119;
    let short_start = feeder.next;
    feeder.push(SHORT);
    let short_write = feeder.last_write_ns();
    thread::sleep(Duration::from_millis(150));
    let next_sound = feeder.next;
    feeder.push_packets_paced(20);
    let (stats, _) = stop(handle);

    let packets = rx.audio(Codec::Alac);
    assert_continuous(&packets, &params);
    assert_eq!(stats.stalls, 1, "{stats:?}");
    let short = packets
        .iter()
        .position(|p| p.frames[0] == signal(short_start))
        .expect("the short write was sent");
    let packet = &packets[short];
    assert!(
        packet.arrival - short_write >= 40 * MS,
        "sent before capture was quiet"
    );
    assert_signal(&packet.frames[..SHORT], short_start, "short write");
    assert!(packet.frames[SHORT..].iter().all(|f| *f == [0, 0]));
    assert!(packets[20..short].iter().all(AudioPacket::is_silence));
    // The next sound starts on a packet boundary of its own.
    let resumed = packets
        .iter()
        .position(|p| p.frames[0] == signal(next_sound))
        .expect("the next sound was sent");
    assert!(
        packets[short + 1..resumed]
            .iter()
            .all(AudioPacket::is_silence)
    );
    for (k, packet) in packets[resumed..].iter().enumerate() {
        let start = next_sound + (k * FRAMES_PER_PACKET) as u64;
        assert_signal(&packet.frames, start, &format!("next sound packet {k}"));
    }
    assert_eq!(packets.len() - resumed, 20);
}

#[test]
fn late_audio_after_a_hiccup_is_trimmed() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());

    feeder.feed(Duration::from_millis(400), &[5]);
    // Capture hiccup: nothing for 200 ms, then Windows delivers it all at once.
    thread::sleep(Duration::from_millis(200));
    let during = handle.stats();
    assert_eq!(during.stalls, 1, "{during:?}");
    feeder.push(8820);
    feeder.feed(Duration::from_millis(300), &[5]);
    let (stats, _) = stop(handle);

    println!(
        "hiccup: {} silence packets, {} frames trimmed, ring {} frames",
        stats.silence_packets, stats.frames_trimmed, stats.ring_frames
    );
    assert_eq!(stats.stalls, 1);
    assert!(stats.frames_trimmed > 4000, "{stats:?}");
    assert!(stats.frames_trimmed <= stats.silence_packets * FRAMES_PER_PACKET as u64);
    assert!(
        stats.ring_frames < 352 + 1323 + 2 * CHUNK as u64,
        "{stats:?}"
    );

    // Without the trim the head would now run ~190 ms (≈8400 frames) ahead of the
    // clock; with it, it stays near the timeline.
    let packets = rx.audio(Codec::Alac);
    assert_continuous(&packets, &params);
    let last_index = packets.len() - 1;
    let last = &packets[last_index];
    let nominal =
        params.first_ts as f64 + (last.arrival - stats.started_ns) as f64 * 44_100.0 / 1e9;
    let head = last.head_after(last_index, params.first_ts) as f64;
    // What remains is the kept backlog (one packet + 30 ms ≈ 1675 frames).
    assert!(head - nominal < 3000.0, "head {head} vs nominal {nominal}");
    // Capture resumes with the oldest queued packet (taken before the trim, as in
    // Python), then skips the trimmed frames.
    let resumed = packets
        .iter()
        .rposition(AudioPacket::is_silence)
        .map(|i| i + 1)
        .unwrap();
    let sent_before = packets[..resumed]
        .iter()
        .filter(|p| !p.is_silence())
        .count() as u64
        * FRAMES_PER_PACKET as u64;
    assert_eq!(
        packets[resumed].frames[0],
        signal(sent_before),
        "resume point"
    );
    assert_eq!(
        packets[resumed + 1].frames[0],
        signal(sent_before + FRAMES_PER_PACKET as u64 + stats.frames_trimmed),
        "after the trim"
    );
}

#[test]
fn retransmit_request_is_answered_from_the_backlog() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, control_port) = start_stream(&params, consumer, ptp_options());
    feeder.feed(Duration::from_millis(500), &[5]);

    // Ask for two packets across the seq wrap (65535 → 0).
    assert!(wait_until(Duration::from_secs(1), || rx
        .audio(Codec::Alac)
        .iter()
        .any(|p| p.seq == 0)));
    let packets = rx.audio(Codec::Alac);
    let wrap = packets.iter().position(|p| p.seq == 65_535).unwrap();
    rx.request_retransmit(control_port, 65_535, 2);
    assert!(wait_until(Duration::from_secs(1), || rx
        .control_packets(0xD6)
        .len()
        >= 2));
    let replies = rx.control_packets(0xD6);
    assert_eq!(replies.len(), 2);
    for (reply, original) in replies.iter().zip(&packets[wrap..wrap + 2]) {
        let reply = &reply.1;
        assert_eq!(&reply[..2], &[0x80, 0xD6]);
        assert_eq!(&reply[2..4], &original.seq.to_be_bytes());
        assert_eq!(&reply[4..], original.raw.as_slice(), "seq {}", original.seq);
    }
    // A request for packets never sent produces no reply but is counted.
    rx.request_retransmit(control_port, 30_000, 3);
    assert!(wait_until(Duration::from_secs(1), || handle
        .stats()
        .retransmit_requests
        == 2));
    let (stats, _) = stop(handle);
    assert_eq!(stats.retransmit_requests, 2);
    assert_eq!(stats.retransmitted, 2);
    assert_eq!(rx.control_packets(0xD6).len(), 2);
}

#[test]
fn drop_every_withholds_packets_but_keeps_them_for_retransmission() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let options = RuntimeOptions {
        drop_every: Some(10),
        ..ptp_options()
    };
    let (handle, control_port) = start_stream(&params, consumer, options);
    feeder.feed(Duration::from_millis(500), &[5]);

    thread::sleep(Duration::from_millis(10)); // let the sender go idle
    let stats = handle.stats();
    assert_eq!(stats.withheld, stats.packets / 10, "{stats:?}");
    settle();
    let packets = rx.audio(Codec::Alac);
    let seqs: Vec<u16> = packets.iter().map(|p| p.seq).collect();
    // Packets 10, 20, 30, ... (1-based) are missing on the wire.
    let tenth = FIRST_SEQ.wrapping_add(9);
    assert!(!seqs.contains(&tenth));
    assert!(seqs.contains(&FIRST_SEQ.wrapping_add(8)));
    assert!(seqs.contains(&FIRST_SEQ.wrapping_add(10)));
    assert!(!seqs.contains(&FIRST_SEQ.wrapping_add(19)));

    rx.request_retransmit(control_port, tenth, 1);
    assert!(wait_until(Duration::from_secs(1), || !rx
        .control_packets(0xD6)
        .is_empty()));
    let reply = rx.control_packets(0xD6).remove(0).1;
    assert_eq!(&reply[2..4], &tenth.to_be_bytes());
    let recovered = AudioPacket::decode(0, reply[4..].to_vec(), Codec::Alac);
    assert_eq!(recovered.seq, tenth);
    assert_eq!(recovered.ts, (params.first_ts + 9 * 352) as u32);
    assert_eq!(recovered.counter, 9);
    assert_eq!(recovered.frames[0], signal(9 * 352));
    // Silence started meanwhile; a packet or two may follow the final snapshot.
    let (stats, _) = stop(handle);
    let on_wire = rx.audio(Codec::Alac).len() as u64;
    let expected = stats.packets - stats.withheld;
    assert!(
        (expected..=expected + 2).contains(&on_wire),
        "{on_wire} vs {stats:?}"
    );
}

#[test]
fn ptp_anchors_have_the_expected_cadence_and_content() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());
    feeder.feed(Duration::from_millis(2300), &[5, 22, 8]);
    let (stats, _) = stop(handle);

    let anchors = rx.control_packets(0xD7);
    let audio = rx.audio(Codec::Alac);
    assert_eq!(anchors.len() as u64, stats.anchors);
    assert!(
        (3..=4).contains(&anchors.len()),
        "{} anchors",
        anchors.len()
    );
    // The first anchor (0x90) arrives before any audio.
    assert!(anchors[0].0 < audio[0].arrival);
    for (i, (arrival, bytes)) in anchors.iter().enumerate() {
        assert_eq!(bytes.len(), 28);
        assert_eq!(bytes[0], if i == 0 { 0x90 } else { 0x80 });
        assert_eq!(&bytes[1..4], &[0xD7, 0x00, 0x06]);
        let played = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
        let master_ns = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        let head = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let clock = u64::from_be_bytes(bytes[20..28].try_into().unwrap());
        assert_eq!(clock, CLOCK_ID);
        assert_eq!(played, head.wrapping_sub(params.latency_frames));
        // master = build time + offset + 1 ms; built at most a few ms before arrival.
        let local = master_ns - PTP_OFFSET_NS - MS;
        assert!(local <= *arrival && *arrival - local < 5 * MS, "anchor {i}");
        // head = smoothed timeline at the build time.
        let nominal = params.first_ts as f64 + (local - stats.started_ns) as f64 * 44_100.0 / 1e9;
        assert!(
            (f64::from(head) - nominal).abs() < 1500.0,
            "anchor {i}: head {head} vs nominal {nominal}"
        );
        if i == 0 {
            assert!(u64::from(head) - params.first_ts < 45, "first head {head}");
        } else {
            let interval = arrival - anchors[i - 1].0;
            assert!(
                (950 * MS..=1050 * MS).contains(&interval),
                "anchor interval {interval} ns"
            );
        }
    }
}

#[test]
fn ntp_anchors_are_d4_sync_packets() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let mut params = rx.params(Codec::Pcm, 250);
    params.timing = TimingProtocol::Ntp;
    let (mut feeder, consumer) = new_ring();
    let anchor_mono = mono_ns();
    let clock = Arc::new(NtpClock::with_anchor(
        1_761_000_000_000_000_000,
        anchor_mono,
    ));
    assert_eq!(
        clock.ntp_at(anchor_mono + 500_000_000),
        unix_ns_to_ntp(1_761_000_000_500_000_000)
    );
    let options = RuntimeOptions {
        ntp: Some(clock.clone()),
        ..Default::default()
    };
    let (handle, _) = start_stream(&params, consumer, options);
    feeder.feed(Duration::from_millis(1100), &[5]);
    let (stats, _) = stop(handle);

    let anchors = rx.control_packets(0xD4);
    assert_eq!(anchors.len(), 2, "{stats:?}");
    for (i, (arrival, bytes)) in anchors.iter().enumerate() {
        assert_eq!(bytes.len(), 20);
        assert_eq!(bytes[0], if i == 0 { 0x90 } else { 0x80 });
        assert_eq!(&bytes[1..4], &[0xD4, 0x00, 0x07]);
        let played = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
        let ntp = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        let head = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        assert_eq!(played, head.wrapping_sub(params.latency_frames));
        let at_arrival = clock.ntp_at(*arrival);
        // Built at most 5 ms (≈ 21.5 M NTP units) before it arrived.
        assert!(
            ntp <= at_arrival && at_arrival - ntp < 21_474_836,
            "anchor {i}"
        );
    }
    // PCM payloads decode back to the signal.
    let packets = rx.audio(Codec::Pcm);
    assert_continuous(&packets, &params);
    assert_eq!(packets[0].frames[0], signal(0));
}

#[test]
fn stop_returns_a_usable_consumer() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());
    feeder.feed(Duration::from_millis(200), &[5]);
    let (first, consumer) = stop(handle);
    assert!(first.packets > 15);

    // The returned consumer is still connected to the producer.
    let before = consumer.available();
    feeder.push(1000);
    assert_eq!(consumer.available(), before + 1000);

    // And it streams again: start clears the stale audio first.
    let rx2 = MockReceiver::new();
    let params2 = rx2.params(Codec::Alac, 100);
    let (handle, _) = start_stream(&params2, consumer, ptp_options());
    let resume_index = feeder.next;
    feeder.feed(Duration::from_millis(200), &[5]);
    let (second, consumer) = stop(handle);
    assert!(second.packets > 15);
    let packets = rx2.audio(Codec::Alac);
    assert_continuous(&packets, &params2);
    assert_eq!(
        packets[0].frames[0],
        signal(resume_index),
        "stale audio was cleared"
    );
    drop(consumer);
}

#[test]
fn dropping_the_handle_stops_the_stream() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());
    feeder.feed(Duration::from_millis(100), &[5]);
    drop(handle);
    settle();
    let count = rx.data.snapshot().len();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(rx.data.snapshot().len(), count, "packets after drop");
}

#[test]
fn start_failure_returns_the_consumer() {
    let rx = MockReceiver::new();
    let mut params = rx.params(Codec::Alac, 100);
    // A local address that does not exist on this machine cannot be bound.
    params.local_ip = Ipv4Addr::new(203, 0, 113, 7);
    let (mut feeder, consumer) = new_ring();
    let control = ControlSocket::bind(LOCAL).unwrap();
    let Err((_, consumer)) = runtime::start(&params, control, consumer, ptp_options()) else {
        panic!("start should fail");
    };
    feeder.push(500);
    assert_eq!(consumer.available(), 500);
}

#[test]
fn sender_wakeup_latency() {
    let _serial = serial();
    let rx = MockReceiver::new();
    let params = rx.params(Codec::Alac, 100);
    let (mut feeder, consumer) = new_ring();
    let (handle, _) = start_stream(&params, consumer, ptp_options());

    // One packet's worth of audio at a time, at the stream rate.
    let count = 200usize;
    let mut pushed_at = Vec::with_capacity(count);
    let t0 = mono_ns();
    for k in 0..count {
        let due = t0 + (k as u64 * 352 * 1_000_000_000) / 44_100;
        while mono_ns() < due {
            thread::sleep(Duration::from_micros(500));
        }
        pushed_at.push(mono_ns());
        feeder.push(352);
    }
    let (stats, _) = stop(handle);
    assert_eq!(stats.silence_packets, 0);

    let packets = rx.audio(Codec::Alac);
    assert_eq!(packets.len(), count);
    assert_continuous(&packets, &params);
    let mut latencies: Vec<u64> = packets
        .iter()
        .zip(&pushed_at)
        .map(|(p, pushed)| p.arrival - pushed)
        .collect();
    latencies.sort_unstable();
    let pct = |p: usize| latencies[(latencies.len() - 1) * p / 100] as f64 / 1e3;
    println!(
        "sender wake-up latency (ring push → packet received), {} packets: \
         min {:.0} us, p50 {:.0} us, p90 {:.0} us, p99 {:.0} us, max {:.0} us",
        count,
        pct(0),
        pct(50),
        pct(90),
        pct(99),
        pct(100)
    );
    assert!(pct(50) < 3_000.0, "median wake-up latency {} us", pct(50));
}

// ------------------------------------------------------------ PTP follower service

fn ptp_ports(service: &PtpService) -> (u16, u16) {
    if service.standard_ports() {
        assert_eq!(service.port(), 319);
        (319, 320)
    } else {
        assert_ne!(service.port(), 319);
        (service.port(), service.port())
    }
}

#[test]
fn ptp_service_locks_onto_synthetic_sync_and_follow_up() {
    let _serial = ptp_serial();
    let service = PtpService::start(LOCAL).unwrap();
    let (event_port, general_port) = ptp_ports(&service);
    println!(
        "PTP service on port {} (standard ports: {})",
        service.port(),
        service.standard_ports()
    );
    let master = UdpSocket::bind((LOCAL, 0)).unwrap();
    let clock = service.clock();

    // Before follow(): ignored.
    master
        .send_to(
            &ptp_message(MSG_SYNC, 1, CLOCK_ID, false, 5),
            (LOCAL, event_port),
        )
        .unwrap();
    assert!(!service.wait_locked(Duration::from_millis(100)));
    assert!(wait_until(Duration::from_secs(1), || clock.syncs() == 1));
    assert_eq!(clock.ignored(), 1);

    service.follow(LOCAL, Some(CLOCK_ID));
    // A foreign clock is ignored while the expected one is awaited.
    master
        .send_to(
            &ptp_message(MSG_SYNC, 7, 0x1111, true, 0),
            (LOCAL, event_port),
        )
        .unwrap();
    // Two-step Sync to the event port, Follow_Up with T1 to the general port.
    let sent = mono_ns();
    let t1 = sent + 5_000_000_000;
    master
        .send_to(
            &ptp_message(MSG_SYNC, 2, CLOCK_ID, true, 0),
            (LOCAL, event_port),
        )
        .unwrap();
    master
        .send_to(
            &ptp_message(MSG_FOLLOW_UP, 2, CLOCK_ID, false, t1),
            (LOCAL, general_port),
        )
        .unwrap();
    assert!(service.wait_locked(Duration::from_secs(2)));
    assert_eq!(clock.master_clock_id(), CLOCK_ID);
    assert!(clock.locked());
    let offset = clock.offset_ns().unwrap();
    // offset = T1 − t2 where t2 is a little after `sent`.
    assert!(offset <= 5e9 && offset > 5e9 - 20e6, "offset {offset}");
    let now = mono_ns();
    let master_now = clock.to_master_ns(now).unwrap();
    assert_eq!(master_now, (now as f64 + offset) as u64 + 1_000_000);
    assert_eq!(clock.ignored(), 2);
    assert_eq!(service.counters().follow_ups, 1);

    // follow() starts over: nothing until the next sample.
    service.follow(LOCAL, Some(CLOCK_ID));
    assert!(!clock.locked());
    assert!(!service.wait_locked(Duration::from_millis(50)));
    master
        .send_to(
            &ptp_message(MSG_SYNC, 3, CLOCK_ID, false, mono_ns() + 7_000_000_000),
            (LOCAL, event_port),
        )
        .unwrap();
    assert!(service.wait_locked(Duration::from_secs(2)));
    assert!(clock.offset_ns().unwrap() > 6.9e9);
}

#[test]
fn ptp_service_stops_when_dropped() {
    let _serial = ptp_serial();
    let service = PtpService::start(LOCAL).unwrap();
    let port = service.port();
    drop(service);
    // The port (or the standard pair) is free again.
    let rebind = UdpSocket::bind((LOCAL, port));
    assert!(rebind.is_ok(), "port {port} still bound");
}

// ------------------------------------------------------------- NTP timing responder

#[test]
fn timing_responder_answers_requests() {
    let clock = Arc::new(NtpClock::new());
    let responder = TimingResponder::start(LOCAL, clock.clone()).unwrap();
    assert_eq!(responder.requests(), 0);
    assert_eq!(responder.last_request_ns(), 0);
    let peer = UdpSocket::bind((LOCAL, 0)).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

    let mut request = [0u8; 32];
    request[0] = 0x80;
    request[1] = 0xD2;
    request[2..4].copy_from_slice(&7u16.to_be_bytes());
    request[24..32].copy_from_slice(&0xDEAD_BEEF_0123_4567u64.to_be_bytes());
    // Not a timing request: ignored.
    let mut other = request;
    other[1] = 0xD4;
    peer.send_to(&other, (LOCAL, responder.port())).unwrap();

    let before = clock.ntp_at(mono_ns());
    peer.send_to(&request, (LOCAL, responder.port())).unwrap();
    let mut reply = [0u8; 64];
    let (len, from) = peer.recv_from(&mut reply).unwrap();
    let after = clock.ntp_at(mono_ns());
    assert_eq!(from, SocketAddr::from((LOCAL, responder.port())));
    assert_eq!(len, 32);
    assert_eq!(&reply[..8], &[0x80, 0xD3, 0x00, 0x07, 0, 0, 0, 0]);
    assert_eq!(
        &reply[8..16],
        &request[24..32],
        "reference = request send time"
    );
    let recv = u64::from_be_bytes(reply[16..24].try_into().unwrap());
    let send = u64::from_be_bytes(reply[24..32].try_into().unwrap());
    assert_eq!(recv, send);
    assert!(before <= recv && recv <= after);
    assert!(wait_until(Duration::from_secs(1), || responder.requests() == 1));
    assert!(responder.last_request_ns() > 0);
}
