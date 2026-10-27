//! Network scan for AirPlay receivers (workstream: discovery). CONTRACT.
//!
//! Multicast DNS-SD browse with `mdns-sd` for `_airplay._tcp.local.` and
//! `_raop._tcp.local.` for the given time, merged by IPv4 address into
//! [`ScanResult`]s (name from the RAOP instance after '@' or the AirPlay instance,
//! model from TXT `am`/`model`), sorted by name. The daemon is shut down afterwards.
//! Only receivers advertising RAOP are returned.
//!
//! This is the port of WinPods v0.1's `scan_network` (pyatv's zeroconf scanner):
//! services are grouped by every usable IPv4 address they resolve to (link-local,
//! loopback and unspecified addresses are skipped, as pyatv does), and the result is
//! sorted case-insensitively by name. The model is the AirPlay `model`, else the RAOP
//! `am` (the first one pyatv's device info picks up for Apple receivers), else empty.

use std::collections::BTreeMap;
use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::types::ScanResult;

/// DNS-SD type of the AirPlay service.
pub const AIRPLAY_SERVICE_TYPE: &str = "_airplay._tcp.local.";
/// DNS-SD type of the RAOP (AirPlay audio) service.
pub const RAOP_SERVICE_TYPE: &str = "_raop._tcp.local.";

/// How long to wait for the daemon to confirm its shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound for the scan time, so deadline arithmetic cannot overflow.
const MAX_SCAN: Duration = Duration::from_secs(3600);

/// Browse the local network for `duration` and return the AirPlay receivers that
/// advertise RAOP, sorted by name.
///
/// Errors only when the mDNS daemon cannot be started or cannot browse; a network
/// where multicast is blocked simply yields an empty list.
pub async fn scan(duration: Duration) -> std::io::Result<Vec<ScanResult>> {
    let daemon = ServiceDaemon::new().map_err(io::Error::other)?;
    let found = browse(&daemon, duration.min(MAX_SCAN)).await;
    shutdown(&daemon).await;
    let results = merge(found?.into_values());
    debug!(receivers = results.len(), "mDNS scan finished");
    Ok(results)
}

/// Collect resolved AirPlay and RAOP services until `duration` has passed, keyed by
/// lowercased full name (a later resolution replaces an earlier one).
async fn browse(
    daemon: &ServiceDaemon,
    duration: Duration,
) -> io::Result<BTreeMap<String, Discovered>> {
    let airplay = daemon
        .browse(AIRPLAY_SERVICE_TYPE)
        .map_err(io::Error::other)?;
    let raop = daemon.browse(RAOP_SERVICE_TYPE).map_err(io::Error::other)?;

    let mut found = BTreeMap::new();
    let deadline = tokio::time::sleep_until(Instant::now() + duration);
    tokio::pin!(deadline);
    // Keep one pending receive per channel across loop iterations, so no event is
    // lost to a cancelled receive.
    let mut airplay_next = airplay.recv_async();
    let mut raop_next = raop.recv_async();
    let (mut airplay_open, mut raop_open) = (true, true);

    while airplay_open || raop_open {
        tokio::select! {
            () = &mut deadline => break,
            event = &mut airplay_next, if airplay_open => {
                airplay_next = airplay.recv_async();
                match event {
                    Ok(event) => handle_event(event, &mut found),
                    Err(_) => airplay_open = false,
                }
            }
            event = &mut raop_next, if raop_open => {
                raop_next = raop.recv_async();
                match event {
                    Ok(event) => handle_event(event, &mut found),
                    Err(_) => raop_open = false,
                }
            }
        }
    }
    if !(airplay_open && raop_open) {
        warn!("mDNS daemon stopped before the scan ended");
    }
    Ok(found)
}

fn handle_event(event: ServiceEvent, found: &mut BTreeMap<String, Discovered>) {
    match event {
        ServiceEvent::ServiceResolved(service) => {
            if let Some(discovered) = Discovered::from_resolved(&service) {
                debug!(name = %service.fullname, "resolved");
                found.insert(service.fullname.to_ascii_lowercase(), discovered);
            }
        }
        ServiceEvent::ServiceRemoved(_, fullname) => {
            found.remove(&fullname.to_ascii_lowercase());
        }
        _ => {}
    }
}

/// Stop the daemon thread and wait (briefly) until it has closed its sockets.
async fn shutdown(daemon: &ServiceDaemon) {
    match daemon.shutdown() {
        Ok(status) => {
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, status.recv_async())
                .await
                .is_err()
            {
                warn!("mDNS daemon did not confirm its shutdown");
            }
        }
        Err(err) => debug!("mDNS daemon shutdown: {err}"),
    }
}

/// Which of the two browsed services a resolution belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    AirPlay,
    Raop,
}

/// The parts of a resolved service the scan uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Discovered {
    kind: Kind,
    /// Instance name (full name without the service type).
    instance: String,
    /// Usable IPv4 addresses, sorted.
    addresses: Vec<Ipv4Addr>,
    /// TXT `model` (AirPlay) or `am` (RAOP), when present and not empty.
    model: Option<String>,
}

impl Discovered {
    fn from_resolved(service: &ResolvedService) -> Option<Self> {
        let kind = if service.ty_domain.eq_ignore_ascii_case(AIRPLAY_SERVICE_TYPE) {
            Kind::AirPlay
        } else if service.ty_domain.eq_ignore_ascii_case(RAOP_SERVICE_TYPE) {
            Kind::Raop
        } else {
            return None;
        };
        let mut addresses: Vec<Ipv4Addr> = service
            .get_addresses_v4()
            .into_iter()
            .filter(|address| usable(*address))
            .collect();
        addresses.sort_unstable();
        let keys = match kind {
            Kind::AirPlay => ["model", "am"],
            Kind::Raop => ["am", "model"],
        };
        let model = keys
            .into_iter()
            .filter_map(|key| service.get_property_val_str(key))
            .find(|value| !value.is_empty())
            .map(str::to_string);
        Some(Self {
            kind,
            instance: instance_name(&service.fullname, &service.ty_domain).to_string(),
            addresses,
            model,
        })
    }
}

/// pyatv skips link-local, loopback and unspecified addresses.
fn usable(address: Ipv4Addr) -> bool {
    !(address.is_link_local() || address.is_loopback() || address.is_unspecified())
}

/// `fullname` without the trailing `.<ty_domain>`; the whole name if it does not end
/// with the type.
fn instance_name<'a>(fullname: &'a str, ty_domain: &str) -> &'a str {
    fullname
        .len()
        .checked_sub(ty_domain.len() + 1)
        .and_then(|cut| {
            let (instance, suffix) = (fullname.get(..cut)?, fullname.get(cut..)?);
            let matches = suffix.strip_prefix('.')?.eq_ignore_ascii_case(ty_domain);
            matches.then_some(instance)
        })
        .unwrap_or(fullname)
}

/// The services seen at one IPv4 address.
#[derive(Default)]
struct Device {
    raop: Option<(String, Option<String>)>,
    airplay: Option<(String, Option<String>)>,
}

/// Merge discovered services by IPv4 address into scan results: only addresses with a
/// RAOP service, named after the RAOP instance (part after '@') or else the AirPlay
/// instance, sorted case-insensitively by name, then address.
fn merge(discovered: impl IntoIterator<Item = Discovered>) -> Vec<ScanResult> {
    let mut devices: BTreeMap<Ipv4Addr, Device> = BTreeMap::new();
    for service in discovered {
        for address in &service.addresses {
            let device = devices.entry(*address).or_default();
            let slot = match service.kind {
                Kind::AirPlay => &mut device.airplay,
                Kind::Raop => &mut device.raop,
            };
            slot.get_or_insert_with(|| (service.instance.clone(), service.model.clone()));
        }
    }

    let mut results: Vec<(Ipv4Addr, ScanResult)> = devices
        .into_iter()
        .filter_map(|(address, device)| {
            let (raop_name, raop_model) = device.raop?;
            let (airplay_name, airplay_model) = device.airplay.unzip();
            let name = match raop_name.split_once('@') {
                Some((_, name)) => name.to_string(),
                None => airplay_name.unwrap_or(raop_name),
            };
            let model = airplay_model.flatten().or(raop_model).unwrap_or_default();
            let result = ScanResult {
                name,
                address: address.to_string(),
                model,
            };
            Some((address, result))
        })
        .collect();
    results.sort_by_cached_key(|(address, result)| (result.name.to_lowercase(), *address));
    results.into_iter().map(|(_, result)| result).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdns_sd::ServiceInfo;

    fn discovered(
        kind: Kind,
        instance: &str,
        addresses: &[[u8; 4]],
        model: Option<&str>,
    ) -> Discovered {
        Discovered {
            kind,
            instance: instance.to_string(),
            addresses: addresses.iter().map(|&a| Ipv4Addr::from(a)).collect(),
            model: model.map(str::to_string),
        }
    }

    fn result(name: &str, address: &str, model: &str) -> ScanResult {
        ScanResult {
            name: name.to_string(),
            address: address.to_string(),
            model: model.to_string(),
        }
    }

    fn resolved(ty: &str, instance: &str, ips: &str, props: &[(&str, &str)]) -> ResolvedService {
        ServiceInfo::new(ty, instance, "host.local.", ips, 7000, props)
            .expect("valid service info")
            .as_resolved_service()
    }

    #[test]
    fn merges_by_address_and_keeps_only_raop() {
        let services = vec![
            discovered(
                Kind::AirPlay,
                "Living Room",
                &[[192, 168, 1, 155]],
                Some("AudioAccessory5,1"),
            ),
            discovered(
                Kind::Raop,
                "02AABBCCDDEE@Living Room",
                &[[192, 168, 1, 155]],
                Some("AudioAccessory5,1"),
            ),
            // AirPlay only (e.g. a video-only receiver): dropped.
            discovered(Kind::AirPlay, "Mac", &[[192, 168, 1, 20]], Some("Mac14,2")),
            discovered(
                Kind::Raop,
                "AABB@bedroom",
                &[[192, 168, 1, 40]],
                Some("AudioAccessory1,1"),
            ),
        ];
        assert_eq!(
            merge(services),
            vec![
                result("bedroom", "192.168.1.40", "AudioAccessory1,1"),
                result("Living Room", "192.168.1.155", "AudioAccessory5,1"),
            ]
        );
    }

    #[test]
    fn name_and_model_fallbacks() {
        let services = vec![
            // RAOP name without '@' falls back to the AirPlay instance.
            discovered(
                Kind::Raop,
                "Kitchen Speaker",
                &[[10, 0, 0, 2]],
                Some("am-model"),
            ),
            discovered(Kind::AirPlay, "Kitchen", &[[10, 0, 0, 2]], None),
            // No AirPlay service: the RAOP instance as is, RAOP model.
            discovered(Kind::Raop, "Den", &[[10, 0, 0, 3]], Some("AirPort10,115")),
            // No model anywhere.
            discovered(Kind::Raop, "AA@Attic", &[[10, 0, 0, 4]], None),
            // AirPlay `model` wins over RAOP `am`.
            discovered(Kind::Raop, "BB@Office", &[[10, 0, 0, 5]], Some("raop")),
            discovered(Kind::AirPlay, "Office", &[[10, 0, 0, 5]], Some("airplay")),
        ];
        assert_eq!(
            merge(services),
            vec![
                result("Attic", "10.0.0.4", ""),
                result("Den", "10.0.0.3", "AirPort10,115"),
                result("Kitchen", "10.0.0.2", "am-model"),
                result("Office", "10.0.0.5", "airplay"),
            ]
        );
    }

    #[test]
    fn multi_homed_devices_appear_per_address() {
        let services = vec![
            discovered(
                Kind::Raop,
                "AA@Lounge",
                &[[10, 0, 0, 9], [192, 168, 1, 9]],
                None,
            ),
            discovered(
                Kind::AirPlay,
                "Lounge",
                &[[192, 168, 1, 9]],
                Some("AppleTV11,1"),
            ),
            discovered(Kind::Raop, "BB@Lounge", &[[10, 0, 0, 1]], None),
        ];
        assert_eq!(
            merge(services),
            vec![
                result("Lounge", "10.0.0.1", ""),
                result("Lounge", "10.0.0.9", ""),
                result("Lounge", "192.168.1.9", "AppleTV11,1"),
            ]
        );
    }

    #[test]
    fn first_service_per_address_wins() {
        let services = vec![
            discovered(Kind::Raop, "AA@First", &[[10, 0, 0, 2]], None),
            discovered(Kind::Raop, "BB@Second", &[[10, 0, 0, 2]], None),
        ];
        assert_eq!(merge(services), vec![result("First", "10.0.0.2", "")]);
        assert!(merge(Vec::new()).is_empty());
        // A service without usable addresses contributes nothing.
        assert!(merge(vec![discovered(Kind::Raop, "AA@Nowhere", &[], None)]).is_empty());
    }

    #[test]
    fn from_resolved_service() {
        let raop = resolved(
            RAOP_SERVICE_TYPE,
            "02AABBCCDDEE@Living Room",
            "192.168.1.155,169.254.10.1,fe80::1",
            &[("am", "AudioAccessory5,1"), ("ft", "0x4A7FCA00,0x3C354BD0")],
        );
        assert_eq!(
            Discovered::from_resolved(&raop),
            Some(discovered(
                Kind::Raop,
                "02AABBCCDDEE@Living Room",
                &[[192, 168, 1, 155]],
                Some("AudioAccessory5,1")
            ))
        );
        let airplay = resolved(
            AIRPLAY_SERVICE_TYPE,
            "Living Room",
            "192.168.1.155,10.0.0.7",
            &[("MODEL", "AudioAccessory5,1")],
        );
        assert_eq!(
            Discovered::from_resolved(&airplay),
            Some(discovered(
                Kind::AirPlay,
                "Living Room",
                &[[10, 0, 0, 7], [192, 168, 1, 155]],
                Some("AudioAccessory5,1")
            ))
        );
        let empty_model = resolved(
            AIRPLAY_SERVICE_TYPE,
            "Box",
            "10.0.0.8",
            &[("model", ""), ("am", "X1,1")],
        );
        assert_eq!(
            Discovered::from_resolved(&empty_model).and_then(|d| d.model),
            Some("X1,1".to_string())
        );
        let other = resolved("_http._tcp.local.", "Web", "10.0.0.9", &[]);
        assert_eq!(Discovered::from_resolved(&other), None);
    }

    #[test]
    fn instance_names() {
        assert_eq!(
            instance_name("Living Room._airplay._tcp.local.", AIRPLAY_SERVICE_TYPE),
            "Living Room"
        );
        assert_eq!(
            instance_name("A.B._RAOP._tcp.local.", RAOP_SERVICE_TYPE),
            "A.B"
        );
        assert_eq!(instance_name("odd", RAOP_SERVICE_TYPE), "odd");
        assert_eq!(
            instance_name("x_raop._tcp.local.", RAOP_SERVICE_TYPE),
            "x_raop._tcp.local."
        );
    }

    #[test]
    fn address_filter() {
        assert!(usable(Ipv4Addr::new(192, 168, 1, 155)));
        assert!(!usable(Ipv4Addr::new(169, 254, 3, 4)));
        assert!(!usable(Ipv4Addr::LOCALHOST));
        assert!(!usable(Ipv4Addr::UNSPECIFIED));
    }

    /// One real scan of the local network, run by hand only:
    /// `cargo test -p winpods-engine scan::tests::real_scan -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "browses the real network with multicast DNS"]
    async fn real_scan() {
        let started = std::time::Instant::now();
        let found = scan(Duration::from_secs(5)).await.expect("scan runs");
        println!(
            "scan took {:?}, {} receiver(s)",
            started.elapsed(),
            found.len()
        );
        for receiver in &found {
            println!(
                "  {:<16} {}  [{}]",
                receiver.address, receiver.name, receiver.model
            );
        }
    }
}
