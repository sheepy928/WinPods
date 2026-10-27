//! Types shared across the AirPlay modules and with the engine. FROZEN CONTRACT:
//! workstreams must not change these without coordinating.

use std::net::Ipv4Addr;

/// Stream sample rate (Hz). AirPlay realtime audio is always 44.1 kHz.
pub const SAMPLE_RATE: u32 = 44_100;
/// Audio channels.
pub const CHANNELS: u16 = 2;
/// Frames per RTP audio packet ("spf").
pub const FRAMES_PER_PACKET: usize = 352;
/// Size of an uncompressed ALAC escape frame for 352 stereo frames.
pub const ALAC_FRAME_BYTES: usize = 1416;
/// Size of a big-endian PCM payload for 352 stereo frames.
pub const PCM_FRAME_BYTES: usize = FRAMES_PER_PACKET * 4;
/// Receiver render time assumed when SETUP does not report `arrivalToRenderLatencyMs`.
pub const DEFAULT_RENDER_MS: u32 = 70;
/// Allowed latency range in milliseconds.
pub const MIN_LATENCY_MS: u32 = 75;
pub const MAX_LATENCY_MS: u32 = 5000;

/// Audio payload format of the realtime stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Codec {
    /// Uncompressed ALAC escape frames (audioFormat 0x40000, ct 2). HomePods need this.
    #[default]
    Alac,
    /// Raw big-endian 16-bit PCM (audioFormat 0x800, ct 1).
    Pcm,
}

impl Codec {
    /// AirPlay 2 `audioFormat` bit for SETUP.
    pub fn audio_format(self) -> u64 {
        match self {
            Codec::Alac => 0x40000,
            Codec::Pcm => 0x800,
        }
    }
    /// AirPlay 2 compression type (`ct`) for SETUP.
    pub fn compression_type(self) -> u64 {
        match self {
            Codec::Alac => 2,
            Codec::Pcm => 1,
        }
    }
    /// Plaintext payload size per packet.
    pub fn payload_bytes(self) -> usize {
        match self {
            Codec::Alac => ALAC_FRAME_BYTES,
            Codec::Pcm => PCM_FRAME_BYTES,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::Alac => "alac",
            Codec::Pcm => "pcm",
        }
    }
    /// Parse a config/CLI value ("alac" / "pcm", case-insensitive).
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "alac" => Some(Codec::Alac),
            "pcm" => Some(Codec::Pcm),
            _ => None,
        }
    }
}

/// Clock protocol offered to an AirPlay 2 receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TimingProtocol {
    /// Follow the receiver's PTP master (HomePods on current firmware require this).
    #[default]
    Ptp,
    /// Legacy NTP-style timing (we answer timing requests, send 0xD4 sync packets).
    Ntp,
}

impl TimingProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            TimingProtocol::Ptp => "PTP",
            TimingProtocol::Ntp => "NTP",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "PTP" => Some(TimingProtocol::Ptp),
            "NTP" => Some(TimingProtocol::Ntp),
            _ => None,
        }
    }
}

/// What discovery learned about a receiver from its mDNS records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverInfo {
    /// Human name (RAOP instance name after '@', else the AirPlay instance name).
    pub name: String,
    /// Model identifier (TXT `am` / `model`), e.g. "AudioAccessory5,1".
    pub model: String,
    pub address: Ipv4Addr,
    /// RTSP port of the RAOP service (usually 7000).
    pub raop_port: u16,
    /// Feature bits from TXT `ft`/`features` ("0xLOW" or "0xLOW,0xHIGH").
    pub features: u64,
    /// Metadata types from TXT `md` ("0,1,2" → [0, 1, 2]); 0 = text metadata.
    pub metadata_types: Vec<u8>,
    /// Encryption types from TXT `et`.
    pub encryption_types: Vec<u8>,
    /// gid/pi/psi values cut at '+', AirPlay service first then RAOP, deduplicated.
    /// Used as `ClockPorts` keys when we cannot listen on UDP 319.
    pub receiver_ids: Vec<String>,
    /// TXT `pw` = true or the password flag in `sf`/`flags`.
    pub requires_password: bool,
    /// Raw TXT records, for logging and diagnostics.
    pub raop_txt: Vec<(String, String)>,
    pub airplay_txt: Vec<(String, String)>,
}

impl ReceiverInfo {
    /// Bit 38 (SupportsUnifiedMediaControl) or bit 48 (CoreUtils pairing) → AirPlay 2.
    pub fn is_airplay2(&self) -> bool {
        self.features & (1 << 38) != 0 || self.features & (1 << 48) != 0
    }
    /// Bit 43 (SystemPairing) or bit 48 (CoreUtils pairing) → transient HAP pairing.
    pub fn supports_transient_pairing(&self) -> bool {
        self.features & (1 << 43) != 0 || self.features & (1 << 48) != 0
    }
    /// TXT `md` contains 0 → send DMAP text metadata (title/artist/album).
    pub fn supports_text_metadata(&self) -> bool {
        self.metadata_types.contains(&0)
    }
}

/// Everything the real-time runtime needs about a negotiated stream. Produced by
/// `session::Session::setup`, consumed by `runtime::start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamParams {
    pub receiver: Ipv4Addr,
    /// Local address of the RTSP connection (bind UDP sockets to it).
    pub local_ip: Ipv4Addr,
    /// Receiver UDP port for audio (`dataPort`).
    pub data_port: u16,
    /// Receiver UDP port for anchors and retransmit replies (`controlPort`).
    pub control_port: u16,
    /// Audio encryption key (`shk`, the Events-Write key).
    pub shk: [u8; 32],
    pub codec: Codec,
    pub timing: TimingProtocol,
    pub latency_ms: u32,
    /// `latency_frames(latency_ms)`; also the anchor latency and the first timestamp.
    pub latency_frames: u32,
    /// `arrivalToRenderLatencyMs` from the stream SETUP response.
    pub render_ms: Option<u32>,
    /// Receiver `timingPeerInfo.ClockID` from the base SETUP response (PTP).
    pub clock_id: Option<u64>,
    /// RTSP session id; also `streamConnectionID` and the RTP SSRC.
    pub ssrc: u32,
    /// Sequence number of the first audio packet (random).
    pub first_seq: u16,
    /// RTP timestamp of the first audio packet (= latency_frames), unwrapped.
    pub first_ts: u64,
}

/// `round_half_even(ms × 44.1)`, matching Python's `int(round(ms * 44100 / 1000))`.
pub fn latency_frames(latency_ms: u32) -> u32 {
    // ms * 441 / 10 exactly; ties (x.5) round to even.
    let tenths = u64::from(latency_ms) * 441;
    let whole = tenths / 10;
    let rem = tenths % 10;
    let rounded = match rem.cmp(&5) {
        std::cmp::Ordering::Less => whole,
        std::cmp::Ordering::Greater => whole + 1,
        std::cmp::Ordering::Equal => whole + (whole & 1),
    };
    rounded as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_frames_matches_python_round() {
        // Python: int(round(ms * 44100 / 1000))
        assert_eq!(latency_frames(100), 4410);
        assert_eq!(latency_frames(75), 3308); // 3307.5 -> 3308 (even)
        assert_eq!(latency_frames(85), 3748); // 3748.5 -> 3748 (even)
        assert_eq!(latency_frames(250), 11025);
        assert_eq!(latency_frames(2001), 88244); // 88244.1
        assert_eq!(latency_frames(5000), 220500);
    }

    #[test]
    fn codec_constants() {
        assert_eq!(Codec::Alac.audio_format(), 0x40000);
        assert_eq!(Codec::Pcm.compression_type(), 1);
        assert_eq!(Codec::parse("ALAC"), Some(Codec::Alac));
        assert_eq!(TimingProtocol::parse("ntp"), Some(TimingProtocol::Ntp));
    }
}
