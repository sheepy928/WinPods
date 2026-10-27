//! The real-time sender thread (workstream: real-time).
//!
//! Port of `_Sender._run` in `legacy/python/winpods/raop_live.py`, adapted to the
//! event-driven ring: loop { drain every complete 352-frame packet (with the
//! trim-after-stall branch); stall check; `wait_any([stop, ring_event,
//! hires_timer(4 ms)])` }. Silence only when the ring has had no writes for
//! `max(slack, 40 ms)` AND the timeline has run `slack` ahead, where
//! `slack = clamp(latency − render − 5, 5, 250)` ms; silence fills up to the
//! timeline (max 16 packets per wake); after a stall, trim ring content above
//! packet + 30 ms, up to the silence debt. MMCSS "Pro Audio"; no allocation,
//! logging or contended locks in the loop. Optional diagnostic `drop_every`:
//! skip the socket send of every Nth packet (still stored in the backlog).
//!
//! Why the thresholds: Windows loopback capture delivers in bursts with gaps of up
//! to ~22 ms. Those gaps are ridden out by the receiver's buffer (latency minus its
//! render time), so silence is only inserted once Windows has really stopped
//! producing audio; it keeps the RTP timeline continuous. Audio that piles up during
//! a capture hiccup would add the hiccup to the latency for good, so as much of it
//! as was padded with silence is dropped when capture resumes.

use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::time::Duration;

use winpods_rt::{Frame, HiResTimer, RingConsumer, mono_ns, wait_any};

use crate::alac::{encode_alac_packet, encode_pcm_packet};
use crate::packet::{AudioCipher, MAX_PACKET_BYTES, rtp_header};
use crate::runtime::Shared;
use crate::timeline::Timeline;
use crate::types::{
    ALAC_FRAME_BYTES, Codec, DEFAULT_RENDER_MS, FRAMES_PER_PACKET, PCM_FRAME_BYTES, SAMPLE_RATE,
};

/// Silence packets sent per wake-up while catching up with the timeline.
pub const MAX_SILENCE_BURST: usize = 16;
/// Never trim more than 1 s of late audio after a stall.
pub const MAX_DEBT_FRAMES: usize = SAMPLE_RATE as usize;
/// Network allowance before a missing packet would be late.
pub const SAFETY_MS: i64 = 5;
/// Windows loopback delivers in bursts with gaps of ~10–25 ms.
pub const MIN_STALL_MS: u32 = 40;
/// Ring content beyond this (after a stall) is piled-up late audio.
pub const NORMAL_BACKLOG_MS: usize = 30;
/// Longest the sender sleeps without new audio before re-checking for a stall.
pub const POLL_INTERVAL: Duration = Duration::from_millis(4);
/// The sender gives up after this many failed socket sends (like the Python sender).
pub const MAX_SEND_ERRORS: u64 = 50;

/// When silence is inserted and how much late audio is trimmed, derived from the
/// stream latency and the receiver's render time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StallPolicy {
    /// `clamp(latency − render − 5, 5, 250)` ms: how far the timeline may run ahead
    /// of the sent audio before silence is due.
    pub slack_ms: u32,
    /// `slack_ms` in frames (fractional, as in Python).
    pub slack_frames: f64,
    /// `max(slack_ms, 40)` ms without capture writes before silence is due, in ns.
    pub stall_ns: u64,
    /// One packet plus 30 ms, in frames: ring content kept after a stall.
    pub normal_backlog_frames: usize,
}

impl StallPolicy {
    /// `render_ms` of `None` or 0 means the receiver did not report one (70 ms).
    pub fn new(latency_ms: u32, render_ms: Option<u32>) -> Self {
        let render_ms = render_ms.filter(|&r| r > 0).unwrap_or(DEFAULT_RENDER_MS);
        let slack = (i64::from(latency_ms) - i64::from(render_ms) - SAFETY_MS).clamp(5, 250);
        let slack_ms = slack as u32;
        Self {
            slack_ms,
            slack_frames: f64::from(slack_ms * SAMPLE_RATE) / 1000.0,
            stall_ns: u64::from(slack_ms.max(MIN_STALL_MS)) * 1_000_000,
            normal_backlog_frames: FRAMES_PER_PACKET
                + NORMAL_BACKLOG_MS * SAMPLE_RATE as usize / 1000,
        }
    }
}

/// Everything the sender needs from the negotiated stream.
pub(crate) struct SenderConfig {
    pub(crate) codec: Codec,
    pub(crate) ssrc: u32,
    pub(crate) first_seq: u16,
    pub(crate) first_ts: u64,
    pub(crate) shk: [u8; 32],
    pub(crate) policy: StallPolicy,
    /// 0 = off.
    pub(crate) drop_every: u32,
}

/// Sender state; lives on the sender thread.
pub(crate) struct Sender {
    socket: UdpSocket,
    cipher: AudioCipher,
    codec: Codec,
    ssrc: u32,
    seq: u16,
    /// Unwrapped RTP time of the next packet (= head of everything sent so far).
    rtptime: u64,
    first: bool,
    timeline: Timeline,
    policy: StallPolicy,
    drop_every: u64,
    built: u64,
    send_errors: u64,
}

impl Sender {
    /// `timeline` must already be started (at the stream's first timestamp).
    pub(crate) fn new(socket: UdpSocket, config: &SenderConfig, timeline: Timeline) -> Self {
        Self {
            socket,
            cipher: AudioCipher::new(&config.shk),
            codec: config.codec,
            ssrc: config.ssrc,
            seq: config.first_seq,
            rtptime: config.first_ts,
            first: true,
            timeline,
            policy: config.policy,
            drop_every: u64::from(config.drop_every),
            built: 0,
            send_errors: 0,
        }
    }

    /// Encode, seal, send (unless withheld), store and account for one packet.
    fn send(&mut self, frames: &[Frame; FRAMES_PER_PACKET], shared: &Shared) -> Result<(), String> {
        let header = rtp_header(self.first, self.seq, self.rtptime, self.ssrc);
        let mut alac = [0u8; ALAC_FRAME_BYTES];
        let mut pcm = [0u8; PCM_FRAME_BYTES];
        let payload: &[u8] = match self.codec {
            Codec::Alac => {
                let len = encode_alac_packet(frames, &mut alac);
                &alac[..len]
            }
            Codec::Pcm => {
                let len = encode_pcm_packet(frames, &mut pcm);
                &pcm[..len]
            }
        };
        let mut packet = [0u8; MAX_PACKET_BYTES];
        let len = self
            .cipher
            .seal(&header, payload, &mut packet)
            .map_err(|e| format!("sender failed: {e}"))?;
        let packet = &packet[..len];
        self.first = false;
        self.built += 1;

        let stats = &shared.stats;
        if self.drop_every != 0 && self.built.is_multiple_of(self.drop_every) {
            stats.withheld.fetch_add(1, Ordering::Relaxed);
        } else if let Err(e) = self.socket.send(packet) {
            self.send_errors += 1;
            stats.send_errors.fetch_add(1, Ordering::Relaxed);
            if self.send_errors > MAX_SEND_ERRORS {
                return Err(format!("sender failed: too many send errors ({e})"));
            }
        }
        shared.backlog.store(self.seq, packet);
        self.seq = self.seq.wrapping_add(1);
        self.rtptime += FRAMES_PER_PACKET as u64;
        stats.packets.fetch_add(1, Ordering::Relaxed);
        self.timeline.observe(mono_ns(), self.rtptime);
        shared.timeline.publish(&self.timeline);
        Ok(())
    }

    /// The sender loop; returns when `shared.stop` is set, or with the reason the
    /// stream cannot continue.
    pub(crate) fn run(
        &mut self,
        consumer: &mut RingConsumer,
        shared: &Shared,
        timer: &HiResTimer,
    ) -> Result<(), String> {
        let ring = consumer.shared().clone();
        let handles = [
            shared.sender_wake.handle(),
            ring.event_handle(),
            timer.handle(),
        ];
        let policy = self.policy;
        let silence = [[0i16; 2]; FRAMES_PER_PACKET];
        let mut frames = [[0i16; 2]; FRAMES_PER_PACKET];
        let mut in_stall = false;
        let mut debt = 0usize; // frames of silence inserted during the current stall
        let stats = &shared.stats;

        loop {
            if shared.stop.load(Ordering::Acquire) {
                return Ok(());
            }

            // Send every complete packet the capture has committed.
            while consumer.pop_exact(&mut frames) {
                if in_stall {
                    in_stall = false;
                    let excess = consumer
                        .available()
                        .saturating_sub(policy.normal_backlog_frames);
                    if debt > 0 && excess > 0 {
                        let trimmed = consumer.discard(debt.min(excess));
                        stats
                            .frames_trimmed
                            .fetch_add(trimmed as u64, Ordering::Relaxed);
                    }
                    debt = 0;
                }
                self.send(&frames, shared)?;
                if shared.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
            }
            stats
                .ring_frames
                .store(consumer.available() as u64, Ordering::Relaxed);

            // Not enough audio for a packet: has Windows stopped playing?
            let now = mono_ns();
            let target = self.timeline.head_at(now);
            if !in_stall {
                let last_write = ring.last_write_ns();
                let quiet = if last_write == 0 {
                    u64::MAX
                } else {
                    now.saturating_sub(last_write)
                };
                if quiet >= policy.stall_ns && target - self.rtptime as f64 >= policy.slack_frames {
                    in_stall = true;
                    stats.stalls.fetch_add(1, Ordering::Relaxed);
                }
            }
            if in_stall {
                let mut burst = 0;
                while (self.rtptime + FRAMES_PER_PACKET as u64) as f64 <= target
                    && burst < MAX_SILENCE_BURST
                {
                    self.send(&silence, shared)?;
                    stats.silence_packets.fetch_add(1, Ordering::Relaxed);
                    debt = (debt + FRAMES_PER_PACKET).min(MAX_DEBT_FRAMES);
                    burst += 1;
                }
            }

            timer
                .arm(POLL_INTERVAL)
                .map_err(|e| format!("sender failed: timer: {e}"))?;
            // The timer bounds the wait; the timeout is only a safety net.
            wait_any(&handles, Some(Duration::from_millis(100)))
                .map_err(|e| format!("sender failed: wait: {e}"))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_policy_matches_python() {
        // 100 ms on a HomePod (render 69 ms): slack 26 ms, silence after 40 ms.
        let p = StallPolicy::new(100, Some(69));
        assert_eq!(p.slack_ms, 26);
        assert_eq!(p.slack_frames, 1146.6);
        assert_eq!(p.stall_ns, 40_000_000);
        assert_eq!(p.normal_backlog_frames, 352 + 1323);

        // Unknown render time → 70 ms; latency below render clamps to 5 ms.
        let p = StallPolicy::new(75, None);
        assert_eq!(p.slack_ms, 5);
        assert_eq!(p.stall_ns, 40_000_000);
        assert_eq!(StallPolicy::new(75, Some(0)), p);

        // Long latencies cap the slack at 250 ms (and the stall wait with it).
        let p = StallPolicy::new(2000, Some(69));
        assert_eq!(p.slack_ms, 250);
        assert_eq!(p.slack_frames, 11025.0);
        assert_eq!(p.stall_ns, 250_000_000);

        let p = StallPolicy::new(250, Some(69));
        assert_eq!(p.slack_ms, 176);
        assert_eq!(p.stall_ns, 176_000_000);
    }
}
