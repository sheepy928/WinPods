//! SETUP / SET_PARAMETER / metadata bodies and response parsing
//! (workstream: control plane).
//!
//! Base and stream SETUP plists exactly as `legacy/python/winpods/raop_live.py`
//! builds them (see the plan appendix and `tests/fixtures/setup_bodies.json`),
//! `latencyMin = min(L, 11025)`, `latencyMax = max(L, 88200)` with
//! `L = crate::latency_frames(ms)`; volume `-30 + 0.3·pct` dB (−144 at 0 %)
//! formatted like Python `str(float)`; DMAP `mlit{minm, asal, asar}` metadata.
//! Fixtures: `setup_bodies.json`, `volume.json`.
//!
//! One deliberate deviation: pyatv's DMAP `string_tag` writes the *character*
//! count as the length, which is wrong for non-ASCII names; [`dmap_metadata`] writes
//! the UTF-8 byte count (identical for ASCII, as in the fixture).

use std::net::Ipv4Addr;

use plist::{Dictionary, Value};

use crate::types::{Codec, FRAMES_PER_PACKET, SAMPLE_RATE, latency_frames};

/// Lower clamp of `latencyMin` (0.25 s).
pub const LATENCY_MIN_CAP: u32 = 11_025;
/// Lower bound of `latencyMax` (2 s).
pub const LATENCY_MAX_FLOOR: u32 = 88_200;
/// Stream type of a realtime audio stream.
pub const STREAM_TYPE_REALTIME: u64 = 0x60;
/// Volume sent for 0 % (AirPlay's "muted").
pub const MUTED_DB: f64 = -144.0;

/// Malformed or incomplete SETUP response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BodyError {
    #[error("SETUP response is missing {0}")]
    Missing(&'static str),
    #[error("SETUP response has an invalid {0}")]
    Invalid(&'static str),
}

/// `(latencyMin, latencyMax)` in frames for a latency in milliseconds.
pub fn latency_bounds(latency_ms: u32) -> (u32, u32) {
    let frames = latency_frames(latency_ms);
    (frames.min(LATENCY_MIN_CAP), frames.max(LATENCY_MAX_FLOOR))
}

/// Uppercase `uuid5(NAMESPACE_DNS, "winpods-<device id>")`: our PTP timing peer ID.
pub fn timing_peer_id(device_id: &str) -> String {
    let name = format!("winpods-{device_id}");
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes())
        .to_string()
        .to_uppercase()
}

/// Ports the receiver must use for our PTP messages when we could not bind 319/320.
#[derive(Debug, Clone, Copy)]
pub struct ClockPorts<'a> {
    /// The receiver's gid/pi/psi values (`ReceiverInfo::receiver_ids`).
    pub receiver_ids: &'a [String],
    /// Our PTP port.
    pub port: u16,
}

/// Timing part of the base SETUP.
#[derive(Debug, Clone, Copy)]
pub enum BaseTiming<'a> {
    /// `timingProtocol = PTP` with `timingPeerInfo`; `clock_ports` only off port 319.
    Ptp {
        local_ip: Ipv4Addr,
        clock_ports: Option<ClockPorts<'a>>,
    },
    /// `timingProtocol = NTP` with our timing responder's port.
    Ntp { timing_port: u16 },
}

/// Inputs of the base SETUP body.
#[derive(Debug, Clone, Copy)]
pub struct BaseSetup<'a> {
    /// Stored MAC, used as `deviceID` and `macAddress`.
    pub device_id: &'a str,
    /// Uppercase random UUID.
    pub session_uuid: &'a str,
    pub sender_name: &'a str,
    pub timing: BaseTiming<'a>,
}

fn int(value: impl Into<plist::Integer>) -> Value {
    Value::Integer(value.into())
}

fn string(value: &str) -> Value {
    Value::String(value.to_string())
}

/// `timingPeerInfo` for PTP: our address, our stable peer ID, and `ClockPorts`
/// (every receiver ID → our port) only when we are not on the standard ports.
pub fn timing_peer_info(
    local_ip: Ipv4Addr,
    device_id: &str,
    clock_ports: Option<ClockPorts<'_>>,
) -> Value {
    let mut peer = Dictionary::new();
    peer.insert(
        "Addresses".into(),
        Value::Array(vec![string(&local_ip.to_string())]),
    );
    peer.insert("ID".into(), string(&timing_peer_id(device_id)));
    peer.insert(
        "SupportsClockPortMatchingOverride".into(),
        Value::Boolean(true),
    );
    if let Some(ports) = clock_ports {
        let mut map = Dictionary::new();
        for id in ports.receiver_ids {
            map.insert(id.clone(), int(ports.port));
        }
        peer.insert("ClockPorts".into(), Value::Dictionary(map));
    }
    Value::Dictionary(peer)
}

/// The base SETUP body (`LiveAirPlayV2._setup_base`).
pub fn base_setup_body(setup: &BaseSetup<'_>) -> Value {
    let mut body = Dictionary::new();
    let protocol = match setup.timing {
        BaseTiming::Ptp { .. } => "PTP",
        BaseTiming::Ntp { .. } => "NTP",
    };
    body.insert("deviceID".into(), string(setup.device_id));
    body.insert("sessionUUID".into(), string(setup.session_uuid));
    body.insert("timingProtocol".into(), string(protocol));
    body.insert("isMultiSelectAirPlay".into(), Value::Boolean(true));
    body.insert("groupContainsGroupLeader".into(), Value::Boolean(false));
    body.insert("macAddress".into(), string(setup.device_id));
    body.insert("model".into(), string("iPhone14,3"));
    body.insert("name".into(), string(setup.sender_name));
    body.insert("osBuildVersion".into(), string("20F66"));
    body.insert("osName".into(), string("iPhone OS"));
    body.insert("osVersion".into(), string("16.5"));
    body.insert("senderSupportsRelay".into(), Value::Boolean(false));
    body.insert("sourceVersion".into(), string("690.7.1"));
    body.insert("statsCollectionEnabled".into(), Value::Boolean(false));
    match setup.timing {
        BaseTiming::Ptp {
            local_ip,
            clock_ports,
        } => {
            body.insert(
                "timingPeerInfo".into(),
                timing_peer_info(local_ip, setup.device_id, clock_ports),
            );
        }
        BaseTiming::Ntp { timing_port } => {
            body.insert("timingPort".into(), int(timing_port));
        }
    }
    Value::Dictionary(body)
}

/// The stream SETUP body (`LiveAirPlayV2.setup_audio_stream`).
pub fn stream_setup_body(
    codec: Codec,
    latency_ms: u32,
    control_port: u16,
    shk: &[u8; 32],
    stream_connection_id: u32,
) -> Value {
    let (latency_min, latency_max) = latency_bounds(latency_ms);
    let mut stream = Dictionary::new();
    stream.insert("audioFormat".into(), int(codec.audio_format()));
    stream.insert("audioMode".into(), string("default"));
    stream.insert("controlPort".into(), int(control_port));
    stream.insert("ct".into(), int(codec.compression_type()));
    stream.insert("isMedia".into(), Value::Boolean(true));
    stream.insert("latencyMax".into(), int(latency_max));
    stream.insert("latencyMin".into(), int(latency_min));
    stream.insert("shk".into(), Value::Data(shk.to_vec()));
    stream.insert("spf".into(), int(FRAMES_PER_PACKET as u64));
    stream.insert("sr".into(), int(SAMPLE_RATE));
    stream.insert("type".into(), int(STREAM_TYPE_REALTIME));
    stream.insert("supportsDynamicStreamID".into(), Value::Boolean(false));
    stream.insert("streamConnectionID".into(), int(stream_connection_id));
    let mut body = Dictionary::new();
    body.insert(
        "streams".into(),
        Value::Array(vec![Value::Dictionary(stream)]),
    );
    Value::Dictionary(body)
}

/// What the session reads from the base SETUP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseSetupResponse {
    /// `eventPort` (0 when absent, like pyatv's `resp.get("eventPort", 0)`).
    pub event_port: u16,
    /// `timingPeerInfo.ClockID & 0xFFFF_FFFF_FFFF_FFFF` (signed values wrap).
    pub clock_id: Option<u64>,
    /// Truthiness of `skipRecord`.
    pub skip_record: bool,
}

/// What the session reads from the stream SETUP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSetupResponse {
    pub data_port: u16,
    pub control_port: u16,
    /// `arrivalToRenderLatencyMs`; absent or 0 = `None`.
    pub render_ms: Option<u32>,
}

/// Integer as u64, accepting negative i64 values as their two's complement.
fn as_u64_wrapping(value: &Value) -> Option<u64> {
    value
        .as_unsigned_integer()
        .or_else(|| value.as_signed_integer().map(|v| v as u64))
}

fn port(value: Option<&Value>, name: &'static str) -> Result<u16, BodyError> {
    let value = value.ok_or(BodyError::Missing(name))?;
    value
        .as_unsigned_integer()
        .and_then(|v| u16::try_from(v).ok())
        .ok_or(BodyError::Invalid(name))
}

/// Python truthiness of a plist value (`bool(resp.get(...))`).
fn truthy(value: &Value) -> bool {
    match value {
        Value::Boolean(b) => *b,
        Value::Integer(i) => i.as_signed() != Some(0),
        Value::Real(r) => *r != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Data(d) => !d.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Dictionary(d) => !d.is_empty(),
        _ => true,
    }
}

/// Parse the base SETUP response body.
pub fn parse_base_setup_response(value: &Value) -> Result<BaseSetupResponse, BodyError> {
    let dict = value
        .as_dictionary()
        .ok_or(BodyError::Invalid("body (not a dictionary)"))?;
    let event_port = match dict.get("eventPort") {
        Some(v) => port(Some(v), "eventPort")?,
        None => 0,
    };
    let clock_id = dict
        .get("timingPeerInfo")
        .and_then(Value::as_dictionary)
        .and_then(|peer| peer.get("ClockID"))
        .and_then(as_u64_wrapping);
    let skip_record = dict.get("skipRecord").is_some_and(truthy);
    Ok(BaseSetupResponse {
        event_port,
        clock_id,
        skip_record,
    })
}

/// Parse the stream SETUP response body (`streams[0]`).
pub fn parse_stream_setup_response(value: &Value) -> Result<StreamSetupResponse, BodyError> {
    let stream = value
        .as_dictionary()
        .and_then(|d| d.get("streams"))
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())
        .and_then(Value::as_dictionary)
        .ok_or(BodyError::Missing("streams[0]"))?;
    let render_ms = match stream.get("arrivalToRenderLatencyMs") {
        Some(Value::Real(r)) if r.is_finite() && *r >= 1.0 => {
            Some(r.trunc().min(f64::from(u32::MAX)) as u32)
        }
        Some(v) => v
            .as_unsigned_integer()
            .filter(|ms| *ms > 0)
            .map(|ms| u32::try_from(ms).unwrap_or(u32::MAX)),
        None => None,
    };
    Ok(StreamSetupResponse {
        data_port: port(stream.get("dataPort"), "dataPort")?,
        control_port: port(stream.get("controlPort"), "controlPort")?,
        render_ms,
    })
}

/// Clamp a volume percentage to 0..=100 (NaN = 0, i.e. muted).
pub fn clamp_pct(pct: f64) -> f64 {
    if pct.is_nan() {
        0.0
    } else {
        pct.clamp(0.0, 100.0)
    }
}

/// The percentage as the user wrote it: an `f32` slider/config value converted
/// through its shortest decimal form, so 33.3 stays 33.3 (not 33.29999923706055).
pub fn pct_from_f32(pct: f32) -> f64 {
    format!("{pct}").parse().unwrap_or(f64::from(pct))
}

/// pyatv `pct_to_dbfs`: −144 for exactly 0 %, else a linear map of 0..100 onto
/// −30..0 dB with Python's operation order. `pct` must already be clamped.
pub fn pct_to_dbfs(pct: f64) -> f64 {
    if pct == 0.0 {
        return MUTED_DB;
    }
    // map_range(level, 0.0, 100.0, -30.0, 0.0)
    (pct - 0.0) * (0.0 - -30.0) / (100.0 - 0.0) + -30.0
}

/// Python's `repr(float)` / `str(float)` for any `f64`: shortest round-trip digits,
/// fixed notation for decimal exponents in [-4, 16), otherwise `d.ddde±XX`.
pub fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.into();
    }
    // `{:e}` gives the shortest round-trip digits, e.g. "-2.0009999999999998e1".
    let sci = format!("{value:e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;
    // Position of the decimal point relative to the digit string.
    let decpt = exponent + 1;
    let body = if -4 < decpt && decpt <= 16 {
        if decpt <= 0 {
            format!("0.{}{digits}", "0".repeat((-decpt) as usize))
        } else if decpt >= n {
            format!("{digits}{}.0", "0".repeat((decpt - n) as usize))
        } else {
            let (int_part, frac) = digits.split_at(decpt as usize);
            format!("{int_part}.{frac}")
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let frac = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{first}{frac}e{exp_sign}{:02}", exponent.unsigned_abs())
    };
    format!("{sign}{body}")
}

/// The value of `SET_PARAMETER volume: <value>` for a percentage (clamped first).
pub fn volume_value(pct: f64) -> String {
    python_float_repr(pct_to_dbfs(clamp_pct(pct)))
}

/// One DMAP tag: 4-byte code, u32 big-endian length, payload.
fn dmap_tag(code: &[u8; 4], payload: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(code);
    // Metadata strings are tiny; saturate rather than wrap on absurd input.
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(payload);
}

/// DMAP now-playing metadata `mlit{minm title, asal album, asar artist}`; empty
/// fields are left out like pyatv does.
pub fn dmap_metadata(title: &str, album: &str, artist: &str) -> Vec<u8> {
    let mut items = Vec::new();
    for (code, value) in [(b"minm", title), (b"asal", album), (b"asar", artist)] {
        if !value.is_empty() {
            dmap_tag(code, value.as_bytes(), &mut items);
        }
    }
    let mut out = Vec::with_capacity(items.len() + 8);
    dmap_tag(b"mlit", &items, &mut out);
    out
}

/// Render a plist value like Python's `repr` of the decoded object (for the
/// feedback diagnostics line), e.g. `{'streams': [{'sr': 44100.0, 'type': 96}]}`.
pub fn describe_plist(value: &Value) -> String {
    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
    match value {
        Value::Dictionary(dict) => {
            let items: Vec<String> = dict
                .iter()
                .map(|(k, v)| format!("{}: {}", quote(k), describe_plist(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(describe_plist).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Boolean(true) => "True".into(),
        Value::Boolean(false) => "False".into(),
        Value::Integer(i) => match (i.as_signed(), i.as_unsigned()) {
            (Some(v), _) => v.to_string(),
            (None, Some(v)) => v.to_string(),
            (None, None) => "?".into(),
        },
        Value::Real(r) => python_float_repr(*r),
        Value::String(s) => quote(s),
        Value::Data(d) => format!("<{} bytes>", d.len()),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;

    fn fixture(name: &str) -> Json {
        let text = match name {
            "setup" => include_str!("../tests/fixtures/setup_bodies.json"),
            "volume" => include_str!("../tests/fixtures/volume.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(text).expect("fixture")
    }

    fn unhex(value: &Json) -> Vec<u8> {
        hex::decode(value.as_str().expect("hex string")).expect("valid hex")
    }

    fn bplist(value: &Json) -> Value {
        Value::from_reader(std::io::Cursor::new(unhex(value))).expect("bplist")
    }

    /// Convert the fixture's plist-as-JSON convention into a plist value.
    fn from_json(value: &Json) -> Value {
        match value {
            Json::Bool(b) => Value::Boolean(*b),
            Json::Number(n) => {
                if let Some(u) = n.as_u64() {
                    int(u)
                } else if let Some(i) = n.as_i64() {
                    int(i)
                } else {
                    Value::Real(n.as_f64().unwrap())
                }
            }
            Json::String(s) => string(s),
            Json::Array(items) => Value::Array(items.iter().map(from_json).collect()),
            Json::Object(map) if map.contains_key("$data") => Value::Data(unhex(&map["$data"])),
            Json::Object(map) => {
                Value::Dictionary(map.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
            }
            Json::Null => unreachable!("no nulls in plists"),
        }
    }

    #[test]
    fn timing_peer_id_matches_python_uuid5() {
        let fx = fixture("setup");
        assert_eq!(
            timing_peer_id(fx["device_id"].as_str().unwrap()),
            fx["timing_peer_id"].as_str().unwrap()
        );
    }

    #[test]
    fn base_bodies_match_python() {
        let fx = fixture("setup");
        let device_id = fx["device_id"].as_str().unwrap();
        let session_uuid = fx["session_uuid"].as_str().unwrap();
        let sender_name = fx["sender_name"].as_str().unwrap();
        let local_ip: Ipv4Addr = fx["local_ip"].as_str().unwrap().parse().unwrap();
        for case in fx["base"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let receiver_ids: Vec<String> = case["receiver_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            let timing = if case["timing_protocol"] == "PTP" {
                let port = case["ptp"]["port"].as_u64().unwrap() as u16;
                let standard = case["ptp"]["standard_ports"].as_bool().unwrap();
                BaseTiming::Ptp {
                    local_ip,
                    clock_ports: (!standard).then_some(ClockPorts {
                        receiver_ids: &receiver_ids,
                        port,
                    }),
                }
            } else {
                BaseTiming::Ntp {
                    timing_port: case["timing_server_port"].as_u64().unwrap() as u16,
                }
            };
            let body = base_setup_body(&BaseSetup {
                device_id,
                session_uuid,
                sender_name,
                timing,
            });
            assert_eq!(body, bplist(&case["body_bplist"]), "{name}");
            assert_eq!(body, from_json(&case["body"]), "{name}");
        }
    }

    #[test]
    fn stream_bodies_match_python() {
        let fx = fixture("setup");
        let shk: [u8; 32] = unhex(&fx["events_write_key"]).try_into().unwrap();
        let sid = fx["rtsp_session_id"].as_u64().unwrap() as u32;
        for case in fx["stream"].as_array().unwrap() {
            let codec = Codec::parse(case["codec"].as_str().unwrap()).unwrap();
            let ms = case["latency_ms"].as_u64().unwrap() as u32;
            let label = format!("{codec:?} {ms} ms");
            assert_eq!(
                u64::from(latency_frames(ms)),
                case["latency_frames"].as_u64().unwrap(),
                "{label}"
            );
            let (min, max) = latency_bounds(ms);
            assert_eq!(
                u64::from(min),
                case["latency_min"].as_u64().unwrap(),
                "{label}"
            );
            assert_eq!(
                u64::from(max),
                case["latency_max"].as_u64().unwrap(),
                "{label}"
            );
            let port = case["control_client_port"].as_u64().unwrap() as u16;
            let body = stream_setup_body(codec, ms, port, &shk, sid);
            assert_eq!(body, bplist(&case["body_bplist"]), "{label}");
            assert_eq!(body, from_json(&case["body"]), "{label}");
        }
    }

    #[test]
    fn base_responses_parse_like_python() {
        let fx = fixture("setup");
        for case in fx["base"].as_array().unwrap() {
            let name = case["response_name"].as_str().unwrap();
            let response = &fx["responses"][name];
            let parsed = parse_base_setup_response(&bplist(&response["bplist"])).unwrap();
            assert_eq!(
                parsed,
                parse_base_setup_response(&from_json(&response["plist"])).unwrap()
            );
            assert_eq!(
                parsed.clock_id,
                case["read"]["receiver_clock_id"].as_u64(),
                "{name}"
            );
            assert_eq!(
                u64::from(parsed.event_port),
                case["read"]["event_channel"]["port"].as_u64().unwrap(),
                "{name}"
            );
            assert!(!parsed.skip_record, "{name}");
        }
    }

    #[test]
    fn stream_response_parses_like_python() {
        let fx = fixture("setup");
        let parsed =
            parse_stream_setup_response(&bplist(&fx["responses"]["stream"]["bplist"])).unwrap();
        let read = &fx["stream"][0]["read"];
        assert_eq!(
            u64::from(parsed.data_port),
            read["server_port"].as_u64().unwrap()
        );
        assert_eq!(
            u64::from(parsed.control_port),
            read["control_port"].as_u64().unwrap()
        );
        assert_eq!(parsed.render_ms, None);
    }

    #[test]
    fn response_edge_cases() {
        let mut dict = Dictionary::new();
        dict.insert("skipRecord".into(), Value::Boolean(true));
        let parsed = parse_base_setup_response(&Value::Dictionary(dict.clone())).unwrap();
        assert_eq!(
            (parsed.event_port, parsed.clock_id, parsed.skip_record),
            (0, None, true)
        );
        dict.insert("skipRecord".into(), int(0));
        dict.insert("eventPort".into(), int(70_000));
        assert_eq!(
            parse_base_setup_response(&Value::Dictionary(dict)),
            Err(BodyError::Invalid("eventPort"))
        );

        let stream = |extra: Option<(&str, Value)>| {
            let mut s = Dictionary::new();
            s.insert("dataPort".into(), int(1u16));
            s.insert("controlPort".into(), int(2u16));
            if let Some((k, v)) = extra {
                s.insert(k.into(), v);
            }
            let mut d = Dictionary::new();
            d.insert("streams".into(), Value::Array(vec![Value::Dictionary(s)]));
            parse_stream_setup_response(&Value::Dictionary(d)).unwrap()
        };
        assert_eq!(
            stream(Some(("arrivalToRenderLatencyMs", int(69u8)))).render_ms,
            Some(69)
        );
        assert_eq!(
            stream(Some(("arrivalToRenderLatencyMs", int(0u8)))).render_ms,
            None
        );
        assert_eq!(
            stream(Some(("arrivalToRenderLatencyMs", Value::Real(69.9)))).render_ms,
            Some(69)
        );
        assert_eq!(
            parse_stream_setup_response(&Value::Dictionary(Dictionary::new())),
            Err(BodyError::Missing("streams[0]"))
        );
    }

    #[test]
    fn volume_strings_match_python() {
        let fx = fixture("volume");
        for level in fx["levels"].as_array().unwrap() {
            let pct = level["pct"].as_f64().unwrap();
            let bits = u64::from_str_radix(level["dbfs_bits"].as_str().unwrap(), 16).unwrap();
            let dbfs = pct_to_dbfs(pct);
            assert_eq!(dbfs.to_bits(), bits, "pct {pct}");
            assert_eq!(
                python_float_repr(dbfs),
                level["str"].as_str().unwrap(),
                "pct {pct}"
            );
            assert_eq!(
                format!("volume: {}", volume_value(pct)),
                level["body"].as_str().unwrap(),
                "pct {pct}"
            );
            // The same percentages arriving as f32 (config/slider) give the same text.
            assert_eq!(
                volume_value(pct_from_f32(pct as f32)),
                level["str"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn volume_is_clamped() {
        assert_eq!(volume_value(-5.0), "-144.0");
        assert_eq!(volume_value(f64::NAN), "-144.0");
        assert_eq!(volume_value(250.0), "0.0");
    }

    #[test]
    fn python_repr_edge_cases() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (123456789012345680.0, "1.2345678901234568e+17"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (-2.5e300, "-2.5e+300"),
            (5e-324, "5e-324"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
            (44100.0, "44100.0"),
        ];
        for (value, expected) in cases {
            assert_eq!(python_float_repr(value), expected, "{value:e}");
        }
    }

    #[test]
    fn dmap_matches_pyatv() {
        let fx = fixture("volume");
        let m = &fx["metadata"];
        let body = dmap_metadata(
            m["title"].as_str().unwrap(),
            m["album"].as_str().unwrap(),
            m["artist"].as_str().unwrap(),
        );
        assert_eq!(body, unhex(&m["dmap"]));
        assert_eq!(dmap_metadata("", "", ""), b"mlit\0\0\0\0");
        // Byte length, not character count.
        assert_eq!(&dmap_metadata("é", "", "")[8..16], b"minm\0\0\0\x02");
    }

    #[test]
    fn ptp_clock_ports_only_off_standard_port() {
        let ids = vec!["A".to_string(), "B".to_string(), "A".to_string()];
        let ip = Ipv4Addr::new(10, 0, 0, 2);
        let plain = timing_peer_info(ip, "02:11:22:33:44:55", None);
        assert!(plain.as_dictionary().unwrap().get("ClockPorts").is_none());
        let with_ports = timing_peer_info(
            ip,
            "02:11:22:33:44:55",
            Some(ClockPorts {
                receiver_ids: &ids,
                port: 50_123,
            }),
        );
        let ports = with_ports.as_dictionary().unwrap()["ClockPorts"]
            .as_dictionary()
            .unwrap()
            .clone();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports["B"].as_unsigned_integer(), Some(50_123));
    }

    #[test]
    fn describe_plist_looks_like_python() {
        let fx = fixture("setup");
        let text = describe_plist(&bplist(&fx["responses"]["ntp"]["bplist"]));
        assert_eq!(text, "{'eventPort': 50002, 'timingPort': 0}");
        let mut d = Dictionary::new();
        d.insert("sr".into(), Value::Real(44100.0));
        d.insert("ok".into(), Value::Boolean(true));
        d.insert("id".into(), int(-1i64));
        assert_eq!(
            describe_plist(&Value::Dictionary(d)),
            "{'sr': 44100.0, 'ok': True, 'id': -1}"
        );
    }
}
