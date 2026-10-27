//! Unicast mDNS lookup of one receiver (workstream: discovery). CONTRACT: `lookup`
//! is used by the engine.
//!
//! Port of pyatv's unicast scan (`pyatv/core/mdns.py`, `create_service_queries`,
//! `UnicastDnsSdClientProtocol`): DNS id 0x35FF, PTR questions with class 0x8001
//! (unicast-response bit) for the same service list pyatv uses, `_sleep-proxy._udp`
//! appended to each message, sent from an ephemeral UDP port to `<ip>:5353`, resent
//! every second until each query has a response or the timeout passes. Responses
//! are parsed (with name compression) from the answer and additional sections into
//! PTR/SRV/TXT/A records and turned into a [`ReceiverInfo`]. No TCP port knocking.
//! Fixture: `tests/fixtures/mdns.json` (real, sanitized HomePod responses).

use std::net::Ipv4Addr;
use std::time::Duration;

use crate::types::ReceiverInfo;

/// Look up the AirPlay receiver at `address`. `Ok(None)` = it answered nothing usable.
pub async fn lookup(address: Ipv4Addr, timeout: Duration) -> std::io::Result<Option<ReceiverInfo>> {
    let _ = (address, timeout);
    todo!("mdns: lookup")
}
