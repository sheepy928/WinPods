//! One AirPlay 2 RTSP session (workstream: control plane). CONTRACT: used by the
//! engine; keep the public signatures.
//!
//! Implements the normative session sequence (plan appendix), split into phases so
//! the engine can bind UDP sockets on the right interface in between:
//!
//! 1. [`Session::connect`] — TCP connect to `info.raop_port`; `GET /info` RTSP/1.0
//!    in plaintext (CSeq 0; non-200 = empty); transient pair-setup (retry up to 3×
//!    on authentication failure, counting retries); enable HAP encryption.
//! 2. [`Session::setup`] — base SETUP (PTP: `timingPeerInfo`; NTP: `timingPort`),
//!    connect the event channel to `eventPort` (5 attempts 1 s apart on connection
//!    refused), stream SETUP (`controlPort` = our control socket), then for PTP
//!    `ptp.follow(receiver, ClockID)` and a 5 s lock wait (failure →
//!    [`SessionError::ClockNotLocked`]). Picks the random first sequence number.
//! 3. [`Session::start_playback`] — DMAP metadata if `md` contains 0 (errors
//!    ignored), start the POST /feedback loop (every 2 s), RECORD unless the base
//!    response had `skipRecord`, FLUSH with `RTP-Info` (errors ignored), initial
//!    volume. The engine then calls `runtime::start`.
//! 4. [`Session::set_volume`] while streaming; [`Session::teardown`] at the end
//!    (TEARDOWN `Session: 0`, 3 s timeout).
//!
//! Internally one task owns the RTSP connection; volume, feedback and teardown are
//! serialised through it. Read EOF or 3 consecutive feedback failures set
//! [`Session::end_reason`].

use std::net::Ipv4Addr;
use std::sync::Arc;

use crate::ptp::PtpService;
use crate::types::{Codec, ReceiverInfo, StreamParams};

/// Static settings for a session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Shown on the receiver ("name" in SETUP, DMAP artist).
    pub sender_name: String,
    /// Stored random MAC "AA:BB:CC:DD:EE:FF" used as deviceID/macAddress.
    pub device_id: String,
    pub latency_ms: u32,
    pub codec: Codec,
    /// Initial receiver volume in percent; `None` = leave it unchanged.
    pub volume_pct: Option<f32>,
    /// DMAP title, "Windows audio".
    pub title: String,
}

/// How the receiver is timed; owned by the engine and passed to `setup`.
pub enum TimingSetup {
    Ptp(Arc<PtpService>),
    Ntp { timing_port: u16 },
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no response to {0} (timeout)")]
    Timeout(String),
    #[error("{method} rejected with {code} {reason}")]
    Rejected {
        method: String,
        code: u16,
        reason: String,
    },
    #[error("pairing failed: {0}")]
    Pairing(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error(
        "No clock (PTP) messages from the receiver reached this PC. Allow WinPods through \
         Windows Firewall for this network (see README), or switch the clock protocol to NTP."
    )]
    ClockNotLocked,
}

/// A connected (and later streaming) AirPlay 2 session.
pub struct Session {
    _private: (),
}

impl Session {
    pub async fn connect(
        info: &ReceiverInfo,
        config: SessionConfig,
    ) -> Result<Session, SessionError> {
        let _ = (info, config);
        todo!("session: connect")
    }

    /// Local address of the RTSP connection (bind UDP sockets here).
    pub fn local_ip(&self) -> Ipv4Addr {
        todo!("session: local_ip")
    }

    /// Pair-setup attempts that had to be retried (should be 0).
    pub fn pairing_retries(&self) -> u32 {
        todo!("session: pairing_retries")
    }

    pub async fn setup(
        &mut self,
        timing: TimingSetup,
        control_port: u16,
    ) -> Result<StreamParams, SessionError> {
        let _ = (timing, control_port);
        todo!("session: setup")
    }

    pub async fn start_playback(&mut self, params: &StreamParams) -> Result<(), SessionError> {
        let _ = params;
        todo!("session: start_playback")
    }

    pub async fn set_volume(&self, pct: f32) -> Result<(), SessionError> {
        let _ = pct;
        todo!("session: set_volume")
    }

    /// Why the session is no longer usable (connection closed, feedback failing).
    pub fn end_reason(&self) -> Option<String> {
        todo!("session: end_reason")
    }

    /// Latest POST /feedback response body, decoded for diagnostics.
    pub fn last_feedback(&self) -> Option<String> {
        todo!("session: last_feedback")
    }

    pub async fn teardown(self) {
        todo!("session: teardown")
    }
}
