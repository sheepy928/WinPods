//! Network scan for AirPlay receivers (workstream: discovery). CONTRACT.
//!
//! Multicast DNS-SD browse with `mdns-sd` for `_airplay._tcp.local.` and
//! `_raop._tcp.local.` for the given time, merged by IPv4 address into
//! [`ScanResult`]s (name from the RAOP instance after '@' or the AirPlay instance,
//! model from TXT `am`/`model`), sorted by name. The daemon is shut down afterwards.
//! Only receivers advertising RAOP are returned.

use std::time::Duration;

use crate::types::ScanResult;

pub async fn scan(duration: Duration) -> std::io::Result<Vec<ScanResult>> {
    let _ = duration;
    todo!("scan: scan")
}
