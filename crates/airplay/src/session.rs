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
//!    (TEARDOWN `Session: 0`; the whole call finishes within 3 s).
//!
//! Internally one task owns the RTSP connection; volume, feedback and teardown go
//! through it. Read EOF or 3 consecutive feedback failures set
//! [`Session::end_reason`].
//!
//! Implementation notes:
//!
//! * The actor task is spawned at the end of `connect` (after pairing) and owns the
//!   [`RtspClient`]. Every later request is a command answered over a oneshot. The
//!   task writes each request as soon as it gets it and matches responses by CSeq,
//!   like pyatv (which writes at once and waits per CSeq), so an unanswered POST
//!   /feedback never holds up FLUSH, a volume change or TEARDOWN. Each request has
//!   its own deadline ([`REQUEST_TIMEOUT`], [`TEARDOWN_TIMEOUT`] for TEARDOWN). The
//!   task always reads the socket, which is also how a closed connection is noticed
//!   between requests.
//! * Request order matches pyatv's: pyatv's feedback task first runs while the
//!   request after `start_feedback()` (RECORD, or FLUSH with `skipRecord`) awaits its
//!   reply, so its POST /feedback takes the next CSeq. Here the first feedback is
//!   written right after that request completes and the next request follows without
//!   waiting for the feedback's reply, which yields the same CSeq sequence (metadata,
//!   RECORD, feedback, FLUSH, volume — `rtsp_requests.json`).
//! * Like pyatv's loop, the next feedback is due 2 s after the previous one was
//!   answered or failed, so at most one is outstanding. Failures are counted per run
//!   of consecutive failures; the third one sets the end reason (pyatv only logged
//!   them).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, trace, warn};

use crate::bodies::{self, BaseSetup, BaseTiming, BodyError, ClockPorts};
use crate::event_channel::EventChannel;
use crate::hap::{self, PairingError, SharedSecret};
use crate::ptp::PtpService;
use crate::rtsp::{
    self, REQUEST_TIMEOUT, Response, RtspClient, RtspContext, RtspError, RtspRequest,
};
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
/// Upper bound for [`Session::teardown`], TEARDOWN exchange included.
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
    /// Write the first POST /feedback now (answered once it is written) and keep
    /// sending one every 2 s.
    StartFeedback { reply: oneshot::Sender<()> },
    /// Stop the feedback loop and send TEARDOWN; answered when its response
    /// arrives, it fails, or `deadline` passes.
    Teardown {
        deadline: Instant,
        reply: oneshot::Sender<()>,
    },
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

    /// Send TEARDOWN, then close the event channel and the connection. The whole
    /// call is bounded by [`TEARDOWN_TIMEOUT`]: if the receiver has not answered by
    /// then (or the session task is stuck), the connection is simply dropped.
    pub async fn teardown(mut self) {
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        let (reply, done) = oneshot::channel();
        let commands = &self.commands;
        let answered = tokio::time::timeout_at(deadline, async move {
            if commands
                .send(Command::Teardown { deadline, reply })
                .await
                .is_ok()
            {
                // An Err only means the task already stopped.
                let _ = done.await;
            }
        })
        .await
        .is_ok();
        if !answered {
            debug!("TEARDOWN not finished in {TEARDOWN_TIMEOUT:?}; dropping the connection");
            if let Some(actor) = &self.actor {
                actor.abort();
            }
        }
        if let Some(channel) = self.event_channel.take() {
            channel.close().await;
        }
        if let Some(actor) = self.actor.take() {
            // The task is idle after TEARDOWN, so Close is handled at once; a full
            // queue would mean it is stuck, and then it is stopped instead.
            match self.commands.try_send(Command::Close) {
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                Err(mpsc::error::TrySendError::Full(_)) => actor.abort(),
            }
            match actor.await {
                Err(err) if !err.is_cancelled() => warn!("session task failed: {err}"),
                _ => {}
            }
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

    /// Write the first feedback (its response is not awaited) and start the 2 s
    /// loop.
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
    /// When the next POST /feedback is due (`None` = none scheduled: the loop is
    /// not running, or one is waiting for its response).
    next: Option<Instant>,
    /// Consecutive failures so far.
    failures: u32,
}

/// Who is waiting for the response to a request on the wire.
enum Waiter {
    /// A [`Command::Request`] caller.
    Request(oneshot::Sender<Result<Response, RtspError>>),
    /// The feedback loop.
    Feedback,
    /// [`Session::teardown`].
    Teardown(oneshot::Sender<()>),
}

/// A request that was written and has not been answered yet.
struct Pending {
    cseq: u32,
    method: String,
    allow_error: bool,
    /// `"<METHOD> <uri> (CSeq <n>)"` for the timeout error.
    label: String,
    /// When the request fails with a timeout.
    deadline: Instant,
    waiter: Waiter,
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

/// The error handed to every request still waiting when the connection failed
/// (`RtspError` is not `Clone`).
fn connection_error(err: &RtspError) -> RtspError {
    match err {
        RtspError::Closed => RtspError::Closed,
        RtspError::Io(io) => RtspError::Io(std::io::Error::new(io.kind(), io.to_string())),
        other => RtspError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            other.to_string(),
        )),
    }
}

/// Spawned by [`Session::connect`]: owns the connection until `Close`, the end of
/// the command channel, or an abort.
async fn run_actor(client: RtspClient, inbox: mpsc::Receiver<Command>, shared: Arc<Shared>) {
    let actor = Actor {
        client,
        shared,
        pending: Vec::new(),
        feedback: Feedback::default(),
        stopping: false,
    };
    actor.run(inbox).await;
}

/// The session task: writes requests as commands arrive and matches responses to
/// them by CSeq, so requests are pipelined like pyatv's and nothing waits behind a
/// slow response.
struct Actor {
    client: RtspClient,
    shared: Arc<Shared>,
    /// Requests on the wire (a handful at most: feedback, a command, TEARDOWN).
    pending: Vec<Pending>,
    feedback: Feedback,
    /// Set by Teardown: the receiver may now close the connection, which is no
    /// longer a reason to report, and feedback results no longer matter.
    stopping: bool,
}

impl Actor {
    async fn run(mut self, mut inbox: mpsc::Receiver<Command>) {
        loop {
            let deadline = self.pending.iter().map(|p| p.deadline).min();
            let feedback_due = self.feedback.next;
            // Every read error closes the client, so this cannot spin.
            let reading = !self.client.is_closed();
            tokio::select! {
                command = inbox.recv() => match command {
                    Some(Command::Close) | None => break,
                    Some(command) => self.handle(command).await,
                },
                () = sleep_until(feedback_due), if feedback_due.is_some() => {
                    self.send_feedback().await;
                }
                () = sleep_until(deadline), if deadline.is_some() => {
                    self.expire(Instant::now());
                }
                // Cancel-safe, so a command or timer can interrupt it at any point.
                result = self.client.next_response(), if reading => match result {
                    Ok(response) => self.dispatch(response),
                    Err(err) => self.connection_lost(&err),
                },
            }
        }
        self.client.shutdown().await;
    }

    /// Act on one command (`Close` is handled by the loop).
    async fn handle(&mut self, command: Command) {
        match command {
            Command::Request { request, reply } => {
                let deadline = Instant::now() + REQUEST_TIMEOUT;
                self.start(&request, deadline, Waiter::Request(reply)).await;
            }
            Command::StartFeedback { reply } => {
                let outstanding = self
                    .pending
                    .iter()
                    .any(|p| matches!(p.waiter, Waiter::Feedback));
                if !outstanding {
                    self.send_feedback().await;
                }
                // The caller may have been cancelled; nothing to do then.
                let _ = reply.send(());
            }
            Command::Teardown { deadline, reply } => {
                self.feedback.next = None;
                self.stopping = true;
                if self.client.is_closed() {
                    debug!("connection already closed; no TEARDOWN");
                    let _ = reply.send(());
                } else {
                    let request = RtspRequest::new("TEARDOWN").header("Session", "0");
                    self.start(&request, deadline, Waiter::Teardown(reply))
                        .await;
                }
            }
            // Stops the loop in `run` before it gets here.
            Command::Close => {}
        }
    }

    /// Write `request` and remember who waits for its response; a failed write
    /// answers the waiter at once.
    async fn start(&mut self, request: &RtspRequest, deadline: Instant, waiter: Waiter) {
        match self.client.send_request(request).await {
            Ok(cseq) => self.pending.push(Pending {
                cseq,
                method: request.method.clone(),
                allow_error: request.allow_error,
                label: request.describe(cseq),
                deadline,
                waiter,
            }),
            Err(err) => {
                if !self.stopping {
                    note_connection(&self.client, &err, &self.shared);
                }
                self.complete(waiter, Err(err));
            }
        }
    }

    /// Write the next POST /feedback; the one after is scheduled when it finishes.
    async fn send_feedback(&mut self) {
        self.feedback.next = None;
        let request = RtspRequest::new("POST").uri("/feedback");
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        self.start(&request, deadline, Waiter::Feedback).await;
    }

    /// Hand a response to the request with its CSeq.
    fn dispatch(&mut self, response: Response) {
        let cseq = response.cseq();
        let Some(index) = self.pending.iter().position(|p| Some(p.cseq) == cseq) else {
            debug!(?cseq, "skipping response without a waiting request");
            return;
        };
        let pending = self.pending.remove(index);
        trace!(code = response.code, cseq = pending.cseq, "RTSP response");
        let result = rtsp::check_status(&pending.method, response, pending.allow_error);
        self.complete(pending.waiter, result);
    }

    /// Fail every request whose deadline has passed.
    fn expire(&mut self, now: Instant) {
        while let Some(index) = self.pending.iter().position(|p| p.deadline <= now) {
            let pending = self.pending.remove(index);
            self.complete(pending.waiter, Err(RtspError::Timeout(pending.label)));
        }
    }

    /// The connection failed or was closed: fail everything still waiting.
    fn connection_lost(&mut self, err: &RtspError) {
        if !self.stopping {
            note_connection(&self.client, err, &self.shared);
        }
        self.feedback.next = None;
        for pending in std::mem::take(&mut self.pending) {
            self.complete(pending.waiter, Err(connection_error(err)));
        }
    }

    /// Deliver the outcome of a request to whoever waits for it.
    fn complete(&mut self, waiter: Waiter, result: Result<Response, RtspError>) {
        match waiter {
            Waiter::Request(reply) => {
                if reply.send(result).is_err() {
                    debug!("request caller went away");
                }
            }
            Waiter::Feedback => self.feedback_done(result),
            Waiter::Teardown(reply) => {
                match &result {
                    Ok(_) => debug!("TEARDOWN accepted"),
                    Err(err) => debug!("TEARDOWN failed: {err}"),
                }
                // The caller may have given up; nothing to do then.
                let _ = reply.send(());
            }
        }
    }

    /// Account for one feedback round trip and schedule the next one.
    fn feedback_done(&mut self, result: Result<Response, RtspError>) {
        if self.stopping {
            return;
        }
        match result {
            Ok(response) => {
                self.feedback.failures = 0;
                if !response.body.is_empty() {
                    match response.body_plist() {
                        Ok(plist) => {
                            *self.shared.last_feedback.lock() =
                                Some(bodies::describe_plist(&plist));
                        }
                        Err(err) => debug!("feedback body is not a plist: {err}"),
                    }
                }
            }
            Err(err) => {
                self.feedback.failures += 1;
                debug!(failures = self.feedback.failures, "feedback failed: {err}");
                if self.feedback.failures >= FEEDBACK_FAILURE_LIMIT {
                    self.shared.set_end_reason(format!(
                        "receiver stopped answering feedback ({} failures, last: {err})",
                        self.feedback.failures
                    ));
                }
            }
        }
        self.feedback.next = (!self.client.is_closed()).then(|| Instant::now() + FEEDBACK_INTERVAL);
    }
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
    fn connection_errors_are_copied_for_every_waiter() {
        assert!(matches!(
            connection_error(&RtspError::Closed),
            RtspError::Closed
        ));
        let reset = RtspError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert!(matches!(
            connection_error(&reset),
            RtspError::Io(err) if err.kind() == std::io::ErrorKind::ConnectionReset
        ));
        assert!(matches!(
            connection_error(&RtspError::Parse("bad".into())),
            RtspError::Io(err) if err.kind() == std::io::ErrorKind::InvalidData
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
