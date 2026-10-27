//! Event channel (workstream: control plane).
//!
//! TCP connection to the `eventPort` from the base SETUP response, HAP-framed with
//! the event keys (send = Events-Read key, receive = Events-Write key). The receiver
//! sends `POST /command RTSP/1.0` requests (~2 KB binary plists, spanning several
//! frames/reads); reply once per complete request with
//! `<proto>/<ver> 200 OK`, `Content-Length: 0`, `Audio-Latency: 0`, the request's
//! `Server` header echoed (else `WinPods/<version>`) and its `CSeq` echoed.
//! Parse errors are logged and the channel stays open. Port of
//! `pyatv/protocols/airplay/channels.py` EventChannel. Fixture: `event_channel.json`.
//!
//! Byte layout of a reply (pyatv `format_response`): the status line, then — only
//! when the request had no `Server` header — `Server: WinPods/<version>` (pyatv
//! sends `pyatv-www/0.18.0` here), then `Content-Length: 0`, `Audio-Latency: 0`,
//! the echoed `Server` and `CSeq` if present, and an empty line. On unparseable data
//! the buffered bytes are dropped (pyatv would spin on them forever).

use std::future::Future;
use std::net::SocketAddrV4;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::hap::{CipherKeys, hap_pair};
use crate::rtsp::{IncomingRequest, parse_request};

/// `Server` header we send when the receiver's request has none.
pub const DEFAULT_SERVER: &str = concat!("WinPods/", env!("CARGO_PKG_VERSION"));
/// Connection attempts while the receiver refuses (it may open the port late).
pub const CONNECT_ATTEMPTS: u32 = 5;
/// Pause between refused connection attempts.
pub const CONNECT_RETRY_DELAY: Duration = Duration::from_secs(1);
/// Time allowed for one connection attempt (a timeout is not retried).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The plaintext reply to one event channel request.
pub fn format_reply(request: &IncomingRequest, default_server: &str) -> Vec<u8> {
    let mut out = format!("{}/{} 200 OK\r\n", request.protocol, request.version);
    let server = request.headers.get("Server");
    if server.is_none() {
        out.push_str(&format!("Server: {default_server}\r\n"));
    }
    out.push_str("Content-Length: 0\r\nAudio-Latency: 0\r\n");
    if let Some(server) = server {
        out.push_str(&format!("Server: {server}\r\n"));
    }
    if let Some(cseq) = request.headers.get("CSeq") {
        out.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// Consume every complete request at the front of `buffer` and return one
/// plaintext reply per request (`EventChannel.handle_received`). An incomplete
/// request stays buffered; unparseable data is logged and discarded.
pub fn handle_received(buffer: &mut Vec<u8>, default_server: &str) -> Vec<Vec<u8>> {
    let mut replies = Vec::new();
    while !buffer.is_empty() {
        match parse_request(buffer) {
            Ok(Some((request, used))) => {
                buffer.drain(..used);
                debug!(
                    method = %request.method,
                    path = %request.path,
                    len = request.body.len(),
                    "event channel request"
                );
                replies.push(format_reply(&request, default_server));
            }
            Ok(None) => break,
            Err(err) => {
                warn!(
                    "dropping {} unparseable bytes on the event channel: {err}",
                    buffer.len()
                );
                buffer.clear();
                break;
            }
        }
    }
    replies
}

/// Retry `connect` while it fails with `ConnectionRefused`, up to `attempts` tries
/// with `delay` in between. Other errors are returned at once.
pub async fn retry_refused<T, F, Fut>(
    attempts: u32,
    delay: Duration,
    mut connect: F,
) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    let mut attempt = 1;
    loop {
        match connect().await {
            Err(err)
                if err.kind() == std::io::ErrorKind::ConnectionRefused && attempt < attempts =>
            {
                debug!(attempt, "event channel connect refused, retrying");
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
            other => return other,
        }
    }
}

/// Counters shared with the channel task.
#[derive(Default)]
struct ChannelState {
    requests: AtomicU64,
    open: AtomicBool,
}

/// A connected event channel; a background task answers the receiver's requests.
/// Dropping it closes the connection.
pub struct EventChannel {
    peer: SocketAddrV4,
    state: Arc<ChannelState>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for EventChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventChannel")
            .field("peer", &self.peer)
            .field("open", &self.is_open())
            .field("requests", &self.requests_handled())
            .finish()
    }
}

impl EventChannel {
    /// Connect once (within [`CONNECT_TIMEOUT`]) and start answering requests.
    pub async fn connect(peer: SocketAddrV4, keys: CipherKeys) -> std::io::Result<EventChannel> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(peer))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("event channel connect to {peer} timed out"),
                )
            })??;
        if let Err(err) = stream.set_nodelay(true) {
            debug!("event channel TCP_NODELAY: {err}");
        }
        debug!(%peer, "event channel connected");
        let state = Arc::new(ChannelState {
            requests: AtomicU64::new(0),
            open: AtomicBool::new(true),
        });
        let task = tokio::spawn(run(stream, keys, state.clone()));
        Ok(EventChannel {
            peer,
            state,
            task: Some(task),
        })
    }

    /// Connect, retrying [`CONNECT_ATTEMPTS`] times [`CONNECT_RETRY_DELAY`] apart
    /// while the receiver refuses the connection.
    pub async fn connect_with_retry(
        peer: SocketAddrV4,
        keys: CipherKeys,
    ) -> std::io::Result<EventChannel> {
        retry_refused(CONNECT_ATTEMPTS, CONNECT_RETRY_DELAY, || {
            EventChannel::connect(peer, keys.clone())
        })
        .await
    }

    /// The receiver's address.
    pub fn peer(&self) -> SocketAddrV4 {
        self.peer
    }

    /// Requests answered so far.
    pub fn requests_handled(&self) -> u64 {
        self.state.requests.load(Ordering::Relaxed)
    }

    /// False once the receiver closed the channel or it failed.
    pub fn is_open(&self) -> bool {
        self.state.open.load(Ordering::Relaxed)
    }

    /// Stop answering and close the connection.
    pub async fn close(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            // A cancelled task reports a JoinError; nothing else can come back.
            let _ = task.await;
        }
        self.state.open.store(false, Ordering::Relaxed);
    }
}

impl Drop for EventChannel {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Channel task: decrypt, answer every complete request, until EOF or an error.
async fn run(mut stream: TcpStream, keys: CipherKeys, state: Arc<ChannelState>) {
    let (mut writer, mut reader) = hap_pair(&keys);
    let mut raw = vec![0u8; 16 * 1024];
    let mut plain = Vec::new();
    loop {
        let n = match stream.read(&mut raw).await {
            Ok(0) => {
                debug!("event channel closed by the receiver");
                break;
            }
            Ok(n) => n,
            Err(err) => {
                debug!("event channel read failed: {err}");
                break;
            }
        };
        if let Err(err) = reader.decrypt(&raw[..n], &mut plain) {
            warn!("event channel: {err}; closing it");
            break;
        }
        for reply in handle_received(&mut plain, DEFAULT_SERVER) {
            let frames = match writer.encrypt(&reply) {
                Ok(frames) => frames,
                Err(err) => {
                    warn!("event channel: {err}; closing it");
                    state.open.store(false, Ordering::Relaxed);
                    return;
                }
            };
            if let Err(err) = stream.write_all(&frames).await {
                debug!("event channel write failed: {err}");
                state.open.store(false, Ordering::Relaxed);
                return;
            }
            state.requests.fetch_add(1, Ordering::Relaxed);
        }
    }
    state.open.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hap::HapSession;
    use serde_json::Value as Json;
    use std::net::Ipv4Addr;

    const PYATV_SERVER: &str = "pyatv-www/0.18.0";

    fn fixture() -> Json {
        serde_json::from_str(include_str!("../tests/fixtures/event_channel.json"))
            .expect("event_channel.json")
    }

    fn unhex(value: &Json) -> Vec<u8> {
        hex::decode(value.as_str().expect("hex string")).expect("valid hex")
    }

    fn hex_list(value: &Json) -> Vec<Vec<u8>> {
        value.as_array().unwrap().iter().map(unhex).collect()
    }

    #[test]
    fn replies_match_pyatv() {
        let fx = fixture();
        for case in fx["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut buffer = unhex(&case["input"]);
            let replies = handle_received(&mut buffer, PYATV_SERVER);
            assert_eq!(replies, hex_list(&case["responses"]), "{name}");
            assert_eq!(buffer, unhex(&case["buffer_after"]), "{name}");
        }
    }

    #[test]
    fn split_requests_wait_for_the_rest() {
        let fx = fixture();
        for case in fx["cases"].as_array().unwrap() {
            let Some(chunks) = case.get("chunks") else {
                continue;
            };
            let name = case["name"].as_str().unwrap();
            let mut buffer = Vec::new();
            let expected = case["responses_per_chunk"].as_array().unwrap();
            for (chunk, want) in hex_list(chunks).iter().zip(expected) {
                buffer.extend_from_slice(chunk);
                assert_eq!(
                    handle_received(&mut buffer, PYATV_SERVER),
                    hex_list(want),
                    "{name}"
                );
            }
            assert!(buffer.is_empty(), "{name}");
        }
    }

    #[test]
    fn default_server_is_winpods() {
        let fx = fixture();
        let case = &fx["cases"][0];
        let mut buffer = unhex(&case["input"]);
        let replies = handle_received(&mut buffer, DEFAULT_SERVER);
        let expected = String::from_utf8(unhex(&case["responses"][0]))
            .unwrap()
            .replace(
                PYATV_SERVER,
                &format!("WinPods/{}", env!("CARGO_PKG_VERSION")),
            );
        assert_eq!(String::from_utf8(replies[0].clone()).unwrap(), expected);
    }

    #[test]
    fn garbage_is_dropped_and_channel_recovers() {
        let mut buffer = b"\x00\x01garbage\r\n\r\n".to_vec();
        assert!(handle_received(&mut buffer, DEFAULT_SERVER).is_empty());
        assert!(buffer.is_empty());
        buffer.extend_from_slice(b"POST /command RTSP/1.0\r\nCSeq: 9\r\n\r\n");
        let replies = handle_received(&mut buffer, DEFAULT_SERVER);
        assert_eq!(replies.len(), 1);
        assert!(String::from_utf8_lossy(&replies[0]).ends_with("CSeq: 9\r\n\r\n"));
    }

    #[tokio::test]
    async fn retry_gives_up_after_the_attempt_limit() {
        let mut calls = 0;
        let result: std::io::Result<()> = retry_refused(5, Duration::from_millis(1), || {
            calls += 1;
            async { Err(std::io::ErrorKind::ConnectionRefused.into()) }
        })
        .await;
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionRefused
        );
        assert_eq!(calls, 5);

        let mut calls = 0;
        let result = retry_refused(5, Duration::from_millis(1), || {
            calls += 1;
            let n = calls;
            async move {
                if n < 3 {
                    Err(std::io::ErrorKind::ConnectionRefused.into())
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 3);

        let mut calls = 0;
        let result: std::io::Result<()> = retry_refused(5, Duration::from_millis(1), || {
            calls += 1;
            async { Err(std::io::ErrorKind::TimedOut.into()) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1, "only refusals are retried");
    }

    #[tokio::test]
    async fn answers_encrypted_requests_over_tcp() {
        let fx = fixture();
        let request = unhex(&fx["cases"][1]["input"]); // two concatenated commands
        let keys = CipherKeys {
            output: [7; 32],
            input: [8; 32],
        };
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let std::net::SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        let receiver_keys = keys.swapped();
        let receiver = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut hap = HapSession::new(&receiver_keys);
            let frames = hap.encrypt(&request).unwrap();
            // Deliver in small pieces to exercise frame reassembly.
            for piece in frames.chunks(300) {
                sock.write_all(piece).await.unwrap();
                sock.flush().await.unwrap();
            }
            let mut plain = Vec::new();
            let mut raw = vec![0u8; 4096];
            while plain.windows(4).filter(|w| w == b"\r\n\r\n").count() < 2 {
                let n = sock.read(&mut raw).await.unwrap();
                assert!(n > 0, "channel closed early");
                hap.decrypt(&raw[..n], &mut plain).unwrap();
            }
            String::from_utf8(plain).unwrap()
        });
        let channel = EventChannel::connect(addr, keys).await.unwrap();
        let replies = receiver.await.unwrap();
        assert!(replies.starts_with("RTSP/1.0 200 OK\r\nServer: WinPods/"));
        assert!(replies.contains("CSeq: 1\r\n\r\nRTSP/1.0 200 OK"));
        assert!(replies.ends_with("CSeq: 2\r\n\r\n"));
        while channel.requests_handled() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(channel.peer(), addr);
        channel.close().await;
    }
}
