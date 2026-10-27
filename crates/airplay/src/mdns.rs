//! Unicast mDNS lookup of one receiver (workstream: discovery). CONTRACT: `lookup`
//! is used by the engine.
//!
//! Port of pyatv's unicast scan (`pyatv/core/mdns.py`: `create_service_queries`,
//! `UnicastDnsSdClientProtocol`, `ServiceParser`; `pyatv/core/scan.py`:
//! `UnicastMdnsScanner`; the RAOP and AirPlay scan handlers and TXT parsers), plus
//! WinPods v0.1's `find_device` and the receiver-id collection in
//! `LiveStreamSession._connect`:
//!
//! * The queries are pyatv's, byte for byte ([`service_queries`]): DNS id 0x35FF, flags
//!   0x0120, PTR questions with class 0x8001 (IN plus the unicast-response bit) for
//!   [`SERVICES`]. pyatv puts three services in each message but slices
//!   `[i*3 .. i*3+4]`, so `_raop._tcp` is asked in both messages, and it appends
//!   `_sleep-proxy._udp` to each message.
//! * They go from an ephemeral UDP socket connected to `<ip>:5353`. Every query is
//!   resent each second until as many responses as queries have arrived or the
//!   timeout has passed.
//! * Responses are decoded by a small DNS codec ([`DnsMessage`]: name compression,
//!   PTR/SRV/TXT/A records, other types skipped), assembled into services from the
//!   answer and additional sections like pyatv's `ServiceParser` ([`parse_services`]),
//!   and turned into a [`ReceiverInfo`] ([`receiver_info`]).
//!
//! No TCP port knocking. Deliberate differences from pyatv, none of which changes the
//! result for a well-behaved receiver (the fixture):
//!
//! * Responses that arrived before the timeout are used. pyatv throws away everything
//!   when it times out, even a complete answer to the first query.
//! * A datagram counts as a response only when it is a well-formed DNS response that is
//!   not byte-identical to one already received. pyatv counts every datagram, so one
//!   repeated reply could end the lookup with a query unanswered.
//! * When the receiver's host record lists the address that was looked up, that address
//!   is used. pyatv takes the first non-link-local A record.
//! * DNS names are matched case-insensitively (as DNS requires). A malformed A, SRV or
//!   TXT record is skipped rather than discarding the whole message, invalid UTF-8 in a
//!   TXT value is decoded lossily, and IDNA (`xn--`) labels are kept as they are.
//! * The name is the RAOP instance name after '@', else the AirPlay instance name.
//!   pyatv uses whichever of the two services the receiver listed first; Apple
//!   receivers give the same name for both.
//!
//! Fixture: `tests/fixtures/mdns.json` (real, sanitized HomePod responses).

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::debug;

use crate::types::ReceiverInfo;

/// The multicast DNS port. Unicast queries go to this port on the receiver.
pub const MDNS_PORT: u16 = 5353;
/// DNS message id of pyatv's unicast queries.
pub const QUERY_ID: u16 = 0x35FF;
/// Header flags of pyatv's queries (`DnsMessage` default, 0x0120).
pub const QUERY_FLAGS: u16 = 0x0120;
/// Question class IN with the mDNS unicast-response ("QU") bit set.
pub const CLASS_IN_UNICAST: u16 = 0x8001;

/// DNS record type A (IPv4 address).
pub const TYPE_A: u16 = 1;
/// DNS record type PTR.
pub const TYPE_PTR: u16 = 12;
/// DNS record type TXT.
pub const TYPE_TXT: u16 = 16;
/// DNS record type SRV.
pub const TYPE_SRV: u16 = 33;

/// Apple's device information service (TXT `model`, e.g. `B520AP`).
pub const DEVICE_INFO_SERVICE: &str = "_device-info._tcp.local";
/// Bonjour sleep proxy service; pyatv appends a question for it to every message.
pub const SLEEP_PROXY_SERVICE: &str = "_sleep-proxy._udp.local";
/// AirPlay service.
pub const AIRPLAY_SERVICE: &str = "_airplay._tcp.local";
/// RAOP (AirPlay audio) service.
pub const RAOP_SERVICE: &str = "_raop._tcp.local";
/// AirPort Express base station service (asked for by pyatv's RAOP scanner).
pub const AIRPORT_SERVICE: &str = "_airport._tcp.local";

/// The service types pyatv's unicast scan asks for when scanning {RAOP, AirPlay}, in
/// pyatv's order (the order decides the bytes of the queries).
pub const SERVICES: [&str; 5] = [
    DEVICE_INFO_SERVICE,
    SLEEP_PROXY_SERVICE,
    AIRPLAY_SERVICE,
    RAOP_SERVICE,
    AIRPORT_SERVICE,
];

/// pyatv's `SERVICES_PER_MSG`.
const SERVICES_PER_MSG: usize = 3;
/// pyatv resends every query once a second.
const RESEND_INTERVAL: Duration = Duration::from_secs(1);
/// Upper bound for the lookup timeout, so deadline arithmetic cannot overflow.
const MAX_TIMEOUT: Duration = Duration::from_secs(3600);
/// Large enough for any UDP datagram, so Windows never fails a read with WSAEMSGSIZE.
const RECV_BUFFER_LEN: usize = 65_536;
/// DNS header length.
const HEADER_LEN: usize = 12;
/// Maximum encoded length of a domain name (RFC 1035 §3.1).
const MAX_NAME_WIRE_LEN: usize = 255;
/// Maximum label length (RFC 1035 §2.3.4).
const MAX_LABEL_LEN: usize = 63;
/// More compression pointers than a 255-byte name can need means a loop.
const MAX_POINTER_JUMPS: usize = 127;
/// QR bit of the header flags: set in responses.
const FLAG_RESPONSE: u16 = 0x8000;
/// Password bit in TXT `sf`/`flags` (pyatv `PASSWORD_BIT`).
const PASSWORD_BIT: u64 = 0x80;

/// Look up the AirPlay receiver at `address`. `Ok(None)` = it answered nothing usable.
///
/// Sends pyatv's unicast DNS-SD queries to `address:5353`, resending them every second
/// until every query got a response or `timeout` passed, and builds a [`ReceiverInfo`]
/// from the receiver's RAOP (required) and AirPlay services. Socket errors before any
/// response arrived are returned as `Err`, except a closed port (ICMP port
/// unreachable), which means nothing answered.
pub async fn lookup(address: Ipv4Addr, timeout: Duration) -> std::io::Result<Option<ReceiverInfo>> {
    lookup_at(address, MDNS_PORT, timeout).await
}

/// [`lookup`] against an arbitrary port, so tests can point it at a mock responder.
#[doc(hidden)]
pub async fn lookup_at(
    address: Ipv4Addr,
    port: u16,
    timeout: Duration,
) -> std::io::Result<Option<ReceiverInfo>> {
    let queries = service_queries(&SERVICES)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.connect(SocketAddrV4::new(address, port)).await?;

    let messages = exchange(&socket, &queries, timeout.min(MAX_TIMEOUT)).await?;
    let services = parse_services(&messages);
    let info = receiver_info(&services, address);
    debug!(
        %address,
        responses = messages.len(),
        services = services.len(),
        found = info.is_some(),
        "unicast mDNS lookup finished"
    );
    Ok(info)
}

/// Send `queries` (resending all of them every second) and collect distinct DNS
/// responses until there are as many as queries or `timeout` passes.
async fn exchange(
    socket: &UdpSocket,
    queries: &[Vec<u8>],
    timeout: Duration,
) -> io::Result<Vec<DnsMessage>> {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut next_send = start;
    let mut received: Vec<Vec<u8>> = Vec::new();
    let mut messages: Vec<DnsMessage> = Vec::new();
    let mut buf = vec![0u8; RECV_BUFFER_LEN];

    while messages.len() < queries.len() {
        let now = Instant::now();
        if now >= deadline {
            debug!(responses = messages.len(), "unicast mDNS lookup timed out");
            break;
        }
        if now >= next_send {
            for query in queries {
                if let Err(err) = socket.send(query).await {
                    return end_on_error(err, messages);
                }
            }
            next_send += RESEND_INTERVAL;
            if next_send <= now {
                next_send = now + RESEND_INTERVAL;
            }
        }

        let wake = next_send.min(deadline);
        let len = match tokio::time::timeout_at(wake, socket.recv(&mut buf)).await {
            // Time to resend, or to give up.
            Err(_elapsed) => continue,
            Ok(Ok(len)) => len,
            Ok(Err(err)) => return end_on_error(err, messages),
        };
        let Some(packet) = buf.get(..len) else {
            continue;
        };
        if received.iter().any(|seen| seen.as_slice() == packet) {
            continue;
        }
        match DnsMessage::parse(packet) {
            Ok(message) if message.is_response() => {
                received.push(packet.to_vec());
                messages.push(message);
            }
            Ok(_) => debug!("ignoring a DNS query from the receiver"),
            Err(err) => debug!("ignoring a malformed DNS response: {err}"),
        }
    }
    Ok(messages)
}

/// pyatv ends the lookup on any socket error and parses what it has. A closed port
/// (WSAECONNRESET after ICMP port unreachable) simply means nobody answered.
fn end_on_error(err: io::Error, messages: Vec<DnsMessage>) -> io::Result<Vec<DnsMessage>> {
    let closed = matches!(
        err.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
    );
    if closed || !messages.is_empty() {
        debug!(
            responses = messages.len(),
            "unicast mDNS lookup ended early: {err}"
        );
        Ok(messages)
    } else {
        Err(err)
    }
}

// ------------------------------------------------------------------------ DNS codec

/// Errors from encoding or decoding a DNS message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DnsError {
    /// The message ended in the middle of a field.
    #[error("DNS message is truncated")]
    Truncated,
    /// A label length byte used the reserved 01/10 prefixes.
    #[error("reserved DNS label type 0x{0:02x}")]
    BadLabelType(u8),
    /// A name is longer than 255 bytes on the wire.
    #[error("DNS name is longer than 255 bytes")]
    NameTooLong,
    /// Compression pointers form a loop.
    #[error("DNS name compression loop")]
    PointerLoop,
    /// A label in a received name is not UTF-8.
    #[error("DNS label is not UTF-8")]
    BadUtf8,
    /// A name to encode has an empty label or one longer than 63 bytes.
    #[error("invalid label in DNS name")]
    BadLabel,
    /// More than 65535 questions.
    #[error("too many DNS questions")]
    TooManyQuestions,
}

/// A DNS question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Dotted name, e.g. `_raop._tcp.local`.
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
}

impl Question {
    /// A PTR question with the unicast-response bit, as pyatv asks them.
    pub fn unicast_ptr(name: &str) -> Self {
        Self {
            name: name.to_string(),
            qtype: TYPE_PTR,
            qclass: CLASS_IN_UNICAST,
        }
    }
}

/// Decoded record data. Only the types DNS-SD discovery needs are decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordData {
    A(Ipv4Addr),
    Ptr(String),
    Srv {
        priority: u16,
        weight: u16,
        port: u16,
        target: String,
    },
    /// The character strings of a TXT record, undecoded.
    Txt(Vec<Vec<u8>>),
    /// Any other type, or an A/SRV/TXT record whose data is malformed.
    Other,
}

/// A DNS resource record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Owner name, labels joined with '.' (as pyatv does, so a label containing a dot
    /// is not distinguishable from two labels).
    pub name: String,
    pub rtype: u16,
    /// Class including the mDNS cache-flush bit.
    pub class: u16,
    pub ttl: u32,
    pub data: RecordData,
}

/// A decoded DNS message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsMessage {
    pub id: u16,
    pub flags: u16,
    pub questions: Vec<Question>,
    pub answers: Vec<Record>,
    pub authorities: Vec<Record>,
    pub additionals: Vec<Record>,
}

impl DnsMessage {
    /// Decode a DNS message, following name compression pointers anywhere in it.
    /// Bytes after the last record are ignored.
    pub fn parse(packet: &[u8]) -> Result<Self, DnsError> {
        let mut reader = Reader { packet, pos: 0 };
        let id = reader.u16()?;
        let flags = reader.u16()?;
        let qdcount = reader.u16()?;
        let ancount = reader.u16()?;
        let nscount = reader.u16()?;
        let arcount = reader.u16()?;

        let mut questions = Vec::new();
        for _ in 0..qdcount {
            let name = reader.name()?;
            let qtype = reader.u16()?;
            let qclass = reader.u16()?;
            questions.push(Question {
                name,
                qtype,
                qclass,
            });
        }
        let answers = reader.records(ancount)?;
        let authorities = reader.records(nscount)?;
        let additionals = reader.records(arcount)?;
        Ok(Self {
            id,
            flags,
            questions,
            answers,
            authorities,
            additionals,
        })
    }

    /// QR bit set.
    pub fn is_response(&self) -> bool {
        self.flags & FLAG_RESPONSE != 0
    }
}

/// Encode a DNS query without name compression (pyatv's `DnsMessage.pack`).
pub fn build_query(id: u16, flags: u16, questions: &[Question]) -> Result<Vec<u8>, DnsError> {
    let count = u16::try_from(questions.len()).map_err(|_| DnsError::TooManyQuestions)?;
    let mut out = Vec::with_capacity(HEADER_LEN + questions.len() * 32);
    for field in [id, flags, count, 0, 0, 0] {
        out.extend_from_slice(&field.to_be_bytes());
    }
    for question in questions {
        encode_name(&question.name, &mut out)?;
        out.extend_from_slice(&question.qtype.to_be_bytes());
        out.extend_from_slice(&question.qclass.to_be_bytes());
    }
    Ok(out)
}

/// pyatv's `create_service_queries(services, PTR)`: messages of PTR questions, three
/// services per message sliced as `[i*3 .. i*3+4]`, each followed by a sleep-proxy
/// question.
pub fn service_queries(services: &[&str]) -> Result<Vec<Vec<u8>>, DnsError> {
    (0..services.len().div_ceil(SERVICES_PER_MSG))
        .map(|i| {
            let questions: Vec<Question> = services
                .iter()
                .skip(i * SERVICES_PER_MSG)
                .take(SERVICES_PER_MSG + 1)
                .chain(std::iter::once(&SLEEP_PROXY_SERVICE))
                .map(|service| Question::unicast_ptr(service))
                .collect();
            build_query(QUERY_ID, QUERY_FLAGS, &questions)
        })
        .collect()
}

/// Append `name` (dotted, optional trailing dot) as uncompressed labels plus the root.
fn encode_name(name: &str, out: &mut Vec<u8>) -> Result<(), DnsError> {
    let name = name.strip_suffix('.').unwrap_or(name);
    let labels: Vec<&[u8]> = if name.is_empty() {
        Vec::new()
    } else {
        name.split('.').map(str::as_bytes).collect()
    };
    let mut wire_len = 1;
    for label in &labels {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(DnsError::BadLabel);
        }
        wire_len += label.len() + 1;
    }
    if wire_len > MAX_NAME_WIRE_LEN {
        return Err(DnsError::NameTooLong);
    }
    for label in labels {
        // Checked above: at most 63, so it fits the length byte.
        out.push(label.len() as u8);
        out.extend_from_slice(label);
    }
    out.push(0);
    Ok(())
}

/// Bounds-checked big-endian reader over a whole message.
struct Reader<'a> {
    packet: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, len: usize) -> Result<&'a [u8], DnsError> {
        let end = self.pos.checked_add(len).ok_or(DnsError::Truncated)?;
        let bytes = self.packet.get(self.pos..end).ok_or(DnsError::Truncated)?;
        self.pos = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DnsError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16, DnsError> {
        self.array().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Result<u32, DnsError> {
        self.array().map(u32::from_be_bytes)
    }

    fn name(&mut self) -> Result<String, DnsError> {
        let (name, end) = read_name(self.packet, self.pos)?;
        self.pos = end;
        Ok(name)
    }

    fn records(&mut self, count: u16) -> Result<Vec<Record>, DnsError> {
        let mut records = Vec::new();
        for _ in 0..count {
            records.push(self.record()?);
        }
        Ok(records)
    }

    fn record(&mut self) -> Result<Record, DnsError> {
        let name = self.name()?;
        let rtype = self.u16()?;
        let class = self.u16()?;
        let ttl = self.u32()?;
        let rdlength = usize::from(self.u16()?);
        let rdata_start = self.pos;
        let rdata = self.bytes(rdlength)?;
        let data = match rtype {
            TYPE_A => <[u8; 4]>::try_from(rdata).map_or(RecordData::Other, |octets| {
                RecordData::A(Ipv4Addr::from(octets))
            }),
            TYPE_PTR => RecordData::Ptr(read_name(self.packet, rdata_start)?.0),
            TYPE_SRV if rdlength >= 7 => {
                let mut srv = Reader {
                    packet: self.packet,
                    pos: rdata_start,
                };
                RecordData::Srv {
                    priority: srv.u16()?,
                    weight: srv.u16()?,
                    port: srv.u16()?,
                    target: srv.name()?,
                }
            }
            TYPE_TXT => read_txt(rdata).map_or(RecordData::Other, RecordData::Txt),
            _ => RecordData::Other,
        };
        Ok(Record {
            name,
            rtype,
            class,
            ttl,
            data,
        })
    }
}

/// Read the (possibly compressed) name starting at `start`. Returns the dotted name and
/// the position just past its encoding at `start`.
fn read_name(packet: &[u8], start: usize) -> Result<(String, usize), DnsError> {
    let mut name = String::new();
    let mut pos = start;
    let mut end = None;
    let mut wire_len = 1;
    let mut jumps = 0;
    loop {
        let len = *packet.get(pos).ok_or(DnsError::Truncated)?;
        match len & 0xC0 {
            0x00 if len == 0 => {
                pos += 1;
                break;
            }
            0x00 => {
                let label_start = pos + 1;
                let label_end = label_start + usize::from(len);
                let label = packet
                    .get(label_start..label_end)
                    .ok_or(DnsError::Truncated)?;
                wire_len += label.len() + 1;
                if wire_len > MAX_NAME_WIRE_LEN {
                    return Err(DnsError::NameTooLong);
                }
                let label = std::str::from_utf8(label).map_err(|_| DnsError::BadUtf8)?;
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(label);
                pos = label_end;
            }
            0xC0 => {
                let low = *packet.get(pos + 1).ok_or(DnsError::Truncated)?;
                end.get_or_insert(pos + 2);
                jumps += 1;
                if jumps > MAX_POINTER_JUMPS {
                    return Err(DnsError::PointerLoop);
                }
                pos = (usize::from(len & 0x3F) << 8) | usize::from(low);
            }
            _ => return Err(DnsError::BadLabelType(len)),
        }
    }
    Ok((name, end.unwrap_or(pos)))
}

/// Split TXT record data into its character strings. `None` if a length runs past
/// the end.
fn read_txt(mut rdata: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut strings = Vec::new();
    while let Some((&len, rest)) = rdata.split_first() {
        let (string, rest) = rest.split_at_checked(usize::from(len))?;
        strings.push(string.to_vec());
        rdata = rest;
    }
    Some(strings)
}

// ------------------------------------------------------------------------- services

/// A DNS-SD service instance assembled from PTR/SRV/TXT/A records (pyatv's
/// `mdns.Service`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Service type and domain, e.g. `_raop._tcp.local`.
    pub service_type: String,
    /// Instance name, e.g. `02AABBCCDDEE@Living Room`.
    pub name: String,
    /// First non-link-local A record of the SRV target (pyatv's choice).
    pub address: Option<Ipv4Addr>,
    /// Every non-link-local A record of the SRV target, in order, deduplicated.
    pub addresses: Vec<Ipv4Addr>,
    /// SRV port; 0 without an SRV record.
    pub port: u16,
    /// TXT properties in record order: keys lowercased (pyatv's `CaseInsensitiveDict`),
    /// a repeated key replaces the earlier value in place.
    pub properties: Vec<(String, String)>,
}

impl Service {
    /// TXT property by case-insensitive key.
    pub fn property(&self, key: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    }

    fn is_type(&self, service_type: &str) -> bool {
        self.service_type.eq_ignore_ascii_case(service_type)
    }

    /// pyatv only handles services with an address and a port.
    fn is_usable(&self) -> bool {
        self.address.is_some() && self.port != 0
    }
}

/// Assemble services from the answer and additional records of `messages` (pyatv's
/// `ServiceParser`). The authority section is not used, as in pyatv.
pub fn parse_services(messages: &[DnsMessage]) -> Vec<Service> {
    struct Entry<'m> {
        name: &'m str,
        srv: Option<(u16, &'m str)>,
        txt: Option<&'m [Vec<u8>]>,
        a: Vec<Ipv4Addr>,
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    // PTRs from service types to instances: qname -> instance, first-seen order.
    let mut ptrs: Vec<(&str, &str)> = Vec::new();

    let records = messages
        .iter()
        .flat_map(|message| message.answers.iter().chain(&message.additionals));
    for record in records {
        if let RecordData::Ptr(target) = &record.data
            && record.name.starts_with('_')
        {
            match ptrs
                .iter_mut()
                .find(|(qname, _)| qname.eq_ignore_ascii_case(&record.name))
            {
                Some(slot) => slot.1 = target.as_str(),
                None => ptrs.push((record.name.as_str(), target.as_str())),
            }
            continue;
        }

        let slot = *index
            .entry(record.name.to_ascii_lowercase())
            .or_insert_with(|| {
                entries.push(Entry {
                    name: &record.name,
                    srv: None,
                    txt: None,
                    a: Vec::new(),
                });
                entries.len() - 1
            });
        let Some(entry) = entries.get_mut(slot) else {
            continue;
        };
        match &record.data {
            RecordData::Srv { port, target, .. } => {
                entry.srv.get_or_insert((*port, target.as_str()));
            }
            RecordData::Txt(strings) => {
                entry.txt.get_or_insert(strings.as_slice());
            }
            RecordData::A(address) if !entry.a.contains(address) => entry.a.push(*address),
            _ => {}
        }
    }

    let mut services = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in &entries {
        let Some((instance, ptr_name)) = split_service_name(entry.name) else {
            continue;
        };
        let (port, target) = entry
            .srv
            .map_or((0, None), |(port, target)| (port, Some(target)));
        let addresses: Vec<Ipv4Addr> = target
            .and_then(|target| index.get(&target.to_ascii_lowercase()))
            .and_then(|&slot| entries.get(slot))
            .map(|host| {
                host.a
                    .iter()
                    .copied()
                    .filter(|a| !a.is_link_local())
                    .collect()
            })
            .unwrap_or_default();
        seen.insert(entry.name.to_ascii_lowercase());
        services.push(Service {
            service_type: ptr_name,
            name: instance,
            address: addresses.first().copied(),
            addresses,
            port,
            properties: entry.txt.map(decode_txt).unwrap_or_default(),
        });
    }

    // PTRs to instances without records of their own become placeholders.
    for (qname, instance) in ptrs {
        if seen.insert(instance.to_ascii_lowercase()) {
            services.push(Service {
                service_type: qname.to_string(),
                name: instance.split('.').next().unwrap_or_default().to_string(),
                address: None,
                addresses: Vec::new(),
                port: 0,
                properties: Vec::new(),
            });
        }
    }
    services
}

/// pyatv's `ServiceInstanceName.split_name`: find the first `_x._tcp`/`_x._udp` label
/// pair; everything before is the instance (dots allowed), the rest the domain.
/// Returns `(instance, "<service>.<domain>")`.
fn split_service_name(name: &str) -> Option<(String, String)> {
    let labels: Vec<&str> = name.split('.').collect();
    let index = labels.windows(2).position(|pair| {
        pair[0].starts_with('_')
            && (pair[1].eq_ignore_ascii_case("_tcp") || pair[1].eq_ignore_ascii_case("_udp"))
    })?;
    let instance = labels.get(..index).unwrap_or_default().join(".");
    let service = labels.get(index..index + 2).unwrap_or_default().join(".");
    let domain = labels.get(index + 2..).unwrap_or_default().join(".");
    Some((instance, format!("{service}.{domain}")))
}

/// pyatv's `parse_txt_dict` followed by `_decode_properties`.
fn decode_txt(strings: &[Vec<u8>]) -> Vec<(String, String)> {
    let mut properties: Vec<(String, String)> = Vec::new();
    for string in strings {
        let (key, value) = match string.iter().position(|&b| b == b'=') {
            Some(eq) => {
                let (key, value) = string.split_at(eq);
                (key, value.get(1..).unwrap_or_default())
            }
            None => (string.as_slice(), &[][..]),
        };
        // Keys are ASCII; pyatv skips empty and non-ASCII keys (and so do we for a
        // bare empty string, which pyatv would store under the key "").
        if key.is_empty() || !key.is_ascii() {
            continue;
        }
        let key = String::from_utf8_lossy(key).to_ascii_lowercase();
        let value = decode_value(value);
        match properties.iter_mut().find(|(name, _)| *name == key) {
            Some(slot) => slot.1 = value,
            None => properties.push((key, value)),
        }
    }
    properties
}

/// pyatv's `decode_value`: non-breaking spaces (C2 A0, 00 A0) become spaces, then UTF-8.
fn decode_value(value: &[u8]) -> String {
    let value = replace_with_space(&replace_with_space(value, [0xC2, 0xA0]), [0x00, 0xA0]);
    match String::from_utf8(value) {
        Ok(text) => text,
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    }
}

/// Replace every non-overlapping occurrence of `pattern` with a space.
fn replace_with_space(input: &[u8], pattern: [u8; 2]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        if rest.starts_with(&pattern) {
            out.push(b' ');
            rest = &rest[2..];
        } else {
            out.push(rest[0]);
            rest = &rest[1..];
        }
    }
    out
}

// -------------------------------------------------------------------- receiver info

/// Build the [`ReceiverInfo`] for a lookup of `requested` from parsed services.
///
/// Requires a RAOP service with an address and a port (WinPods v0.1 `find_device`).
/// The RAOP service whose host lists `requested` wins, else the first one. The AirPlay
/// service is the one on the same address. `None` if there is no usable RAOP service.
pub fn receiver_info(services: &[Service], requested: Ipv4Addr) -> Option<ReceiverInfo> {
    let raop_services: Vec<&Service> = services
        .iter()
        .filter(|s| s.is_usable() && s.is_type(RAOP_SERVICE))
        .collect();
    let (raop, address) = raop_services
        .iter()
        .find(|s| s.addresses.contains(&requested))
        .map(|s| (*s, requested))
        .or_else(|| {
            raop_services
                .first()
                .and_then(|s| s.address.map(|address| (*s, address)))
        })?;
    let airplay = services
        .iter()
        .find(|s| s.is_usable() && s.is_type(AIRPLAY_SERVICE) && s.addresses.contains(&address));

    let name = match raop.name.split_once('@') {
        Some((_, name)) => name.to_string(),
        None => airplay.map_or(&raop.name, |airplay| &airplay.name).clone(),
    };

    // pyatv's raw_model: AirPlay `model` (listed first by Apple receivers), RAOP `am`.
    let model = airplay
        .and_then(|airplay| airplay.property("model"))
        .filter(|model| !model.is_empty())
        .or_else(|| raop.property("am").filter(|model| !model.is_empty()))
        .or_else(|| {
            services
                .iter()
                .find(|s| s.is_type(DEVICE_INFO_SERVICE))
                .and_then(|s| s.property("model"))
        })
        .unwrap_or_default()
        .to_string();

    // pyatv reads RAOP `ft`, then `features`; fall back to the AirPlay service.
    let features = [
        raop.property("ft"),
        raop.property("features"),
        airplay.and_then(|airplay| airplay.property("features")),
        airplay.and_then(|airplay| airplay.property("ft")),
    ]
    .into_iter()
    .flatten()
    .find(|value| !value.is_empty())
    .and_then(parse_features)
    .unwrap_or(0);

    // `LiveStreamSession._connect`: gid/pi/psi of AirPlay then RAOP, cut at '+',
    // deduplicated (empty values skipped).
    let mut receiver_ids: Vec<String> = Vec::new();
    for service in airplay.into_iter().chain(std::iter::once(raop)) {
        for key in ["gid", "pi", "psi"] {
            let Some(value) = service.property(key) else {
                continue;
            };
            let id = value.split('+').next().unwrap_or_default();
            if !id.is_empty() && !receiver_ids.iter().any(|known| known == id) {
                receiver_ids.push(id.to_string());
            }
        }
    }

    Some(ReceiverInfo {
        name,
        model,
        address,
        raop_port: raop.port,
        features,
        metadata_types: parse_int_list(raop.property("md")),
        encryption_types: parse_int_list(raop.property("et")),
        receiver_ids,
        requires_password: requires_password(raop),
        raop_txt: raop.properties.clone(),
        airplay_txt: airplay
            .map(|airplay| airplay.properties.clone())
            .unwrap_or_default(),
    })
}

/// pyatv's `parse_features`: `0xLOW` or `0xLOW,0xHIGH` (1 to 8 hex digits each) →
/// `(HIGH << 32) | LOW`.
pub fn parse_features(value: &str) -> Option<u64> {
    fn hex32(digits: &str) -> Option<u64> {
        let valid =
            (1..=8).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_hexdigit());
        if valid {
            u64::from_str_radix(digits, 16).ok()
        } else {
            None
        }
    }
    let rest = value.strip_prefix("0x")?;
    match rest.split_once(',') {
        None => hex32(rest),
        Some((low, high)) => Some((hex32(high.strip_prefix("0x")?)? << 32) | hex32(low)?),
    }
}

/// pyatv's `is_password_required`: `pw` is "true", or bit 0x80 of `sf`/`flags`.
fn requires_password(service: &Service) -> bool {
    if service
        .property("pw")
        .is_some_and(|pw| pw.eq_ignore_ascii_case("true"))
    {
        return true;
    }
    let flags = [service.property("sf"), service.property("flags")]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .and_then(parse_hex)
        .unwrap_or(0);
    flags & PASSWORD_BIT != 0
}

/// Python's `int(value, 16)` for flag words: optional `0x`, hex digits. Only the low
/// 64 bits are kept (callers test single bits).
fn parse_hex(value: &str) -> Option<u64> {
    let value = value.trim();
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let low = digits.get(digits.len().saturating_sub(16)..)?;
    u64::from_str_radix(low, 16).ok()
}

/// pyatv's `get_metadata_types`/`get_encryption_types` input: a comma-separated list of
/// integers. Any non-integer makes the whole list empty (as pyatv); values above 255
/// are dropped.
fn parse_int_list(value: Option<&str>) -> Vec<u8> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in value.split(',') {
        let Ok(number) = part.trim().parse::<i64>() else {
            return Vec::new();
        };
        if let Ok(byte) = u8::try_from(number) {
            out.push(byte);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/mdns.json");

    fn fixture() -> serde_json::Value {
        serde_json::from_str(FIXTURE).expect("mdns.json parses")
    }

    fn hex_list(value: &serde_json::Value) -> Vec<Vec<u8>> {
        value
            .as_array()
            .expect("array")
            .iter()
            .map(|item| hex::decode(item.as_str().expect("hex string")).expect("valid hex"))
            .collect()
    }

    fn fixture_messages() -> Vec<DnsMessage> {
        hex_list(&fixture()["responses"])
            .iter()
            .map(|packet| DnsMessage::parse(packet).expect("fixture response parses"))
            .collect()
    }

    fn service(service_type: &str, name: &str, address: Option<Ipv4Addr>, port: u16) -> Service {
        Service {
            service_type: service_type.to_string(),
            name: name.to_string(),
            address,
            addresses: address.into_iter().collect(),
            port,
            properties: Vec::new(),
        }
    }

    fn props(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Header + one record with the given owner bytes, type and rdata.
    fn packet_with_record(owner: &[u8], rtype: u16, rdata: &[u8]) -> Vec<u8> {
        let mut out = vec![0x35, 0xFF, 0x84, 0x00, 0, 0, 0, 1, 0, 0, 0, 0];
        out.extend_from_slice(owner);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&[0x80, 0x01, 0, 0, 0, 10]);
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
        out
    }

    #[test]
    fn services_match_fixture() {
        let expected: Vec<String> = fixture()["services"]
            .as_array()
            .expect("array")
            .iter()
            .map(|s| s.as_str().expect("string").to_string())
            .collect();
        assert_eq!(SERVICES.to_vec(), expected);
    }

    #[test]
    fn queries_are_byte_identical_to_pyatv() {
        let queries = service_queries(&SERVICES).expect("queries encode");
        assert_eq!(queries, hex_list(&fixture()["queries"]));
        // `_raop._tcp` is in both messages; each ends with a sleep-proxy question.
        let first = DnsMessage::parse(&queries[0]).expect("parses");
        let second = DnsMessage::parse(&queries[1]).expect("parses");
        assert_eq!(first.id, QUERY_ID);
        assert_eq!(first.flags, QUERY_FLAGS);
        assert_eq!(first.questions.len(), 5);
        assert_eq!(second.questions.len(), 3);
        assert_eq!(second.questions[0], Question::unicast_ptr(RAOP_SERVICE));
        assert_eq!(
            second.questions[2],
            Question::unicast_ptr(SLEEP_PROXY_SERVICE)
        );
        assert!(!first.is_response());
    }

    #[test]
    fn service_queries_chunking() {
        assert!(service_queries(&[]).expect("encodes").is_empty());
        let one = service_queries(&["_a._tcp.local"]).expect("encodes");
        assert_eq!(one.len(), 1);
        let parsed = DnsMessage::parse(&one[0]).expect("parses");
        assert_eq!(parsed.questions.len(), 2);
        // Seven services: [0..4], [3..7], [6..7].
        let seven: Vec<String> = (0..7).map(|i| format!("_s{i}._tcp.local")).collect();
        let seven: Vec<&str> = seven.iter().map(String::as_str).collect();
        let queries = service_queries(&seven).expect("encodes");
        let counts: Vec<usize> = queries
            .iter()
            .map(|q| DnsMessage::parse(q).expect("parses").questions.len())
            .collect();
        assert_eq!(counts, vec![5, 5, 2]);
    }

    #[test]
    fn encode_name_rejects_bad_labels() {
        let mut out = Vec::new();
        assert_eq!(encode_name("a..b", &mut out), Err(DnsError::BadLabel));
        let long_label = "x".repeat(64);
        assert_eq!(encode_name(&long_label, &mut out), Err(DnsError::BadLabel));
        let long_name = vec!["y".repeat(63); 4].join(".");
        assert_eq!(
            encode_name(&long_name, &mut out),
            Err(DnsError::NameTooLong)
        );
        assert!(out.is_empty());
        encode_name("local.", &mut out).expect("trailing dot is fine");
        assert_eq!(out, b"\x05local\x00");
        out.clear();
        encode_name("", &mut out).expect("root");
        assert_eq!(out, [0]);
    }

    #[test]
    fn parses_fixture_responses_with_compression() {
        let messages = fixture_messages();
        assert_eq!(messages.len(), 2);
        let first = &messages[0];
        assert!(first.is_response());
        assert_eq!(first.id, QUERY_ID);
        assert_eq!(first.questions.len(), 3);
        assert_eq!(first.answers.len(), 3);
        assert!(first.authorities.is_empty());
        assert_eq!(first.additionals.len(), 10);
        assert_eq!(
            first.answers[2].data,
            RecordData::Ptr("02AABBCCDDEE@Living Room._raop._tcp.local".to_string())
        );
        assert_eq!(first.answers[0].class, 0x0001);
        let srv = first
            .additionals
            .iter()
            .find(|r| r.rtype == TYPE_SRV && r.name.starts_with("02AABBCCDDEE@"))
            .expect("RAOP SRV");
        assert_eq!(
            srv.data,
            RecordData::Srv {
                priority: 0,
                weight: 0,
                port: 7000,
                target: "Living-Room.local".to_string()
            }
        );
        let a = first
            .additionals
            .iter()
            .find(|r| r.rtype == TYPE_A)
            .expect("A record");
        assert_eq!(a.name, "Living-Room.local");
        assert_eq!(a.data, RecordData::A(Ipv4Addr::new(192, 168, 1, 155)));
        // AAAA records are kept as `Other`.
        assert!(
            first
                .additionals
                .iter()
                .any(|r| r.rtype == 28 && r.data == RecordData::Other)
        );
        assert_eq!(
            first.additionals[0].name,
            "70-35-60-63.1 Living Room._sleep-proxy._udp.local"
        );
    }

    #[test]
    fn parsed_services_match_pyatv() {
        let services = parse_services(&fixture_messages());
        let expected = fixture()["expected"]["parsed_services"].clone();
        let expected = expected.as_array().expect("array");
        assert_eq!(services.len(), expected.len());
        for (service, want) in services.iter().zip(expected) {
            assert_eq!(service.service_type, want["type"].as_str().expect("type"));
            assert_eq!(service.name, want["name"].as_str().expect("name"));
            assert_eq!(service.port, want["port"].as_u64().expect("port") as u16);
            let address = want["address"]
                .as_str()
                .map(|a| a.parse::<Ipv4Addr>().expect("ip"));
            assert_eq!(service.address, address);
        }
        let device_info = &services[2];
        assert_eq!(device_info.property("MODEL"), Some("B520AP"));
    }

    #[test]
    fn receiver_info_from_fixture() {
        let fixture = fixture();
        let expected = &fixture["expected"];
        let services = parse_services(&fixture_messages());
        let info = receiver_info(&services, Ipv4Addr::new(192, 168, 1, 155)).expect("found");
        assert_eq!(info.name, expected["name"].as_str().expect("name"));
        assert_eq!(info.model, expected["model"].as_str().expect("model"));
        assert_eq!(
            info.address.to_string(),
            expected["address"].as_str().expect("ip")
        );
        assert_eq!(
            u64::from(info.raop_port),
            expected["raop_port"].as_u64().expect("port")
        );
        assert_eq!(
            info.features,
            expected["features"]["_raop._tcp"].as_u64().expect("ft")
        );
        assert_eq!(info.metadata_types, vec![0, 1, 2]);
        assert_eq!(info.encryption_types, vec![0, 3, 5]);
        let ids: Vec<&str> = expected["receiver_ids"]
            .as_array()
            .expect("array")
            .iter()
            .map(|v| v.as_str().expect("string"))
            .collect();
        assert_eq!(info.receiver_ids, ids);
        assert!(!info.requires_password);
        assert!(info.is_airplay2());
        assert!(info.supports_transient_pairing());
        assert!(info.supports_text_metadata());
        for (field, service) in [
            (&info.raop_txt, "_raop._tcp"),
            (&info.airplay_txt, "_airplay._tcp"),
        ] {
            let want = expected["txt"][service].as_object().expect("txt object");
            assert_eq!(field.len(), want.len());
            for (key, value) in field {
                assert_eq!(want[key].as_str(), Some(value.as_str()), "{service} {key}");
            }
        }
        // Record order is kept.
        let keys: Vec<&str> = info.raop_txt.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "cn", "da", "et", "ft", "sf", "md", "am", "pk", "tp", "vn", "vs", "ov", "vv"
            ]
        );
    }

    #[test]
    fn first_response_alone_is_enough() {
        let messages = fixture_messages();
        let services = parse_services(&messages[..1]);
        let full = receiver_info(&parse_services(&messages), Ipv4Addr::LOCALHOST);
        assert_eq!(receiver_info(&services, Ipv4Addr::LOCALHOST), full);
        // The second response alone has no AirPlay service: ids from RAOP only.
        let raop_only = receiver_info(&parse_services(&messages[1..]), Ipv4Addr::LOCALHOST)
            .expect("RAOP is enough");
        assert_eq!(raop_only.name, "Living Room");
        assert_eq!(raop_only.model, "AudioAccessory5,1");
        assert!(raop_only.airplay_txt.is_empty());
        assert!(raop_only.receiver_ids.is_empty());
    }

    #[test]
    fn requested_address_wins_on_multi_homed_hosts() {
        let mut raop = service(
            RAOP_SERVICE,
            "AA@Den",
            Some(Ipv4Addr::new(10, 0, 0, 2)),
            7000,
        );
        raop.addresses.push(Ipv4Addr::new(192, 168, 1, 20));
        let mut airplay = service(
            AIRPLAY_SERVICE,
            "Den",
            Some(Ipv4Addr::new(10, 0, 0, 2)),
            7000,
        );
        airplay.addresses.push(Ipv4Addr::new(192, 168, 1, 20));
        airplay.properties = props(&[("model", "AppleTV11,1")]);
        let info =
            receiver_info(&[raop.clone(), airplay], Ipv4Addr::new(192, 168, 1, 20)).expect("found");
        assert_eq!(info.address, Ipv4Addr::new(192, 168, 1, 20));
        assert_eq!(info.model, "AppleTV11,1");
        // Unknown requested address: pyatv's first address.
        let info = receiver_info(&[raop], Ipv4Addr::new(172, 16, 0, 1)).expect("found");
        assert_eq!(info.address, Ipv4Addr::new(10, 0, 0, 2));
    }

    #[test]
    fn sleep_proxied_devices_do_not_shadow_the_requested_one() {
        let other = service(
            RAOP_SERVICE,
            "BB@Bedroom",
            Some(Ipv4Addr::new(192, 168, 1, 30)),
            7000,
        );
        let wanted = service(
            RAOP_SERVICE,
            "AA@Kitchen",
            Some(Ipv4Addr::new(192, 168, 1, 31)),
            7000,
        );
        let info = receiver_info(&[other, wanted], Ipv4Addr::new(192, 168, 1, 31)).expect("found");
        assert_eq!(info.name, "Kitchen");
    }

    #[test]
    fn raop_without_port_or_address_is_not_a_receiver() {
        let no_port = service(RAOP_SERVICE, "AA@Den", Some(Ipv4Addr::new(10, 0, 0, 2)), 0);
        let no_address = service(RAOP_SERVICE, "AA@Den", None, 7000);
        let airplay = service(
            AIRPLAY_SERVICE,
            "Den",
            Some(Ipv4Addr::new(10, 0, 0, 2)),
            7000,
        );
        assert_eq!(
            receiver_info(&[no_port, no_address, airplay], Ipv4Addr::new(10, 0, 0, 2)),
            None
        );
        assert_eq!(receiver_info(&[], Ipv4Addr::LOCALHOST), None);
    }

    #[test]
    fn name_selection() {
        let address = Some(Ipv4Addr::new(10, 0, 0, 2));
        let with_at = service(RAOP_SERVICE, "AA@B@Den", address, 7000);
        let info = receiver_info(&[with_at], Ipv4Addr::LOCALHOST).expect("found");
        assert_eq!(info.name, "B@Den");
        let without_at = service(RAOP_SERVICE, "Den Speaker", address, 7000);
        let airplay = service(AIRPLAY_SERVICE, "Den", address, 7000);
        let info =
            receiver_info(&[without_at.clone(), airplay], Ipv4Addr::LOCALHOST).expect("found");
        assert_eq!(info.name, "Den");
        let info = receiver_info(&[without_at], Ipv4Addr::LOCALHOST).expect("found");
        assert_eq!(info.name, "Den Speaker");
    }

    #[test]
    fn txt_derived_fields() {
        let address = Some(Ipv4Addr::new(10, 0, 0, 2));
        let mut raop = service(RAOP_SERVICE, "AA@Den", address, 7000);
        raop.properties = props(&[
            ("features", "0x5A7FFFF7,0x1E"),
            ("md", "0, 1,2"),
            ("et", "0,x"),
            ("pw", "TRUE"),
            ("gid", "G1+x"),
            ("psi", "P1"),
        ]);
        let mut airplay = service(AIRPLAY_SERVICE, "Den", address, 7000);
        airplay.properties = props(&[("pi", "P1"), ("psi", "")]);
        let info = receiver_info(&[raop.clone(), airplay], Ipv4Addr::LOCALHOST).expect("found");
        assert_eq!(info.features, 0x1E_5A7F_FFF7);
        assert_eq!(info.metadata_types, vec![0, 1, 2]);
        assert!(info.encryption_types.is_empty());
        assert!(info.requires_password);
        // AirPlay first, then RAOP; cut at '+'; duplicates and empty values dropped.
        assert_eq!(info.receiver_ids, vec!["P1", "G1"]);
        assert_eq!(info.model, "");

        raop.properties = props(&[("sf", ""), ("flags", "0x84"), ("ft", "bogus")]);
        let info = receiver_info(&[raop], Ipv4Addr::LOCALHOST).expect("found");
        assert!(info.requires_password);
        assert_eq!(info.features, 0);
    }

    #[test]
    fn feature_strings() {
        assert_eq!(
            parse_features("0x4A7FCA00,0x3C354BD0"),
            Some(0x3C35_4BD0_4A7F_CA00)
        );
        assert_eq!(
            parse_features("0x445F8A00,0x1C340"),
            Some(0x1_C340_445F_8A00)
        );
        assert_eq!(parse_features("0x77"), Some(0x77));
        assert_eq!(parse_features("0x"), None);
        assert_eq!(parse_features("77"), None);
        assert_eq!(parse_features("0x123456789"), None);
        assert_eq!(parse_features("0x1,2"), None);
        assert_eq!(parse_features("0x1,0x"), None);
        assert_eq!(parse_features("0x+1"), None);
        assert_eq!(parse_features("0X1"), None);
    }

    #[test]
    fn hex_flags_and_int_lists() {
        assert_eq!(parse_hex("0x1a0404"), Some(0x1a0404));
        assert_eq!(parse_hex(" 0X80 "), Some(0x80));
        assert_eq!(parse_hex("84"), Some(0x84));
        assert_eq!(parse_hex("0x"), None);
        assert_eq!(parse_hex("0xzz"), None);
        assert_eq!(parse_hex("0x1ffffffffffffffff"), Some(u64::MAX));
        assert_eq!(parse_int_list(None), Vec::<u8>::new());
        assert_eq!(parse_int_list(Some("")), Vec::<u8>::new());
        assert_eq!(parse_int_list(Some("0,3,5")), vec![0, 3, 5]);
        assert_eq!(parse_int_list(Some("0,300,-1,4")), vec![0, 4]);
        assert_eq!(parse_int_list(Some("0,,1")), Vec::<u8>::new());
    }

    #[test]
    fn txt_decoding_quirks() {
        let strings: Vec<Vec<u8>> = vec![
            b"Model=A".to_vec(),
            b"flag".to_vec(),
            b"=skipped".to_vec(),
            b"".to_vec(),
            b"k=v=w".to_vec(),
            b"MODEL=B".to_vec(),
            b"nbsp=a\xc2\xa0b\x00\xa0c".to_vec(),
            b"bad=\xff".to_vec(),
            b"\xffkey=1".to_vec(),
        ];
        assert_eq!(
            decode_txt(&strings),
            props(&[
                ("model", "B"),
                ("flag", ""),
                ("k", "v=w"),
                ("nbsp", "a b c"),
                ("bad", "\u{FFFD}")
            ])
        );
        assert_eq!(
            read_txt(b"\x01a\x02bc"),
            Some(vec![b"a".to_vec(), b"bc".to_vec()])
        );
        assert_eq!(read_txt(b"\x05ab"), None);
        assert_eq!(read_txt(b""), Some(Vec::new()));
    }

    #[test]
    fn split_names_like_pyatv() {
        assert_eq!(
            split_service_name("70-35-60-63.1 Living Room._sleep-proxy._udp.local"),
            Some((
                "70-35-60-63.1 Living Room".to_string(),
                "_sleep-proxy._udp.local".to_string()
            ))
        );
        assert_eq!(
            split_service_name("_raop._TCP.local"),
            Some((String::new(), "_raop._TCP.local".to_string()))
        );
        assert_eq!(
            split_service_name("x._raop._tcp"),
            Some(("x".into(), "_raop._tcp.".into()))
        );
        assert_eq!(split_service_name("Living-Room.local"), None);
        assert_eq!(split_service_name("local"), None);
    }

    #[test]
    fn placeholder_for_ptr_without_records() {
        let mut out = vec![0x35, 0xFF, 0x84, 0x00, 0, 0, 0, 2, 0, 0, 0, 0];
        // PTR _raop._tcp.local -> "AA@Den._raop._tcp.local" (compressed), twice.
        for _ in 0..2 {
            out.extend_from_slice(b"\x05_raop\x04_tcp\x05local\x00");
            out.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 10, 0, 9]);
            out.extend_from_slice(b"\x06AA@Den\xc0\x0c");
        }
        let message = DnsMessage::parse(&out).expect("parses");
        let services = parse_services(&[message]);
        assert_eq!(
            services,
            vec![Service {
                service_type: "_raop._tcp.local".to_string(),
                name: "AA@Den".to_string(),
                address: None,
                addresses: Vec::new(),
                port: 0,
                properties: Vec::new(),
            }]
        );
        assert_eq!(receiver_info(&services, Ipv4Addr::LOCALHOST), None);
    }

    #[test]
    fn link_local_addresses_are_skipped() {
        let mut out = vec![0x35, 0xFF, 0x84, 0x00, 0, 0, 0, 3, 0, 0, 0, 0];
        out.extend_from_slice(b"\x06AA@Den\x05_raop\x04_tcp\x05local\x00"); // offset 12
        out.extend_from_slice(&[0, 33, 0x80, 1, 0, 0, 0, 10, 0, 12, 0, 0, 0, 0, 0x1B, 0x58]);
        out.extend_from_slice(b"\x03Den\xc0\x1e"); // target "Den.local" (offset 30 = "local")
        let host = out.len() - 6;
        for octets in [[169, 254, 1, 1], [192, 168, 1, 9]] {
            out.extend_from_slice(&[0xC0 | (host >> 8) as u8, host as u8]);
            out.extend_from_slice(&[0, 1, 0x80, 1, 0, 0, 0, 10, 0, 4]);
            out.extend_from_slice(&octets);
        }
        let message = DnsMessage::parse(&out).expect("parses");
        assert_eq!(message.answers[1].name, "Den.local");
        let services = parse_services(&[message]);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].address, Some(Ipv4Addr::new(192, 168, 1, 9)));
        assert_eq!(services[0].addresses, vec![Ipv4Addr::new(192, 168, 1, 9)]);
        assert_eq!(services[0].port, 7000);
    }

    #[test]
    fn malformed_packets_are_rejected() {
        assert_eq!(DnsMessage::parse(&[]), Err(DnsError::Truncated));
        assert_eq!(DnsMessage::parse(&[0; 11]), Err(DnsError::Truncated));
        // Header promises a question that is not there.
        assert_eq!(
            DnsMessage::parse(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]),
            Err(DnsError::Truncated)
        );
        // Pointer to itself.
        let looped = packet_with_record(&[0xC0, 12], TYPE_A, &[1, 2, 3, 4]);
        assert_eq!(DnsMessage::parse(&looped), Err(DnsError::PointerLoop));
        // Pointer past the end.
        let past = packet_with_record(&[0xC0, 0xFF], TYPE_A, &[1, 2, 3, 4]);
        assert_eq!(DnsMessage::parse(&past), Err(DnsError::Truncated));
        // Reserved label type.
        let reserved = packet_with_record(&[0x40, 0], TYPE_A, &[1, 2, 3, 4]);
        assert_eq!(
            DnsMessage::parse(&reserved),
            Err(DnsError::BadLabelType(0x40))
        );
        // Name over 255 bytes.
        let mut long = Vec::new();
        for _ in 0..5 {
            long.push(63);
            long.extend_from_slice(&[b'a'; 63]);
        }
        long.push(0);
        let long = packet_with_record(&long, TYPE_A, &[1, 2, 3, 4]);
        assert_eq!(DnsMessage::parse(&long), Err(DnsError::NameTooLong));
        // Non-UTF-8 label.
        let bad = packet_with_record(&[1, 0xFF, 0], TYPE_A, &[1, 2, 3, 4]);
        assert_eq!(DnsMessage::parse(&bad), Err(DnsError::BadUtf8));
        // rdlength past the end.
        let mut short = packet_with_record(&[0], TYPE_A, &[1, 2, 3, 4]);
        short.truncate(short.len() - 1);
        assert_eq!(DnsMessage::parse(&short), Err(DnsError::Truncated));
    }

    #[test]
    fn malformed_record_data_is_skipped() {
        let a = packet_with_record(&[0], TYPE_A, &[1, 2, 3]);
        let message = DnsMessage::parse(&a).expect("parses");
        assert_eq!(message.answers[0].data, RecordData::Other);
        let srv = packet_with_record(&[0], TYPE_SRV, &[0, 0, 0, 0, 0x1B, 0x58]);
        let message = DnsMessage::parse(&srv).expect("parses");
        assert_eq!(message.answers[0].data, RecordData::Other);
        let txt = packet_with_record(&[0], TYPE_TXT, &[9, b'a']);
        let message = DnsMessage::parse(&txt).expect("parses");
        assert_eq!(message.answers[0].data, RecordData::Other);
        let other = packet_with_record(&[0], 47, &[0xC0]);
        let message = DnsMessage::parse(&other).expect("parses");
        assert_eq!(message.answers[0].rtype, 47);
        assert_eq!(message.answers[0].data, RecordData::Other);
    }

    #[test]
    fn authority_records_are_parsed_but_not_used() {
        let mut out = vec![0x35, 0xFF, 0x84, 0x00, 0, 0, 0, 0, 0, 1, 0, 0];
        out.extend_from_slice(b"\x06AA@Den\x05_raop\x04_tcp\x05local\x00");
        out.extend_from_slice(&[
            0, 33, 0, 1, 0, 0, 0, 10, 0, 8, 0, 0, 0, 0, 0x1B, 0x58, 0xC0, 0x13,
        ]);
        let message = DnsMessage::parse(&out).expect("parses");
        assert_eq!(message.authorities.len(), 1);
        assert!(matches!(
            &message.authorities[0].data,
            RecordData::Srv { port: 7000, target, .. } if target == "_raop._tcp.local"
        ));
        assert!(parse_services(&[message]).is_empty());
    }

    #[test]
    fn decode_value_replacements() {
        assert_eq!(decode_value(b"Living\xc2\xa0Room"), "Living Room");
        assert_eq!(decode_value(b"a\x00\xa0b"), "a b");
        assert_eq!(decode_value(b"\xc2\xa0\xc2\xa0"), "  ");
        assert_eq!(decode_value(b""), "");
    }
}
