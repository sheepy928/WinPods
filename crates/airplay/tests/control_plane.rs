//! Control-plane integration tests against a mock AirPlay 2 receiver on 127.0.0.1.
//!
//! The mock implements the receiver side for real: SRP-6a server math with
//! num-bigint (verifier from the salt and "Pair-Setup:3939", random `b`,
//! `B = k·v + g^b`), verification of the client's M1 and an M4 with the server proof
//! (or an Error TLV to exercise retries), HAP-encrypted RTSP with swapped control
//! keys, an event listener that sends a ~2 KB `POST /command` and checks the reply,
//! and canned SETUP/RECORD/FLUSH/SET_PARAMETER/feedback/TEARDOWN answers (feedback
//! and TEARDOWN can be left unanswered to check the session's time limits). It records
//! every decrypted request; the tests compare the order with the plan appendix and
//! the bytes with `fixtures/rtsp_requests.json` after normalising random fields.
//!
//! PTP is not exercised here (`ptp::PtpService` belongs to another workstream);
//! sessions use `TimingSetup::Ntp`. The PTP body logic is unit-tested in `bodies`
//! and `session`.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use num_bigint::BigUint;
use parking_lot::Mutex;
use plist::{Dictionary, Value};
use serde_json::Value as Json;
use sha2::{Digest, Sha512};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use winpods_airplay::hap::{HapSession, SharedSecret};
use winpods_airplay::rtsp::{IncomingRequest, parse_request};
use winpods_airplay::session::{
    Session, SessionConfig, SessionError, TEARDOWN_TIMEOUT, TimingSetup,
};
use winpods_airplay::tlv8::{self, tag};
use winpods_airplay::{Codec, ReceiverInfo, TimingProtocol, srp};

const SERVER: &str = "AirTunes/980.77.2";
/// `ClockID` the mock reports, as the negative int64 a HomePod may send.
const SIGNED_CLOCK_ID: i64 = -960_748_862_324_285_970;
const DATA_PORT: u64 = 50_010;
const CONTROL_PORT: u64 = 50_011;
const OUR_CONTROL_PORT: u16 = 6001;
const OUR_TIMING_PORT: u16 = 6002;

// ------------------------------------------------------------------- fixtures
fn fixture(name: &str) -> Json {
    let text = match name {
        "rtsp" => include_str!("fixtures/rtsp_requests.json"),
        "events" => include_str!("fixtures/event_channel.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(text).expect("fixture")
}

fn unhex(value: &Json) -> Vec<u8> {
    hex::decode(value.as_str().expect("hex string")).expect("valid hex")
}

fn split_head(bytes: &[u8]) -> (String, Vec<u8>) {
    let end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    (
        String::from_utf8(bytes[..end].to_vec()).expect("utf-8 head"),
        bytes[end + 4..].to_vec(),
    )
}

fn decode_plist(bytes: &[u8]) -> Value {
    Value::from_reader(std::io::Cursor::new(bytes)).expect("plist")
}

fn encode_plist(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    value.to_writer_binary(&mut out).expect("bplist");
    out
}

// ------------------------------------------------------------ SRP server side
fn sha512(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

fn pad(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    let mut out = vec![0u8; 384 - bytes.len()];
    out.extend_from_slice(&bytes);
    out
}

/// Receiver half of SRP-6a (srptools `SRPServerSession`).
struct SrpServer {
    n: BigUint,
    salt: Vec<u8>,
    verifier: BigUint,
    b: BigUint,
    b_pub: BigUint,
}

impl SrpServer {
    fn new() -> SrpServer {
        let n = BigUint::from_bytes_be(&srp::PRIME_3072);
        let g = BigUint::from(5u32);
        let mut salt = vec![0u8; 16];
        getrandom::fill(&mut salt).expect("rng");
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).expect("rng");
        let b = BigUint::from_bytes_be(&b);
        let x = BigUint::from_bytes_be(&sha512(&[&salt, &sha512(&[b"Pair-Setup:3939"])]));
        let verifier = g.modpow(&x, &n);
        let k = BigUint::from_bytes_be(&sha512(&[&n.to_bytes_be(), &pad(&g)]));
        let b_pub = (&k * &verifier + g.modpow(&b, &n)) % &n;
        SrpServer {
            n,
            salt,
            verifier,
            b,
            b_pub,
        }
    }

    /// Check the client's proof; return (verified, M2, K).
    fn finish(&self, a_pub: &[u8], client_proof: &[u8]) -> (bool, Vec<u8>, [u8; 64]) {
        let a = BigUint::from_bytes_be(a_pub);
        let u = BigUint::from_bytes_be(&sha512(&[&pad(&a), &pad(&self.b_pub)]));
        let s = (&a * self.verifier.modpow(&u, &self.n)).modpow(&self.b, &self.n);
        let key: [u8; 64] = sha512(&[&s.to_bytes_be()]).try_into().expect("64 bytes");
        let h_n = BigUint::from_bytes_be(&sha512(&[&self.n.to_bytes_be()]));
        let h_g = BigUint::from_bytes_be(&sha512(&[&[5u8]]));
        let h_i = BigUint::from_bytes_be(&sha512(&[b"Pair-Setup"]));
        let a_min = a.to_bytes_be();
        let expected = sha512(&[
            &(h_n ^ h_g).to_bytes_be(),
            &h_i.to_bytes_be(),
            &self.salt,
            &a_min,
            &self.b_pub.to_bytes_be(),
            &key,
        ]);
        let m2 = sha512(&[&a_min, client_proof, &key]);
        (expected == client_proof, m2, key)
    }
}

// ---------------------------------------------------------------- mock receiver
#[derive(Clone, Default)]
struct MockOptions {
    /// The first N pair-setup attempts get an Authentication error in M4.
    failed_pairings: u32,
    /// Status for POST /feedback (200 when 0).
    feedback_status: u16,
    /// Put `skipRecord = true` in the base SETUP response.
    skip_record: bool,
    /// Answer the stream SETUP with this status instead of 200.
    stream_setup_status: u16,
    /// Close the connection after answering the request with this name.
    close_after: Option<&'static str>,
    /// Answer only the first N POST /feedback requests; later ones get no reply
    /// (a receiver that stopped responding). `None` = answer all.
    answered_feedback: Option<usize>,
    /// Never answer TEARDOWN.
    silent_teardown: bool,
}

#[derive(Default)]
struct MockLog {
    /// (name, decrypted request bytes) in arrival order.
    requests: Vec<(String, Vec<u8>)>,
    /// Result of checking each client M1 proof.
    proofs_verified: Vec<bool>,
    /// The session key of the successful pairing.
    secret: Option<[u8; 64]>,
}

struct MockReceiver {
    port: u16,
    log: Arc<Mutex<MockLog>>,
    /// Decrypted reply to the event channel command.
    event_reply: Option<oneshot::Receiver<Vec<u8>>>,
    /// Fires when the event channel was closed by the client.
    event_closed: Option<oneshot::Receiver<()>>,
    task: tokio::task::JoinHandle<()>,
}

/// One side of a (possibly HAP-encrypted) connection.
struct Conn {
    stream: TcpStream,
    hap: Option<HapSession>,
    plain: Vec<u8>,
}

impl Conn {
    async fn next_request(&mut self) -> Option<(IncomingRequest, Vec<u8>)> {
        let mut raw = vec![0u8; 8192];
        loop {
            if let Some((request, used)) = parse_request(&self.plain).expect("request parses") {
                let bytes: Vec<u8> = self.plain.drain(..used).collect();
                return Some((request, bytes));
            }
            let n = self.stream.read(&mut raw).await.ok()?;
            if n == 0 {
                return None;
            }
            match &mut self.hap {
                Some(hap) => hap
                    .decrypt(&raw[..n], &mut self.plain)
                    .expect("HAP decrypt"),
                None => self.plain.extend_from_slice(&raw[..n]),
            }
        }
    }

    async fn send(&mut self, bytes: &[u8]) {
        let out = match &mut self.hap {
            Some(hap) => hap.encrypt(bytes).expect("HAP encrypt"),
            None => bytes.to_vec(),
        };
        self.stream.write_all(&out).await.expect("mock write");
    }
}

fn response(
    status: u16,
    cseq: Option<&str>,
    extra: &[(&str, &str)],
    content_type: Option<&str>,
    body: &[u8],
    http: bool,
) -> Vec<u8> {
    let proto = if http { "HTTP/1.1" } else { "RTSP/1.0" };
    let reason = if status == 200 { "OK" } else { "Error" };
    let mut head = format!(
        "{proto} {status} {reason}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Server: {SERVER}\r\n"));
    if let Some(cseq) = cseq {
        head.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

fn plist_response(cseq: Option<&str>, value: &Value) -> Vec<u8> {
    response(
        200,
        cseq,
        &[],
        Some("application/x-apple-binary-plist"),
        &encode_plist(value),
        false,
    )
}

fn dict(items: Vec<(&str, Value)>) -> Value {
    Value::Dictionary(
        items
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<Dictionary>(),
    )
}

fn int(v: i64) -> Value {
    Value::Integer(v.into())
}

/// Name of a request in `rtsp_requests.json` terms.
fn request_name(request: &IncomingRequest) -> String {
    let content_type = request.headers.get("Content-Type").unwrap_or_default();
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/info") => "info".into(),
        ("POST", "/pair-pin-start") => "pair_pin_start".into(),
        ("POST", "/pair-setup") => {
            let tlv = tlv8::decode(&request.body).expect("pair-setup TLV");
            format!("pair_setup_m{}", tlv.get_u8(tag::SEQ_NO).unwrap_or(0))
        }
        ("SETUP", _) => {
            let body = decode_plist(&request.body);
            if body
                .as_dictionary()
                .is_some_and(|d| d.contains_key("streams"))
            {
                "setup_stream".into()
            } else {
                "setup_base".into()
            }
        }
        ("SET_PARAMETER", _) if content_type == "application/x-dmap-tagged" => {
            "set_metadata".into()
        }
        ("SET_PARAMETER", _) => "set_parameter_volume".into(),
        ("RECORD", _) => "record".into(),
        ("POST", "/feedback") => "feedback".into(),
        ("FLUSH", _) => "flush".into(),
        ("TEARDOWN", _) => "teardown".into(),
        (method, path) => format!("unexpected {method} {path}"),
    }
}

impl MockReceiver {
    async fn start(options: MockOptions) -> MockReceiver {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let events = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind events");
        let port = listener.local_addr().expect("addr").port();
        let event_port = events.local_addr().expect("addr").port();
        let log = Arc::new(Mutex::new(MockLog::default()));
        let (reply_tx, reply_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = oneshot::channel();
        let task = tokio::spawn(serve(
            listener,
            events,
            event_port,
            options,
            log.clone(),
            reply_tx,
            closed_tx,
        ));
        MockReceiver {
            port,
            log,
            event_reply: Some(reply_rx),
            event_closed: Some(closed_rx),
            task,
        }
    }

    fn info(&self) -> ReceiverInfo {
        ReceiverInfo {
            name: "Living Room".into(),
            model: "AudioAccessory5,1".into(),
            address: Ipv4Addr::LOCALHOST,
            raop_port: self.port,
            features: 0x3C35_4BD0_4A7F_CA00,
            metadata_types: vec![0, 1, 2],
            encryption_types: vec![0, 3, 5],
            receiver_ids: vec!["0A0A0A0A-0000-4000-8000-000000000001".into()],
            requires_password: false,
            raop_txt: Vec::new(),
            airplay_txt: Vec::new(),
        }
    }

    fn names(&self) -> Vec<String> {
        self.log
            .lock()
            .requests
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    fn request(&self, name: &str) -> Vec<u8> {
        self.log
            .lock()
            .requests
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.clone())
            .unwrap_or_else(|| panic!("no {name} request"))
    }

    /// Wait until `count` requests named `name` have arrived.
    async fn wait_for(&self, name: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.names().iter().filter(|n| *n == name).count() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} {name} requests in time"));
    }

    /// Wait for the event channel exchange and return the decrypted reply.
    async fn event_reply(&mut self) -> Vec<u8> {
        let rx = self.event_reply.take().expect("event reply taken once");
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("event reply in time")
            .expect("event task alive")
    }

    async fn finish(self) {
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("mock finishes")
            .expect("mock task");
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    listener: TcpListener,
    events: TcpListener,
    event_port: u16,
    options: MockOptions,
    log: Arc<Mutex<MockLog>>,
    reply_tx: oneshot::Sender<Vec<u8>>,
    closed_tx: oneshot::Sender<()>,
) {
    let (stream, _) = listener.accept().await.expect("accept");
    let mut conn = Conn {
        stream,
        hap: None,
        plain: Vec::new(),
    };
    let mut events = Some(events);
    let mut srp_server: Option<SrpServer> = None;
    let mut pairing_attempts = 0;
    let mut event_task = None;
    let mut reply_tx = Some(reply_tx);
    let mut closed_tx = Some(closed_tx);
    let mut feedback_seen = 0usize;

    while let Some((request, bytes)) = conn.next_request().await {
        let name = request_name(&request);
        log.lock().requests.push((name.clone(), bytes));
        let cseq = request.headers.get("CSeq").map(str::to_string);
        let cseq = cseq.as_deref();
        match name.as_str() {
            "info" => {
                let body = dict(vec![
                    ("deviceID", Value::String("02:AA:BB:CC:DD:EE".into())),
                    ("model", Value::String("AudioAccessory5,1".into())),
                    ("name", Value::String("Living Room".into())),
                ]);
                conn.send(&plist_response(cseq, &body)).await;
            }
            "pair_pin_start" => conn.send(&response(200, None, &[], None, &[], true)).await,
            "pair_setup_m1" => {
                let server = SrpServer::new();
                let body = tlv8::encode(&[
                    (tag::SEQ_NO, &[2]),
                    (tag::SALT, &server.salt),
                    (tag::PUBLIC_KEY, &server.b_pub.to_bytes_be()),
                ]);
                srp_server = Some(server);
                conn.send(&response(
                    200,
                    None,
                    &[],
                    Some("application/octet-stream"),
                    &body,
                    true,
                ))
                .await;
            }
            "pair_setup_m3" => {
                pairing_attempts += 1;
                let tlv = tlv8::decode(&request.body).expect("M3 TLV");
                let server = srp_server.take().expect("M1 before M3");
                let (verified, m2, key) = server.finish(
                    tlv.get(tag::PUBLIC_KEY).expect("A"),
                    tlv.get(tag::PROOF).expect("M1 proof"),
                );
                log.lock().proofs_verified.push(verified);
                let fail = pairing_attempts <= options.failed_pairings || !verified;
                let body = if fail {
                    tlv8::encode(&[(tag::SEQ_NO, &[4]), (tag::ERROR, &[2])])
                } else {
                    tlv8::encode(&[(tag::SEQ_NO, &[4]), (tag::PROOF, &m2)])
                };
                conn.send(&response(
                    200,
                    None,
                    &[],
                    Some("application/octet-stream"),
                    &body,
                    true,
                ))
                .await;
                if !fail {
                    let secret = SharedSecret::new(key);
                    log.lock().secret = Some(key);
                    conn.hap = Some(HapSession::new(&secret.control_keys().swapped()));
                }
            }
            "setup_base" => {
                let key = log.lock().secret.expect("paired before SETUP");
                let keys = SharedSecret::new(key).event_keys().swapped();
                let reply = reply_tx.take().expect("one base SETUP");
                let closed = closed_tx.take().expect("one base SETUP");
                let listener = events.take().expect("one base SETUP");
                event_task = Some(tokio::spawn(event_receiver(listener, keys, reply, closed)));
                let mut items = vec![
                    ("eventPort", int(i64::from(event_port))),
                    ("timingPort", int(0)),
                    (
                        "timingPeerInfo",
                        dict(vec![
                            (
                                "Addresses",
                                Value::Array(vec![Value::String("127.0.0.1".into())]),
                            ),
                            ("ClockID", int(SIGNED_CLOCK_ID)),
                            (
                                "ID",
                                Value::String("0D0D0D0D-0000-4000-8000-000000000004".into()),
                            ),
                            ("SupportsClockPortMatchingOverride", Value::Boolean(true)),
                        ]),
                    ),
                ];
                if options.skip_record {
                    items.push(("skipRecord", Value::Boolean(true)));
                }
                conn.send(&plist_response(cseq, &dict(items))).await;
            }
            "setup_stream" => {
                if options.stream_setup_status != 0 {
                    conn.send(&response(
                        options.stream_setup_status,
                        cseq,
                        &[],
                        None,
                        &[],
                        false,
                    ))
                    .await;
                } else {
                    let stream = dict(vec![
                        ("audioBufferSize", int(8_388_608)),
                        ("controlPort", int(CONTROL_PORT as i64)),
                        ("dataPort", int(DATA_PORT as i64)),
                        ("streamID", int(1)),
                        ("type", int(96)),
                        ("arrivalToRenderLatencyMs", int(69)),
                    ]);
                    let body = dict(vec![("streams", Value::Array(vec![stream]))]);
                    conn.send(&plist_response(cseq, &body)).await;
                }
            }
            "record" => {
                conn.send(&response(
                    200,
                    cseq,
                    &[("Audio-Latency", "0")],
                    None,
                    &[],
                    false,
                ))
                .await
            }
            "feedback" => {
                feedback_seen += 1;
                if options.answered_feedback.is_some_and(|n| feedback_seen > n) {
                    // No reply; keep serving the requests that follow.
                } else if options.feedback_status != 0 && options.feedback_status != 200 {
                    conn.send(&response(
                        options.feedback_status,
                        cseq,
                        &[],
                        None,
                        &[],
                        false,
                    ))
                    .await;
                } else {
                    let stream = dict(vec![
                        ("sr", Value::Real(44100.0)),
                        ("streamConnectionID", int(305_419_896)),
                        ("type", int(96)),
                    ]);
                    let body = dict(vec![("streams", Value::Array(vec![stream]))]);
                    conn.send(&plist_response(cseq, &body)).await;
                }
            }
            "teardown" if options.silent_teardown => {}
            "set_metadata" | "flush" | "set_parameter_volume" | "teardown" => {
                conn.send(&response(200, cseq, &[], None, &[], false)).await
            }
            other => panic!("mock got {other}"),
        }
        if options.close_after == Some(name.as_str()) {
            break;
        }
    }
    drop(conn);
    if let Some(task) = event_task {
        task.await.expect("event receiver");
    }
}

/// Receiver side of the event channel: send one ~2 KB command, report the reply,
/// then wait for the client to close the channel.
async fn event_receiver(
    listener: TcpListener,
    keys: winpods_airplay::hap::CipherKeys,
    reply_tx: oneshot::Sender<Vec<u8>>,
    closed_tx: oneshot::Sender<()>,
) {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("event channel connects")
        .expect("accept");
    let mut conn = Conn {
        stream,
        hap: Some(HapSession::new(&keys)),
        plain: Vec::new(),
    };
    let command = unhex(&fixture("events")["cases"][0]["input"]);
    assert!(command.len() > 2000);
    conn.send(&command).await;
    let mut raw = vec![0u8; 4096];
    while !conn.plain.ends_with(b"\r\n\r\n") {
        let n = conn.stream.read(&mut raw).await.unwrap_or(0);
        if n == 0 {
            // Closed before replying (a test that tears down early); the full
            // session test waits for the reply and fails on the dropped sender.
            return;
        }
        conn.hap
            .as_mut()
            .expect("hap")
            .decrypt(&raw[..n], &mut conn.plain)
            .expect("event decrypt");
    }
    let _ = reply_tx.send(conn.plain.clone());
    // The client closes the channel during teardown.
    while conn
        .stream
        .read(&mut raw)
        .await
        .map(|n| n > 0)
        .unwrap_or(false)
    {}
    let _ = closed_tx.send(());
}

fn config(volume_pct: Option<f32>) -> SessionConfig {
    SessionConfig {
        sender_name: "WinPods".into(),
        device_id: "02:11:22:33:44:55".into(),
        latency_ms: 100,
        codec: Codec::Alac,
        volume_pct,
        title: "Windows audio".into(),
    }
}

// ------------------------------------------------------------- normalisation
/// Header text of a fixture request with the fixture's random values replaced by
/// this session's, and plist Content-Length values masked.
fn normalise_head(head: &str, dacp: &str, active_remote: &str, uri: &str, seq: u16) -> String {
    head.replace("0123456789ABCDEF", dacp)
        .replace(
            "Active-Remote: 1234567890",
            &format!("Active-Remote: {active_remote}"),
        )
        .replace("rtsp://192.168.1.57/305419896", uri)
        .replace("seq=12345;", &format!("seq={seq};"))
}

fn mask_plist_length(head: &str) -> String {
    if !head.contains("application/x-apple-binary-plist") {
        return head.to_string();
    }
    head.lines()
        .map(|l| {
            if l.starts_with("Content-Length:") {
                "Content-Length: <plist>"
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn header_value(head: &str, name: &str) -> String {
    head.lines()
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_string()
}

// ------------------------------------------------------------------- tests
#[tokio::test]
async fn full_session_matches_python_transcript() {
    let mut mock = MockReceiver::start(MockOptions::default()).await;
    let mut session = Session::connect(&mock.info(), config(Some(40.0)))
        .await
        .expect("connect");
    assert_eq!(session.pairing_retries(), 0);
    assert_eq!(session.local_ip(), Ipv4Addr::LOCALHOST);
    assert!(session.end_reason().is_none());

    let params = session
        .setup(
            TimingSetup::Ntp {
                timing_port: OUR_TIMING_PORT,
            },
            OUR_CONTROL_PORT,
        )
        .await
        .expect("setup");
    let secret = SharedSecret::new(mock.log.lock().secret.expect("paired"));
    assert_eq!(params.receiver, Ipv4Addr::LOCALHOST);
    assert_eq!(params.local_ip, Ipv4Addr::LOCALHOST);
    assert_eq!(u64::from(params.data_port), DATA_PORT);
    assert_eq!(u64::from(params.control_port), CONTROL_PORT);
    assert_eq!(params.shk, secret.shk());
    assert_eq!(params.codec, Codec::Alac);
    assert_eq!(params.timing, TimingProtocol::Ntp);
    assert_eq!((params.latency_ms, params.latency_frames), (100, 4410));
    assert_eq!(params.render_ms, Some(69));
    assert_eq!(params.clock_id, Some(SIGNED_CLOCK_ID as u64));
    assert_eq!(params.first_ts, 4410);

    session
        .start_playback(&params)
        .await
        .expect("start playback");
    let feedback = session.last_feedback().expect("first feedback decoded");
    assert!(
        feedback.contains("'streamConnectionID': 305419896"),
        "{feedback}"
    );

    let event_reply = mock.event_reply().await;
    let expected_reply = String::from_utf8(unhex(&fixture("events")["cases"][0]["responses"][0]))
        .expect("utf-8")
        .replace(
            "pyatv-www/0.18.0",
            &format!("WinPods/{}", env!("CARGO_PKG_VERSION")),
        );
    assert_eq!(
        String::from_utf8(event_reply).expect("utf-8"),
        expected_reply
    );

    session.teardown().await;
    let closed = mock.event_closed.take().expect("closed signal");
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("event channel closed by teardown")
        .expect("event task alive");

    // Order: plan appendix steps 2-16 (and rtsp_requests.json).
    let fx = fixture("rtsp");
    let expected_names: Vec<String> = fx["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .map(|r| r["name"].as_str().expect("name").to_string())
        .collect();
    assert_eq!(mock.names(), expected_names);
    assert!(mock.log.lock().proofs_verified.iter().all(|v| *v));

    // Bytes, after normalising the random fields.
    let (base_head, _) = split_head(&mock.request("setup_base"));
    let dacp = header_value(&base_head, "DACP-ID");
    let active_remote = header_value(&base_head, "Active-Remote");
    assert_eq!(header_value(&base_head, "Client-Instance"), dacp);
    assert!(u64::from_str_radix(&dacp, 16).is_ok(), "{dacp}");
    let uri = format!("rtsp://127.0.0.1/{}", params.ssrc);
    assert!(base_head.starts_with(&format!("SETUP {uri} RTSP/1.0\r\n")));

    for entry in fx["requests"].as_array().expect("requests") {
        let name = entry["name"].as_str().expect("name");
        let (expected_head, expected_body) = split_head(&unhex(&entry["bytes"]));
        let (head, body) = split_head(&mock.request(name));
        let expected_head = normalise_head(
            &expected_head,
            &dacp,
            &active_remote,
            &uri,
            params.first_seq,
        );
        assert_eq!(
            mask_plist_length(&head),
            mask_plist_length(&expected_head),
            "{name} head"
        );
        match name {
            "setup_base" => {
                // The transcript used PTP; this session uses NTP, so its body must be
                // the transcript's with the timing keys swapped (setup_bodies "ntp").
                let mut want = decode_plist(&expected_body)
                    .into_dictionary()
                    .expect("dict");
                let got = decode_plist(&body).into_dictionary().expect("dict");
                let uuid = got["sessionUUID"].as_string().expect("uuid").to_string();
                assert_eq!(uuid, uuid.to_uppercase());
                assert_eq!(
                    uuid::Uuid::parse_str(&uuid)
                        .expect("uuid")
                        .get_version_num(),
                    4
                );
                want.insert("sessionUUID".into(), Value::String(uuid));
                want.insert("timingProtocol".into(), Value::String("NTP".into()));
                want.remove("timingPeerInfo");
                want.insert("timingPort".into(), int(i64::from(OUR_TIMING_PORT)));
                assert_eq!(got, want, "{name} body");
            }
            "setup_stream" => {
                let mut want = decode_plist(&expected_body);
                let stream = want.as_dictionary_mut().expect("dict")["streams"]
                    .as_array_mut()
                    .expect("streams")[0]
                    .as_dictionary_mut()
                    .expect("stream");
                stream.insert("shk".into(), Value::Data(secret.shk().to_vec()));
                stream.insert("streamConnectionID".into(), int(i64::from(params.ssrc)));
                assert_eq!(decode_plist(&body), want, "{name} body");
            }
            "pair_setup_m3" => {
                let tlv = tlv8::decode(&body).expect("M3");
                let tags: Vec<(u8, usize)> = tlv.iter().map(|(t, v)| (t, v.len())).collect();
                assert_eq!(
                    tags,
                    vec![(tag::SEQ_NO, 1), (tag::PUBLIC_KEY, 384), (tag::PROOF, 64)]
                );
                assert_eq!(tlv.get_u8(tag::SEQ_NO), Some(3));
            }
            _ => assert_eq!(body, expected_body, "{name} body"),
        }
    }
    mock.finish().await;
}

#[tokio::test]
async fn volume_changes_go_through_the_session_task() {
    let mock = MockReceiver::start(MockOptions::default()).await;
    let mut session = Session::connect(&mock.info(), config(None))
        .await
        .expect("connect");
    let params = session
        .setup(TimingSetup::Ntp { timing_port: 1 }, 2)
        .await
        .expect("setup");
    session.start_playback(&params).await.expect("start");
    session.set_volume(66.7).await.expect("volume");
    session.set_volume(0.0).await.expect("mute");
    session.teardown().await;
    let names = mock.names();
    assert_eq!(
        names[names.len() - 3..],
        ["set_parameter_volume", "set_parameter_volume", "teardown"]
    );
    let bodies: Vec<String> = mock
        .log
        .lock()
        .requests
        .iter()
        .filter(|(n, _)| n == "set_parameter_volume")
        .map(|(_, b)| String::from_utf8(split_head(b).1).expect("utf-8"))
        .collect();
    assert_eq!(bodies, ["volume: -9.989999999999998", "volume: -144.0"]);
    // No initial volume configured: none sent before the explicit changes.
    assert!(!names[..names.len() - 3].contains(&"set_parameter_volume".to_string()));
    mock.finish().await;
}

#[tokio::test]
async fn skip_record_sends_flush_before_the_first_feedback() {
    let mock = MockReceiver::start(MockOptions {
        skip_record: true,
        ..Default::default()
    })
    .await;
    let mut session = Session::connect(&mock.info(), config(Some(40.0)))
        .await
        .expect("connect");
    let params = session
        .setup(TimingSetup::Ntp { timing_port: 1 }, 2)
        .await
        .expect("setup");
    session.start_playback(&params).await.expect("start");
    session.teardown().await;
    let names = mock.names();
    assert_eq!(
        names[4..],
        [
            "setup_base",
            "setup_stream",
            "set_metadata",
            "flush",
            "feedback",
            "set_parameter_volume",
            "teardown"
        ]
    );
    mock.finish().await;
}

#[tokio::test]
async fn rejected_pairing_is_retried_and_counted() {
    let mock = MockReceiver::start(MockOptions {
        failed_pairings: 2,
        ..Default::default()
    })
    .await;
    let session = Session::connect(&mock.info(), config(None))
        .await
        .expect("connect");
    assert_eq!(session.pairing_retries(), 2);
    let names = mock.names();
    assert_eq!(names.iter().filter(|n| *n == "pair_setup_m3").count(), 3);
    assert_eq!(names.iter().filter(|n| *n == "pair_pin_start").count(), 3);
    assert_eq!(names[0], "info");
    // Every client proof was valid; the failures were the receiver's choice.
    assert!(mock.log.lock().proofs_verified.iter().all(|v| *v));
    // Dropping the session without teardown closes the connection.
    drop(session);
    mock.finish().await;
}

#[tokio::test]
async fn pairing_gives_up_after_three_retries() {
    let mock = MockReceiver::start(MockOptions {
        failed_pairings: u32::MAX,
        ..Default::default()
    })
    .await;
    let err = Session::connect(&mock.info(), config(None))
        .await
        .unwrap_err();
    assert!(matches!(err, SessionError::Pairing(_)), "{err}");
    assert!(err.to_string().contains("Authentication"), "{err}");
    assert_eq!(
        mock.names()
            .iter()
            .filter(|n| *n == "pair_setup_m3")
            .count(),
        4,
        "one attempt plus three retries"
    );
    mock.finish().await;
}

#[tokio::test]
async fn closed_connection_sets_end_reason() {
    let mock = MockReceiver::start(MockOptions {
        close_after: Some("set_parameter_volume"),
        ..Default::default()
    })
    .await;
    let mut session = Session::connect(&mock.info(), config(Some(50.0)))
        .await
        .expect("connect");
    let params = session
        .setup(TimingSetup::Ntp { timing_port: 1 }, 2)
        .await
        .expect("setup");
    session.start_playback(&params).await.expect("start");
    let reason = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(reason) = session.end_reason() {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("end reason in time");
    assert_eq!(reason, "receiver closed the connection");
    assert!(session.set_volume(10.0).await.is_err());
    session.teardown().await;
    assert!(!mock.names().contains(&"teardown".to_string()));
    mock.finish().await;
}

/// pyatv only logs failed feedback requests. A receiver that answers POST /feedback
/// with an error status is still there, so the session must keep going (it used to
/// end after the third error status).
#[tokio::test]
async fn rejected_feedback_does_not_end_the_session() {
    let mock = MockReceiver::start(MockOptions {
        feedback_status: 500,
        ..Default::default()
    })
    .await;
    let mut session = Session::connect(&mock.info(), config(None))
        .await
        .expect("connect");
    let params = session
        .setup(TimingSetup::Ntp { timing_port: 1 }, 2)
        .await
        .expect("setup");
    session
        .start_playback(&params)
        .await
        .expect("feedback errors do not fail playback");
    // More rejected feedbacks than FEEDBACK_FAILURE_LIMIT (one every 2 s).
    tokio::time::timeout(Duration::from_secs(10), async {
        while mock.names().iter().filter(|n| *n == "feedback").count() < 4 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("four feedback requests");
    // Let the fourth response be handled before looking.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(session.end_reason(), None);
    assert!(session.last_feedback().is_none());
    // The session still works.
    session
        .set_volume(30.0)
        .await
        .expect("volume after rejected feedback");
    session.teardown().await;
    assert!(mock.names().contains(&"teardown".to_string()));
    mock.finish().await;
}

/// Bound for a request the mock answers at once, even with a feedback outstanding
/// (an unanswered feedback would otherwise hold it for up to 10 s).
const PROMPT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn unanswered_feedback_does_not_hold_up_volume_or_teardown() {
    let mock = MockReceiver::start(MockOptions {
        answered_feedback: Some(1),
        ..Default::default()
    })
    .await;
    let mut session = Session::connect(&mock.info(), config(Some(40.0)))
        .await
        .expect("connect");
    let params = session
        .setup(TimingSetup::Ntp { timing_port: 1 }, 2)
        .await
        .expect("setup");
    session.start_playback(&params).await.expect("start");
    assert!(session.last_feedback().is_some(), "first feedback answered");
    // The periodic feedback 2 s later is never answered.
    mock.wait_for("feedback", 2).await;

    let started = Instant::now();
    session.set_volume(50.0).await.expect("volume");
    let volume_time = started.elapsed();
    assert!(volume_time < PROMPT, "set_volume took {volume_time:?}");
    assert!(session.end_reason().is_none());

    let started = Instant::now();
    session.teardown().await;
    let teardown_time = started.elapsed();
    assert!(teardown_time < PROMPT, "teardown took {teardown_time:?}");
    let names = mock.names();
    assert_eq!(
        names[names.len() - 3..],
        ["feedback", "set_parameter_volume", "teardown"]
    );
    mock.finish().await;
}

#[tokio::test]
async fn unanswered_first_feedback_does_not_delay_flush() {
    let mock = MockReceiver::start(MockOptions {
        answered_feedback: Some(0),
        ..Default::default()
    })
    .await;
    let mut session = Session::connect(&mock.info(), config(Some(40.0)))
        .await
        .expect("connect");
    let params = session
        .setup(TimingSetup::Ntp { timing_port: 1 }, 2)
        .await
        .expect("setup");
    let started = Instant::now();
    session.start_playback(&params).await.expect("start");
    let start_time = started.elapsed();
    assert!(start_time < PROMPT, "start_playback took {start_time:?}");
    assert!(session.last_feedback().is_none());
    assert_eq!(
        mock.names()[4..],
        [
            "setup_base",
            "setup_stream",
            "set_metadata",
            "record",
            "feedback",
            "flush",
            "set_parameter_volume"
        ]
    );
    let started = Instant::now();
    session.teardown().await;
    let teardown_time = started.elapsed();
    assert!(teardown_time < PROMPT, "teardown took {teardown_time:?}");
    mock.finish().await;
}

#[tokio::test]
async fn teardown_is_bounded_when_the_receiver_stops_answering() {
    let mut mock = MockReceiver::start(MockOptions {
        answered_feedback: Some(0),
        silent_teardown: true,
        ..Default::default()
    })
    .await;
    let mut session = Session::connect(&mock.info(), config(None))
        .await
        .expect("connect");
    let params = session
        .setup(TimingSetup::Ntp { timing_port: 1 }, 2)
        .await
        .expect("setup");
    session.start_playback(&params).await.expect("start");

    let started = Instant::now();
    session.teardown().await;
    let elapsed = started.elapsed();
    // It waited for the TEARDOWN response, but no longer than the limit.
    assert!(
        elapsed >= TEARDOWN_TIMEOUT - Duration::from_millis(50),
        "{elapsed:?}"
    );
    assert!(
        elapsed < TEARDOWN_TIMEOUT + Duration::from_millis(500),
        "{elapsed:?}"
    );
    assert_eq!(mock.names().last().map(String::as_str), Some("teardown"));
    let closed = mock.event_closed.take().expect("closed signal");
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("event channel closed by teardown")
        .expect("event task alive");
    mock.finish().await;
}

#[tokio::test]
async fn rejected_stream_setup_is_an_error() {
    let mock = MockReceiver::start(MockOptions {
        stream_setup_status: 500,
        ..Default::default()
    })
    .await;
    let mut session = Session::connect(&mock.info(), config(None))
        .await
        .expect("connect");
    let err = session
        .setup(TimingSetup::Ntp { timing_port: 1 }, 2)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SessionError::Rejected { method, code: 500, .. } if method == "SETUP"),
        "{err}"
    );
    session.teardown().await;
    assert_eq!(mock.names().last().map(String::as_str), Some("teardown"));
    mock.finish().await;
}

#[tokio::test]
async fn unreachable_receiver_is_a_network_error() {
    // Bind and drop to get a port nobody listens on.
    let port = {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let mut info = ReceiverInfo {
        name: "Nobody".into(),
        model: String::new(),
        address: Ipv4Addr::LOCALHOST,
        raop_port: port,
        features: 1 << 48,
        metadata_types: Vec::new(),
        encryption_types: Vec::new(),
        receiver_ids: Vec::new(),
        requires_password: false,
        raop_txt: Vec::new(),
        airplay_txt: Vec::new(),
    };
    let err = Session::connect(&info, config(None)).await.unwrap_err();
    assert!(matches!(err, SessionError::Io(_)), "{err}");
    info.features = 0;
    assert!(matches!(
        Session::connect(&info, config(None)).await.unwrap_err(),
        SessionError::Pairing(_)
    ));
}
