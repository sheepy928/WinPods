//! Integration tests for the unicast mDNS lookup against a mock responder on 127.0.0.1
//! that answers with the recorded (sanitized) HomePod packets from
//! `tests/fixtures/mdns.json`. Nothing here talks to a real device, except
//! `real_lookup`, which is `#[ignore]`d and needs `WINPODS_TEST_RECEIVER`.

use std::collections::HashMap;
use std::fmt;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use winpods_airplay::ReceiverInfo;
use winpods_airplay::mdns::{self, SERVICES};

const FIXTURE: &str = include_str!("fixtures/mdns.json");

#[derive(Deserialize)]
struct Fixture {
    services: Vec<String>,
    queries: Vec<String>,
    responses: Vec<String>,
    expected: Expected,
}

#[derive(Deserialize)]
struct Expected {
    name: String,
    address: Ipv4Addr,
    model: String,
    raop_port: u16,
    txt: Txt,
    features: HashMap<String, u64>,
    receiver_ids: Vec<String>,
}

#[derive(Deserialize)]
struct Txt {
    #[serde(rename = "_raop._tcp")]
    raop: OrderedTxt,
    #[serde(rename = "_airplay._tcp")]
    airplay: OrderedTxt,
}

/// A JSON object of strings with its key order kept (the fixture lists TXT entries in
/// record order; `serde_json::Map` would sort them).
struct OrderedTxt(Vec<(String, String)>);

impl OrderedTxt {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

impl<'de> Deserialize<'de> for OrderedTxt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor;
        impl<'de> Visitor<'de> for OrderedVisitor {
            type Value = OrderedTxt;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of strings")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<OrderedTxt, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, String>()? {
                    entries.push(entry);
                }
                Ok(OrderedTxt(entries))
            }
        }
        deserializer.deserialize_map(OrderedVisitor)
    }
}

fn fixture() -> Fixture {
    serde_json::from_str(FIXTURE).expect("mdns.json parses")
}

fn decode(list: &[String]) -> Vec<Vec<u8>> {
    list.iter()
        .map(|item| hex::decode(item).expect("valid hex"))
        .collect()
}

fn int_list(value: Option<&str>) -> Vec<u8> {
    value
        .map(|v| v.split(',').map(|n| n.parse().expect("integer")).collect())
        .unwrap_or_default()
}

/// The `ReceiverInfo` the fixture's `expected` section describes.
fn expected_info(fixture: &Fixture) -> ReceiverInfo {
    let expected = &fixture.expected;
    let raop = &expected.txt.raop;
    ReceiverInfo {
        name: expected.name.clone(),
        model: expected.model.clone(),
        address: expected.address,
        raop_port: expected.raop_port,
        features: expected.features["_raop._tcp"],
        metadata_types: int_list(raop.get("md")),
        encryption_types: int_list(raop.get("et")),
        receiver_ids: expected.receiver_ids.clone(),
        // No `pw`, and sf/flags 0x1a0404 lacks the 0x80 password bit.
        requires_password: false,
        raop_txt: raop.0.clone(),
        airplay_txt: expected.txt.airplay.0.clone(),
    }
}

/// What the mock answers to the `index`-th datagram it receives.
type Policy = Box<dyn FnMut(usize, &[u8]) -> Vec<Vec<u8>> + Send>;

struct Mock {
    port: u16,
    received: Arc<Mutex<Vec<Vec<u8>>>>,
    task: JoinHandle<()>,
}

impl Mock {
    async fn start(mut policy: Policy) -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind mock");
        let port = socket.local_addr().expect("local addr").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            let mut index = 0;
            loop {
                let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                    continue;
                };
                let packet = buf[..len].to_vec();
                log.lock().expect("lock").push(packet.clone());
                for reply in policy(index, &packet) {
                    let _ = socket.send_to(&reply, peer).await;
                }
                index += 1;
            }
        });
        Self {
            port,
            received,
            task,
        }
    }

    fn received(&self) -> Vec<Vec<u8>> {
        self.received.lock().expect("lock").clone()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answer each fixture query with its recorded response, skipping the first `drop`
/// replies the mock would send.
fn answer_queries(fixture: &Fixture, mut drop: usize) -> Policy {
    let queries = decode(&fixture.queries);
    let responses = decode(&fixture.responses);
    Box::new(move |_: usize, packet: &[u8]| {
        let Some(i) = queries.iter().position(|q| q.as_slice() == packet) else {
            return Vec::new();
        };
        if drop > 0 {
            drop -= 1;
            return Vec::new();
        }
        vec![responses[i].clone()]
    })
}

async fn lookup(
    mock: &Mock,
    timeout: Duration,
) -> (std::io::Result<Option<ReceiverInfo>>, Duration) {
    let started = Instant::now();
    let result = mdns::lookup_at(Ipv4Addr::LOCALHOST, mock.port, timeout).await;
    (result, started.elapsed())
}

#[test]
fn fixture_services_and_queries() {
    let fixture = fixture();
    assert_eq!(fixture.services, SERVICES.map(str::to_string).to_vec());
    assert_eq!(
        mdns::service_queries(&SERVICES).expect("encode"),
        decode(&fixture.queries)
    );
}

#[tokio::test]
async fn lookup_reproduces_fixture() {
    let fixture = fixture();
    let mock = Mock::start(answer_queries(&fixture, 0)).await;
    let (result, elapsed) = lookup(&mock, Duration::from_secs(5)).await;
    let info = result.expect("lookup ok").expect("receiver found");
    assert_eq!(info, expected_info(&fixture));
    // Both answers came back to the first round: no resend.
    assert_eq!(mock.received(), decode(&fixture.queries));
    assert!(elapsed < Duration::from_millis(900), "took {elapsed:?}");
}

#[tokio::test]
async fn lookup_resends_after_a_lost_reply() {
    let fixture = fixture();
    let mock = Mock::start(answer_queries(&fixture, 1)).await;
    let (result, elapsed) = lookup(&mock, Duration::from_secs(5)).await;
    let info = result.expect("lookup ok").expect("receiver found");
    assert_eq!(info, expected_info(&fixture));
    let queries = decode(&fixture.queries);
    let received = mock.received();
    assert_eq!(received.len(), 2 * queries.len(), "one resend round");
    assert_eq!(received[..2], queries[..]);
    assert_eq!(received[2..], queries[..]);
    assert!(elapsed >= Duration::from_millis(900), "took {elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
}

#[tokio::test]
async fn lookup_times_out_without_replies() {
    let fixture = fixture();
    let mock = Mock::start(Box::new(|_: usize, _: &[u8]| Vec::new())).await;
    let (result, elapsed) = lookup(&mock, Duration::from_millis(1500)).await;
    assert_eq!(result.expect("a timeout is not an error"), None);
    assert!(elapsed >= Duration::from_millis(1450), "took {elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    // Sent at t = 0 s and t = 1 s.
    let queries = decode(&fixture.queries);
    let mut rounds = queries.clone();
    rounds.extend(queries);
    assert_eq!(mock.received(), rounds);
}

#[tokio::test]
async fn repeated_replies_count_once_and_partial_answers_are_used() {
    let fixture = fixture();
    let first = decode(&fixture.responses)[0].clone();
    // Junk, a query echo and the first response (repeated for every query): the lookup
    // waits for a second distinct response until the timeout, then uses what it has.
    let echo = decode(&fixture.queries)[0].clone();
    let mock = Mock::start(Box::new(move |_: usize, _: &[u8]| {
        vec![b"not dns".to_vec(), echo.clone(), first.clone()]
    }))
    .await;
    let (result, elapsed) = lookup(&mock, Duration::from_millis(1200)).await;
    let info = result
        .expect("lookup ok")
        .expect("first response is complete");
    assert_eq!(info, expected_info(&fixture));
    assert!(elapsed >= Duration::from_millis(1150), "took {elapsed:?}");
}

#[tokio::test]
async fn closed_port_means_no_receiver() {
    // Bind and release a port so nothing listens on it.
    let port = {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        socket.local_addr().expect("addr").port()
    };
    let started = Instant::now();
    let result = mdns::lookup_at(Ipv4Addr::LOCALHOST, port, Duration::from_millis(1500)).await;
    assert_eq!(result.expect("closed port is not an error"), None);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn unrelated_responder_yields_none() {
    // A responder that only knows a printer.
    let mut packet = vec![0x35, 0xFF, 0x84, 0x00, 0, 0, 0, 1, 0, 0, 0, 0];
    packet.extend_from_slice(b"\x04_ipp\x04_tcp\x05local\x00");
    packet.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 10, 0, 10]);
    packet.extend_from_slice(b"\x07Printer\xc0\x0c");
    let mock = Mock::start(Box::new(move |_: usize, _: &[u8]| vec![packet.clone()])).await;
    let (result, _) = lookup(&mock, Duration::from_millis(1200)).await;
    assert_eq!(result.expect("lookup ok"), None);
}

/// One real lookup, run by hand only:
/// `WINPODS_TEST_RECEIVER=<ip> cargo test -p winpods-airplay --test mdns -- --ignored --nocapture`
#[tokio::test]
#[ignore = "contacts a real receiver; set WINPODS_TEST_RECEIVER"]
async fn real_lookup() {
    let Some(address) = std::env::var("WINPODS_TEST_RECEIVER")
        .ok()
        .and_then(|value| value.parse::<Ipv4Addr>().ok())
    else {
        eprintln!("WINPODS_TEST_RECEIVER is not set; skipping");
        return;
    };
    let started = Instant::now();
    let info = mdns::lookup(address, Duration::from_secs(5))
        .await
        .expect("lookup ok")
        .expect("receiver answered");
    println!("lookup of {address} took {:?}", started.elapsed());
    println!("{info:#?}");
    println!(
        "airplay2={} transient={} text_metadata={}",
        info.is_airplay2(),
        info.supports_transient_pairing(),
        info.supports_text_metadata()
    );
    assert_eq!(info.address, address);
}
