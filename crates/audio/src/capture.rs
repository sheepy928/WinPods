//! WASAPI capture into the `rt` ring. CONTRACT.
//!
//! Capture thread (MMCSS "Pro Audio", COM MTA): shared-mode event-driven client with
//! a buffer of at least 100 ms; loopback for render endpoints. On every wake (or the
//! 200 ms timeout) drain all packets: `while next_packet_size > 0 { read }` into a
//! buffer preallocated from the buffer size; never use the `_to_deque` read. Packets
//! flagged silent are processed as zeros; data discontinuities are counted. Mix
//! formats f32/i16/i24/i32 with any channel count: take FL/FR (channels 0 and 1),
//! duplicate mono. Resample any rate to 44.1 kHz (see `resample`), clamp to i16,
//! push frames into the ring and publish the chunk peak. Any WASAPI error (device
//! invalidated, format changed, service stopped) ends the thread with
//! [`CaptureExit::DeviceLost`]; the engine restarts capture reusing the producer.
//! `TestTone` generates the beep from `tone` on a high-resolution timer instead.

use winpods_rt::RingProducer;

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureSource {
    /// Loopback of the current default render device (Multimedia role).
    DefaultRender,
    /// A specific endpoint: matched by `id` first, then by `display_name`/`name`.
    Device { id: Option<String>, name: String },
    /// Generated 880 Hz beep once per second (`tone`), nothing is captured.
    TestTone,
}

/// Facts about the running capture, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureInfo {
    pub device_id: String,
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// e.g. "f32", "i16", "i24", "i32", "tone".
    pub format: String,
}

/// How a capture thread ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureExit {
    /// `stop` was called.
    Stopped,
    /// The device went away or changed format (HRESULT and description).
    DeviceLost { hresult: i32, detail: String },
    /// Any other failure (including a panic in the capture thread).
    Failed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("capture device not found: {0}")]
    NotFound(String),
    #[error("audio device error: {0}")]
    Device(String),
}

/// A running capture thread.
pub struct CaptureHandle {
    _private: (),
}

impl CaptureHandle {
    /// Start capturing. On failure the producer is handed back with the error.
    pub fn start(
        source: &CaptureSource,
        producer: RingProducer,
    ) -> Result<CaptureHandle, (CaptureError, RingProducer)> {
        let _ = (source, producer);
        todo!("audio: CaptureHandle::start")
    }
    pub fn info(&self) -> &CaptureInfo {
        todo!("audio: info")
    }
    /// `Some` once the thread has ended on its own (device lost, failure).
    pub fn exit(&self) -> Option<CaptureExit> {
        todo!("audio: exit")
    }
    /// WASAPI data-discontinuity flags seen.
    pub fn discontinuities(&self) -> u64 {
        todo!("audio: discontinuities")
    }
    /// Stop the thread and get the producer back.
    pub fn stop(self) -> (RingProducer, CaptureExit) {
        todo!("audio: stop")
    }
}
