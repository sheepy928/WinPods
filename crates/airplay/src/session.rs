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
//!
//! Implementation notes:
//!
//! * The actor task is spawned at the end of `connect` (after pairing) and owns the
//!   [`RtspClient`]. Every later request is a command answered over a oneshot, so
//!   requests never overlap. While idle the task keeps reading the socket, which is
//!   how a closed connection is noticed between requests.
//! * Request order matches pyatv's: pyatv's feedback task first runs while the
//!   request after `start_feedback()` (RECORD, or FLUSH with `skipRecord`) awaits its
//!   reply, so its POST /feedback takes the next CSeq. Here the first feedback is
//!   sent right after that request completes, which yields the same CSeq sequence
//!   (metadata, RECORD, feedback, FLUSH, volume — `rtsp_requests.json`).
//! * Feedback failures are counted per run of consecutive failures; the third one
//!   sets the end reason (pyatv only logged them).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::bodies::{self, BaseSetup, BaseTiming, BodyError, ClockPorts};
use crate::event_channel::EventChannel;
use crate::hap::{self, PairingError, SharedSecret};
use crate::ptp::PtpService;
use crate::rtsp::{Response, RtspClient, RtspContext, RtspError, RtspRequest};
use crate::types::{
    Codec, DEFAULT_RENDER_MS, ReceiverInfo, StreamParams, TimingProtocol, latency_frames,
};

/// Time allowed for the TCP connection to the receiver.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Pair-setup is re-run at most this many times after a rejected attempt.
pub const MAX_PAIRING_RETRIES: u32 = 3;
/// Interval between POST /feedback requests.
pub const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// Consecutive feedback failures that end the session.
pub const FEEDBACK_FAILURE_LIMIT: u32 = 3;
/// How long `setup` waits for the PTP follower to lock.
pub const PTP_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Time allowed for the TEARDOWN exchange.
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// Network allowance on top of the receiver's render time (latency warning).
const SAFETY_MS: u32 = 5;
/// DMAP album shown on the receiver.
const DMAP_ALBUM: &str = "AirPlay";

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

impl From<RtspError> for SessionError {
    fn from(err: RtspError) -> SessionError {
        match err {
            RtspError::Io(err) => SessionError::Io(err),
            RtspError::Timeout(what) => SessionError::Timeout(what),
            RtspError::Closed => SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "the receiver closed the connection",
            )),
            RtspError::NotAuthenticated { method, code } => SessionError::Rejected {
                method,
                code,
                reason: "not authenticated".into(),
            },
            RtspError::Status {
                method,
                code,
                reason,
            } => SessionError::Rejected {
                method,
                code,
                reason,
            },
            other @ (RtspError::Parse(_) | RtspError::Plist(_) | RtspError::Hap(_)) => {
                SessionError::Protocol(other.to_string())
            }
        }
    }
}

impl From<BodyError> for SessionError {
    fn from(err: BodyError) -> SessionError {
        SessionError::Protocol(err.to_string())
    }
}

impl From<PairingError> for SessionError {
    fn from(err: PairingError) -> SessionError {
        match err {
            PairingError::Transport(err) => err.into(),
            other => SessionError::Pairing(other.to_string()),
        }
    }
}

/// State the actor publishes for the synchronous getters.
#[derive(Default)]
struct Shared {
    end_reason: Mutex<Option<String>>,
    last_feedback: Mutex<Option<String>>,
}

impl Shared {
    /// Record why the session ended; the first reason wins.
    fn set_end_reason(&self, reason: String) {
        let mut slot = self.end_reason.lock();
        if slot.is_none() {
            warn!("session ending: {reason}");
            *slot = Some(reason);
        }
    }
}

/// Work items for the actor task.
enum Command {
    /// Send one RTSP request and hand back the response.
    Request {
        request: RtspRequest,
        reply: oneshot::Sender<Result<Response, RtspError>>,
    },
    /// Send the first POST /feedback now and keep sending one every 2 s.
    StartFeedback { reply: oneshot::Sender<()> },
    /// Send TEARDOWN (3 s timeout) and stop the feedback loop.
    Teardown { reply: oneshot::Sender<()> },
    /// Close the connection and stop the task.
    Close,
}

/// A connected (and later streaming) AirPlay 2 session.
pub struct Session {
    config: SessionConfig,
    receiver: ReceiverInfo,
    local_ip: Ipv4Addr,
    ctx: RtspContext,
    secret: SharedSecret,
    pairing_retries: u32,
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    actor: Option<JoinHandle<()>>,
    event_channel: Option<EventChannel>,
    skip_record: bool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("receiver", &self.receiver.address)
            .field("local_ip", &self.local_ip)
            .field("session_id", &self.ctx.session_id)
            .field("pairing_retries", &self.pairing_retries)
            .finish_non_exhaustive()
    }
}

fn closed_error() -> SessionError {
    SessionError::Io(std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "the session task has stopped",
    ))
}

/// Base SETUP timing for PTP: `ClockPorts` (every receiver ID → our port) only
/// when the follower could not bind the standard ports (`clock_port` is `None` on
/// 319/320).
fn ptp_base_timing(
    local_ip: Ipv4Addr,
    clock_port: Option<u16>,
    receiver_ids: &[String],
) -> BaseTiming<'_> {
    BaseTiming::Ptp {
        local_ip,
        clock_ports: clock_port.map(|port| ClockPorts { receiver_ids, port }),
    }
}

/// Header value `seq=<S>;rtptime=<T>`.
fn rtp_info(params: &StreamParams) -> String {
    format!("seq={};rtptime={}", params.first_seq, params.first_ts)
}

impl Session {
    pub async fn connect(
        info: &ReceiverInfo,
        config: SessionConfig,
    ) -> Result<Session, SessionError> {
        if !info.supports_transient_pairing() {
            return Err(SessionError::Pairing(format!(
                "{} does not support transient (HAP) pairing",
                info.name
            )));
        }
        let addr = SocketAddrV4::new(info.address, info.raop_port);
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| SessionError::Timeout(format!("connect to {addr}")))??;
        if let Err(err) = stream.set_nodelay(true) {
            debug!("TCP_NODELAY: {err}");
        }
        let local_ip = match stream.local_addr()? {
            SocketAddr::V4(local) => *local.ip(),
            SocketAddr::V6(local) => local.ip().to_ipv4_mapped().ok_or_else(|| {
                SessionError::Protocol(format!("connected from non-IPv4 address {local}"))
            })?,
        };
        let ctx = RtspContext::random(local_ip)
            .map_err(|err| SessionError::Io(std::io::Error::other(err.to_string())))?;
        info!(receiver = %addr, %local_ip, "connected");
        let mut client = RtspClient::new(stream, ctx.clone());

        let response = client
            .exchange(&RtspRequest::new("GET").uri("/info").allow_error())
            .await?;
        if response.code == 200 {
            match response.body_plist() {
                Ok(plist) => debug!("receiver /info: {}", bodies::describe_plist(&plist)),
                Err(err) => debug!("receiver /info is not a plist: {err}"),
            }
        } else {
            debug!(code = response.code, "receiver does not support /info");
        }

        let mut retries = 0;
        let secret = loop {
            match hap::transient_pair_setup(&mut client).await {
                Ok(secret) => break secret,
                Err(err) if err.is_retryable() && retries < MAX_PAIRING_RETRIES => {
                    retries += 1;
                    warn!(retry = retries, "pair-setup failed: {err}; retrying");
                }
                Err(err) if err.is_retryable() => {
                    return Err(SessionError::Pairing(format!(
                        "{err} (gave up after {retries} retries)"
                    )));
                }
                Err(err) => return Err(err.into()),
            }
        };
        info!(retries, "paired (transient)");
        client.enable_encryption(&secret.control_keys());

        let shared = Arc::new(Shared::default());
        let (commands, inbox) = mpsc::channel(8);
        let actor = tokio::spawn(run_actor(client, inbox, shared.clone()));
        Ok(Session {
            config,
            receiver: info.clone(),
            local_ip,
            ctx,
            secret,
            pairing_retries: retries,
            shared,
            commands,
            actor: Some(actor),
            event_channel: None,
            skip_record: false,
        })
    }

    /// Local address of the RTSP connection (bind UDP sockets here).
    pub fn local_ip(&self) -> Ipv4Addr {
        self.local_ip
    }

    /// Pair-setup attempts that had to be retried (should be 0).
    pub fn pairing_retries(&self) -> u32 {
        self.pairing_retries
    }

    pub async fn setup(
        &mut self,
        timing: TimingSetup,
        control_port: u16,
    ) -> Result<StreamParams, SessionError> {
        let remote = self.receiver.address;
        let session_uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
        let base_timing = match &timing {
            TimingSetup::Ptp(ptp) => {
                let clock_port = (!ptp.standard_ports()).then(|| ptp.port());
                ptp_base_timing(self.local_ip, clock_port, &self.receiver.receiver_ids)
            }
            TimingSetup::Ntp { timing_port } => BaseTiming::Ntp {
                timing_port: *timing_port,
            },
        };
        let body = bodies::base_setup_body(&BaseSetup {
            device_id: &self.config.device_id,
            session_uuid: &session_uuid,
            sender_name: &self.config.sender_name,
            timing: base_timing,
        });
        let response = self.request(RtspRequest::new("SETUP").plist(body)).await?;
        let base_plist = response.body_plist()?;
        debug!(
            "base SETUP response: {}",
            bodies::describe_plist(&base_plist)
        );
        let base = bodies::parse_base_setup_response(&base_plist)?;
        self.skip_record = base.skip_record;

        if base.event_port == 0 {
            return Err(SessionError::Protocol(
                "base SETUP response has no eventPort".into(),
            ));
        }
        let channel = EventChannel::connect_with_retry(
            SocketAddrV4::new(remote, base.event_port),
            self.secret.event_keys(),
        )
        .await?;
        self.event_channel = Some(channel);

        let shk = self.secret.shk();
        let body = bodies::stream_setup_body(
            self.config.codec,
            self.config.latency_ms,
            control_port,
            &shk,
            self.ctx.session_id,
        );
        let response = self.request(RtspRequest::new("SETUP").plist(body)).await?;
        let stream_plist = response.body_plist()?;
        debug!(
            "stream SETUP response: {}",
            bodies::describe_plist(&stream_plist)
        );
        let stream = bodies::parse_stream_setup_response(&stream_plist)?;

        let protocol = match timing {
            TimingSetup::Ptp(ptp) => {
                ptp.follow(remote, base.clock_id);
                let locked = tokio::task::spawn_blocking(move || ptp.wait_locked(PTP_LOCK_TIMEOUT))
                    .await
                    .map_err(|err| {
                        SessionError::Protocol(format!("PTP lock wait failed: {err}"))
                    })?;
                if !locked {
                    return Err(SessionError::ClockNotLocked);
                }
                info!("PTP clock locked");
                TimingProtocol::Ptp
            }
            TimingSetup::Ntp { .. } => TimingProtocol::Ntp,
        };

        let render_ms = stream.render_ms.unwrap_or(DEFAULT_RENDER_MS);
        if self.config.latency_ms < render_ms + SAFETY_MS {
            warn!(
                "latency {} ms is below what {} needs (~{} ms); expect dropouts",
                self.config.latency_ms,
                self.receiver.name,
                render_ms + SAFETY_MS
            );
        }
        let first_seq = getrandom::u32()
            .map_err(|err| SessionError::Io(std::io::Error::other(err.to_string())))?
            as u16;
        let frames = latency_frames(self.config.latency_ms);
        let params = StreamParams {
            receiver: remote,
            local_ip: self.local_ip,
            data_port: stream.data_port,
            control_port: stream.control_port,
            shk,
            codec: self.config.codec,
            timing: protocol,
            latency_ms: self.config.latency_ms,
            latency_frames: frames,
            render_ms: stream.render_ms,
            clock_id: base.clock_id,
            ssrc: self.ctx.session_id,
            first_seq,
            first_ts: u64::from(frames),
        };
        info!(
            data_port = params.data_port,
            control_port = params.control_port,
            render_ms = ?params.render_ms,
            "stream negotiated"
        );
        Ok(params)
    }

    pub async fn start_playback(&mut self, params: &StreamParams) -> Result<(), SessionError> {
        if self.receiver.supports_text_metadata() {
            let body =
                bodies::dmap_metadata(&self.config.title, DMAP_ALBUM, &self.config.sender_name);
            let request = RtspRequest::new("SET_PARAMETER")
                .bytes("application/x-dmap-tagged", body)
                .header("Session", "0")
                .header("RTP-Info", rtp_info(params));
            if let Err(err) = self.request(request).await {
                debug!("metadata not accepted: {err}");
            }
        }

        let mut feedback_started = false;
        if !self.skip_record {
            self.request(RtspRequest::new("RECORD")).await?;
            self.start_feedback().await?;
            feedback_started = true;
        }
        let flush = RtspRequest::new("FLUSH")
            .header("Range", "npt=0-")
            .header("Session", "0")
            .header("RTP-Info", rtp_info(params));
        if let Err(err) = self.request(flush).await {
            debug!("FLUSH not accepted: {err}");
        }
        if !feedback_started {
            self.start_feedback().await?;
        }

        if let Some(pct) = self.config.volume_pct {
            self.set_volume(pct).await?;
        }
        info!(latency_ms = params.latency_ms, "playback started");
        Ok(())
    }

    pub async fn set_volume(&self, pct: f32) -> Result<(), SessionError> {
        let value = bodies::volume_value(bodies::pct_from_f32(pct));
        debug!(pct, %value, "set volume");
        let request = RtspRequest::new("SET_PARAMETER")
            .bytes("text/parameters", format!("volume: {value}").into_bytes());
        self.request(request).await.map(|_| ())
    }

    /// Why the session is no longer usable (connection closed, feedback failing).
    pub fn end_reason(&self) -> Option<String> {
        self.shared.end_reason.lock().clone()
    }

    /// Latest POST /feedback response body, decoded for diagnostics.
    pub fn last_feedback(&self) -> Option<String> {
        self.shared.last_feedback.lock().clone()
    }

    pub async fn teardown(mut self) {
        let (reply, done) = oneshot::channel();
        if self
            .commands
            .send(Command::Teardown { reply })
            .await
            .is_ok()
        {
            // An Err only means the task already stopped.
            let _ = done.await;
        }
        if let Some(channel) = self.event_channel.take() {
            channel.close().await;
        }
        if self.commands.send(Command::Close).await.is_err() {
            debug!("session task already stopped");
        }
        if let Some(actor) = self.actor.take()
            && let Err(err) = actor.await
        {
            warn!("session task failed: {err}");
        }
        info!("session closed");
    }

    /// Run one request through the actor.
    async fn request(&self, request: RtspRequest) -> Result<Response, SessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Request { request, reply })
            .await
            .map_err(|_| closed_error())?;
        Ok(response.await.map_err(|_| closed_error())??)
    }

    /// Send the first feedback and start the 2 s loop.
    async fn start_feedback(&self) -> Result<(), SessionError> {
        let (reply, done) = oneshot::channel();
        self.commands
            .send(Command::StartFeedback { reply })
            .await
            .map_err(|_| closed_error())?;
        done.await.map_err(|_| closed_error())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Without teardown(): stop the task, which closes the connection.
        if let Some(actor) = self.actor.take() {
            actor.abort();
        }
    }
}

/// Feedback loop bookkeeping inside the actor.
#[derive(Default)]
struct Feedback {
    /// When the next POST /feedback is due (`None` = loop not running).
    next: Option<Instant>,
    /// Consecutive failures so far.
    failures: u32,
}

/// Sleep until `deadline`, or forever when there is none.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Record a lost connection as the end reason.
fn note_connection<S>(client: &RtspClient<S>, err: &RtspError, shared: &Shared)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if client.is_closed() {
        let reason = match err {
            RtspError::Closed => "receiver closed the connection".to_string(),
            other => format!("connection to the receiver failed: {other}"),
        };
        shared.set_end_reason(reason);
    }
}

async fn send_feedback(client: &mut RtspClient, feedback: &mut Feedback, shared: &Shared) {
    match client
        .exchange(&RtspRequest::new("POST").uri("/feedback"))
        .await
    {
        Ok(response) => {
            feedback.failures = 0;
            if !response.body.is_empty() {
                match response.body_plist() {
                    Ok(plist) => {
                        *shared.last_feedback.lock() = Some(bodies::describe_plist(&plist))
                    }
                    Err(err) => debug!("feedback body is not a plist: {err}"),
                }
            }
        }
        Err(err) => {
            feedback.failures += 1;
            debug!(failures = feedback.failures, "feedback failed: {err}");
            note_connection(client, &err, shared);
            if feedback.failures >= FEEDBACK_FAILURE_LIMIT {
                shared.set_end_reason(format!(
                    "receiver stopped answering feedback ({} failures, last: {err})",
                    feedback.failures
                ));
            }
        }
    }
    feedback.next = (!client.is_closed()).then(|| Instant::now() + FEEDBACK_INTERVAL);
}

/// The actor: owns the connection and serialises every request.
async fn run_actor(
    mut client: RtspClient,
    mut inbox: mpsc::Receiver<Command>,
    shared: Arc<Shared>,
) {
    let mut feedback = Feedback::default();
    // Set by Teardown: the receiver may now close the connection, which is no
    // longer a reason to report.
    let mut stopping = false;
    loop {
        let reading = !client.is_closed() && !stopping;
        tokio::select! {
            command = inbox.recv() => match command {
                Some(Command::Request { request, reply }) => {
                    let result = client.exchange(&request).await;
                    if let Err(err) = &result
                        && !stopping
                    {
                        note_connection(&client, err, &shared);
                    }
                    if reply.send(result).is_err() {
                        debug!("request caller went away");
                    }
                }
                Some(Command::StartFeedback { reply }) => {
                    send_feedback(&mut client, &mut feedback, &shared).await;
                    // The caller may have been cancelled; nothing to do then.
                    let _ = reply.send(());
                }
                Some(Command::Teardown { reply }) => {
                    feedback.next = None;
                    stopping = true;
                    if client.is_closed() {
                        debug!("connection already closed; no TEARDOWN");
                    } else {
                        let request = RtspRequest::new("TEARDOWN").header("Session", "0");
                        let exchange = client.exchange(&request);
                        match tokio::time::timeout(TEARDOWN_TIMEOUT, exchange).await {
                            Ok(Ok(_)) => debug!("TEARDOWN accepted"),
                            Ok(Err(err)) => debug!("TEARDOWN failed: {err}"),
                            Err(_) => debug!("TEARDOWN timed out"),
                        }
                    }
                    // The caller may have been cancelled; nothing to do then.
                    let _ = reply.send(());
                }
                Some(Command::Close) | None => break,
            },
            () = sleep_until(feedback.next), if feedback.next.is_some() => {
                send_feedback(&mut client, &mut feedback, &shared).await;
            }
            result = client.read_some(), if reading => {
                if let Err(err) = result {
                    note_connection(&client, &err, &shared);
                    feedback.next = None;
                }
            }
        }
    }
    client.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptp_timing_adds_clock_ports_only_off_the_standard_ports() {
        let ids = vec!["0E0E".to_string(), "0F0F".to_string()];
        let ip = Ipv4Addr::new(192, 168, 1, 57);
        let standard = ptp_base_timing(ip, None, &ids);
        let BaseTiming::Ptp { clock_ports, .. } = standard else {
            panic!("expected PTP timing");
        };
        assert!(clock_ports.is_none());

        let dynamic = ptp_base_timing(ip, Some(50_123), &ids);
        let body = bodies::base_setup_body(&BaseSetup {
            device_id: "02:11:22:33:44:55",
            session_uuid: "2B5E7C1A-0000-4000-8000-0000000000AA",
            sender_name: "WinPods",
            timing: dynamic,
        });
        let dict = body.as_dictionary().unwrap();
        assert_eq!(dict["timingProtocol"].as_string(), Some("PTP"));
        let peer = dict["timingPeerInfo"].as_dictionary().unwrap();
        assert_eq!(
            peer["ID"].as_string(),
            Some("E0B44A76-E30B-53A3-BF25-6D9E0C0452BD")
        );
        let ports = peer["ClockPorts"].as_dictionary().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports["0F0F"].as_unsigned_integer(), Some(50_123));
        assert!(dict.get("timingPort").is_none());
    }

    #[test]
    fn rtp_info_uses_first_seq_and_ts() {
        let params = StreamParams {
            receiver: Ipv4Addr::LOCALHOST,
            local_ip: Ipv4Addr::LOCALHOST,
            data_port: 1,
            control_port: 2,
            shk: [0; 32],
            codec: Codec::Alac,
            timing: TimingProtocol::Ptp,
            latency_ms: 100,
            latency_frames: 4410,
            render_ms: Some(69),
            clock_id: None,
            ssrc: 7,
            first_seq: 12345,
            first_ts: 4410,
        };
        assert_eq!(rtp_info(&params), "seq=12345;rtptime=4410");
    }

    #[test]
    fn rtsp_errors_map_to_session_errors() {
        assert!(matches!(
            SessionError::from(RtspError::Status {
                method: "SETUP".into(),
                code: 500,
                reason: "Internal".into()
            }),
            SessionError::Rejected { code: 500, .. }
        ));
        assert!(matches!(
            SessionError::from(RtspError::Timeout("SETUP".into())),
            SessionError::Timeout(_)
        ));
        assert!(matches!(
            SessionError::from(RtspError::Closed),
            SessionError::Io(_)
        ));
        assert!(matches!(
            SessionError::from(PairingError::Rejected("x".into())),
            SessionError::Pairing(_)
        ));
        assert!(matches!(
            SessionError::from(BodyError::Missing("dataPort")),
            SessionError::Protocol(_)
        ));
    }

    #[test]
    fn session_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Session>();
    }

    #[test]
    fn end_reason_keeps_the_first_cause() {
        let shared = Shared::default();
        shared.set_end_reason("first".into());
        shared.set_end_reason("second".into());
        assert_eq!(shared.end_reason.lock().as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn connect_refuses_receivers_without_transient_pairing() {
        let info = ReceiverInfo {
            name: "Old".into(),
            model: String::new(),
            address: Ipv4Addr::LOCALHOST,
            raop_port: 9,
            features: 0,
            metadata_types: Vec::new(),
            encryption_types: Vec::new(),
            receiver_ids: Vec::new(),
            requires_password: false,
            raop_txt: Vec::new(),
            airplay_txt: Vec::new(),
        };
        let config = SessionConfig {
            sender_name: "WinPods".into(),
            device_id: "02:11:22:33:44:55".into(),
            latency_ms: 100,
            codec: Codec::Alac,
            volume_pct: None,
            title: "Windows audio".into(),
        };
        let err = Session::connect(&info, config).await.unwrap_err();
        assert!(matches!(err, SessionError::Pairing(_)), "{err}");
    }
}
