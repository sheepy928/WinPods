//! One connection attempt, run as its own task so the supervisor keeps handling
//! commands (Disconnect, Quit, Find, setting changes) while it is under way.
//!
//! Order: local address of the route to the receiver → PTP follower bound there
//! (UDP 319/320 taken = fail at once, before the receiver is contacted) → discovery
//! (unless cached) → `Session::connect` → control socket / NTP responder →
//! `Session::setup` → `Session::start_playback`.
//!
//! Every network step races the cancel signal. Before a session exists a cancelled
//! attempt just ends (dropping the half-open connection); once one exists it is torn
//! down first (TEARDOWN, bounded by `session::TEARDOWN_TIMEOUT`). The ring consumer
//! never enters the task, so a cancelled or failed attempt cannot lose it: the
//! engine calls `runtime::start` with the pieces returned in [`Connected`].

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use winpods_airplay::ntp::{NtpClock, TimingResponder};
use winpods_airplay::ptp::PtpService;
use winpods_airplay::runtime::{ControlSocket, RuntimeOptions};
use winpods_airplay::session::{Session, SessionConfig, TimingSetup};
use winpods_airplay::{ReceiverInfo, StreamParams, TimingProtocol, mdns};

/// Error when the PTP follower cannot have the standard ports (HomePods reject the
/// dynamic-port fallback).
pub const PTP_PORTS_BUSY: &str = "UDP ports 319/320 are in use by another program (is WinPods \
                                  v0.1 or another AirPlay sender running?)";

/// How long discovery waits for the receiver's mDNS answer.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an attempt waits for the previous attempt's PTP follower to let go of
/// UDP 319/320 (a cancelled lock wait keeps it for up to `PTP_LOCK_TIMEOUT`).
const PTP_RELEASE_WAIT: Duration = Duration::from_secs(6);
const PTP_RELEASE_POLL: Duration = Duration::from_millis(50);

/// Port used only to ask the routing table for the local address (nothing is sent).
const ROUTE_PROBE_PORT: u16 = 7000;

/// Local IPv4 address Windows would use to reach `remote`. Connecting a UDP socket
/// sends nothing; it only picks the route.
pub fn route_local_ip(remote: Ipv4Addr) -> io::Result<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect((remote, ROUTE_PROBE_PORT))?;
    match socket.local_addr()? {
        SocketAddr::V4(local) if !local.ip().is_unspecified() => Ok(*local.ip()),
        other => Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("no IPv4 route to {remote} (local address {other})"),
        )),
    }
}

/// Start the PTP follower on `local_ip`. Without the standard ports it fails with
/// [`PTP_PORTS_BUSY`] when `require_standard` is set, and otherwise keeps the
/// dynamic port (logged).
pub fn start_ptp(local_ip: Ipv4Addr, require_standard: bool) -> Result<Arc<PtpService>, String> {
    let ptp = PtpService::start(local_ip).map_err(|e| e.to_string())?;
    if !ptp.standard_ports() {
        if require_standard {
            return Err(PTP_PORTS_BUSY.to_string());
        }
        warn!("{PTP_PORTS_BUSY}; using port {}", ptp.port());
    }
    Ok(ptp)
}

/// Engine side of the cancel signal.
pub(crate) struct Canceller(watch::Sender<bool>);

/// Task side of the cancel signal. Dropping the [`Canceller`] also cancels.
pub(crate) struct CancelToken(watch::Receiver<bool>);

pub(crate) fn cancel_pair() -> (Canceller, CancelToken) {
    let (tx, rx) = watch::channel(false);
    (Canceller(tx), CancelToken(rx))
}

impl Canceller {
    pub(crate) fn cancel(&self) {
        self.0.send_replace(true);
    }
}

impl CancelToken {
    /// Resolves once cancelled (or once the canceller is gone).
    async fn cancelled(&mut self) {
        // Err = the sender was dropped, which also means "stop".
        let _ = self.0.wait_for(|cancelled| *cancelled).await;
    }

    /// Run `step` unless the attempt is cancelled first (`None`).
    pub(crate) async fn run<F: Future>(&mut self, step: F) -> Option<F::Output> {
        tokio::select! {
            biased;
            () = self.cancelled() => None,
            out = step => Some(out),
        }
    }
}

/// Weak handle to the newest PTP follower an attempt started, so the next attempt
/// can wait until its ports are free again.
pub(crate) type PtpSlot = Arc<Mutex<Weak<PtpService>>>;

/// Everything an attempt needs; owned, so the task borrows nothing from the engine.
pub(crate) struct ConnectRequest {
    pub address: Ipv4Addr,
    /// Cached discovery result (used when its address matches).
    pub receiver: Option<ReceiverInfo>,
    pub config: SessionConfig,
    pub timing: TimingProtocol,
    pub require_standard_ptp_ports: bool,
    pub drop_every: Option<u32>,
    pub ptp_slot: PtpSlot,
}

/// A negotiated, playing session plus what `runtime::start` needs.
pub(crate) struct Connected {
    pub session: Session,
    pub params: StreamParams,
    pub control: ControlSocket,
    pub options: RuntimeOptions,
    pub ptp: Option<Arc<PtpService>>,
    pub ntp: Option<TimingResponder>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectError {
    Failed(String),
    Cancelled,
}

pub(crate) struct ConnectOutcome {
    /// Set when this attempt ran discovery and found the receiver.
    pub discovered: Option<ReceiverInfo>,
    pub result: Result<Connected, ConnectError>,
}

/// Run one attempt to the end (success, failure or cancellation).
pub(crate) async fn connect(request: ConnectRequest, mut cancel: CancelToken) -> ConnectOutcome {
    let mut discovered = None;
    let result = attempt(&request, &mut cancel, &mut discovered).await;
    ConnectOutcome { discovered, result }
}

async fn attempt(
    req: &ConnectRequest,
    cancel: &mut CancelToken,
    discovered: &mut Option<ReceiverInfo>,
) -> Result<Connected, ConnectError> {
    use ConnectError::{Cancelled, Failed};

    let address = req.address;
    let route_ip = route_local_ip(address)
        .map_err(|e| Failed(format!("no network route to {address}: {e}")))?;
    // PTP first: when 319/320 are taken there is no point in pairing.
    let mut ptp = match req.timing {
        TimingProtocol::Ptp => Some(bind_ptp(route_ip, req, cancel).await?),
        TimingProtocol::Ntp => None,
    };

    let info = match &req.receiver {
        Some(info) if info.address == address => info.clone(),
        _ => {
            let found = cancel
                .run(mdns::lookup(address, LOOKUP_TIMEOUT))
                .await
                .ok_or(Cancelled)?
                .map_err(|err| Failed(format!("discovery failed: {err}")))?
                .ok_or_else(|| Failed(format!("No AirPlay receiver answered at {address}")))?;
            info!(name = %found.name, model = %found.model, "found receiver");
            *discovered = Some(found.clone());
            found
        }
    };
    if !info.is_airplay2() {
        return Err(Failed(format!(
            "{} is not an AirPlay 2 receiver",
            info.name
        )));
    }

    // Cancelled here = the half-open connection is simply dropped (no session yet).
    let mut session = cancel
        .run(Session::connect(&info, req.config.clone()))
        .await
        .ok_or(Cancelled)?
        .map_err(|e| Failed(e.to_string()))?;

    match negotiate(&mut session, route_ip, &mut ptp, req, cancel).await {
        Ok((params, control, options, ntp)) => Ok(Connected {
            session,
            params,
            control,
            options,
            ptp,
            ntp,
        }),
        Err(err) => {
            // Failed or cancelled with a session: always say goodbye (bounded).
            session.teardown().await;
            Err(err)
        }
    }
}

/// Wait for the previous follower to release the ports, then start a new one.
async fn bind_ptp(
    ip: Ipv4Addr,
    req: &ConnectRequest,
    cancel: &mut CancelToken,
) -> Result<Arc<PtpService>, ConnectError> {
    let previous = req.ptp_slot.lock().clone();
    if previous.strong_count() > 0 {
        debug!("waiting for the previous PTP follower to release its ports");
        let deadline = Instant::now() + PTP_RELEASE_WAIT;
        cancel
            .run(async {
                while previous.strong_count() > 0 && Instant::now() < deadline {
                    tokio::time::sleep(PTP_RELEASE_POLL).await;
                }
            })
            .await
            .ok_or(ConnectError::Cancelled)?;
    }
    let ptp = start_ptp(ip, req.require_standard_ptp_ports).map_err(ConnectError::Failed)?;
    *req.ptp_slot.lock() = Arc::downgrade(&ptp);
    Ok(ptp)
}

/// Sockets, SETUP and playback on a connected session.
async fn negotiate(
    session: &mut Session,
    route_ip: Ipv4Addr,
    ptp: &mut Option<Arc<PtpService>>,
    req: &ConnectRequest,
    cancel: &mut CancelToken,
) -> Result<
    (
        StreamParams,
        ControlSocket,
        RuntimeOptions,
        Option<TimingResponder>,
    ),
    ConnectError,
> {
    use ConnectError::{Cancelled, Failed};

    let local_ip = session.local_ip();
    if local_ip != route_ip && ptp.is_some() {
        warn!(%route_ip, %local_ip, "the receiver connection uses another local address; moving the PTP follower");
        *ptp = None; // release the ports before binding them again
        let moved = start_ptp(local_ip, req.require_standard_ptp_ports).map_err(Failed)?;
        *req.ptp_slot.lock() = Arc::downgrade(&moved);
        *ptp = Some(moved);
    }

    let control =
        ControlSocket::bind(local_ip).map_err(|e| Failed(format!("control socket: {e}")))?;
    let mut options = RuntimeOptions {
        drop_every: req.drop_every,
        ..Default::default()
    };
    let (timing, ntp) = match ptp {
        Some(ptp) => {
            options.ptp = Some(ptp.clock());
            (TimingSetup::Ptp(ptp.clone()), None)
        }
        None => {
            let clock = Arc::new(NtpClock::new());
            let responder = TimingResponder::start(local_ip, clock.clone())
                .map_err(|e| Failed(format!("timing socket: {e}")))?;
            options.ntp = Some(clock);
            (
                TimingSetup::Ntp {
                    timing_port: responder.port(),
                },
                Some(responder),
            )
        }
    };

    let params = cancel
        .run(session.setup(timing, control.port()))
        .await
        .ok_or(Cancelled)?
        .map_err(|e| Failed(e.to_string()))?;
    cancel
        .run(session.start_playback(&params))
        .await
        .ok_or(Cancelled)?
        .map_err(|e| Failed(e.to_string()))?;
    Ok((params, control, options, ntp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_to_loopback_is_loopback() {
        assert_eq!(
            route_local_ip(Ipv4Addr::LOCALHOST).expect("route"),
            Ipv4Addr::LOCALHOST
        );
    }

    #[tokio::test]
    async fn cancel_token_stops_a_pending_step() {
        let (canceller, mut token) = cancel_pair();
        {
            let step = token.run(std::future::pending::<()>());
            tokio::pin!(step);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut step)
                    .await
                    .is_err(),
                "still running before the cancel"
            );
            canceller.cancel();
            assert_eq!(step.await, None);
        }
        // Once cancelled, later steps do not start.
        assert_eq!(token.run(async { 1 }).await, None);
    }

    #[tokio::test]
    async fn dropping_the_canceller_cancels() {
        let (canceller, mut token) = cancel_pair();
        assert_eq!(token.run(async { 7 }).await, Some(7));
        drop(canceller);
        assert_eq!(token.run(std::future::pending::<()>()).await, None);
    }
}
