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
//!
//! [`CaptureHandle::start`] opens the device on the new thread and waits until the
//! stream runs (or failed, in which case the producer comes back with the error), so
//! a returned handle always describes a running capture. The thread body runs under
//! `catch_unwind`: a panic ends it with [`CaptureExit::Failed`] and the producer is
//! still returned by [`CaptureHandle::stop`]. In steady state the loop only waits,
//! reads packets into preallocated buffers, converts, resamples and pushes; it does
//! not allocate, log or lock.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use wasapi::{
    AudioCaptureClient, AudioClient, Device, DeviceEnumerator, Direction, SampleType, StreamMode,
    WasapiError,
};
use winpods_rt::{AutoEvent, Frame, HiResTimer, MmcssGuard, RingProducer, Wait, wait_any};

use crate::convert::{SampleFormat, decode_stereo, peak};
use crate::devices::{self, ComGuard, DeviceInfo};
use crate::resample::{ResampleError, StereoResampler};
use crate::tone::{self, ToneGenerator};

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

/// Requested WASAPI buffer: 100 ms (in 100 ns units). Capture latency does not depend
/// on it (packets are drained as soon as they arrive); it only bounds how long the
/// thread may stall before audio is lost.
pub const BUFFER_DURATION: Duration = Duration::from_millis(100);
/// Longest wait for a packet before the loop drains (and flushes the resampler)
/// anyway.
pub const WAIT_TIMEOUT: Duration = Duration::from_millis(200);

/// State shared between the handle and its thread.
struct Shared {
    stop: AutoEvent,
    stop_requested: AtomicBool,
    discontinuities: AtomicU64,
    /// Written once, when the thread ends.
    exit: Mutex<Option<CaptureExit>>,
}

impl Shared {
    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        self.stop.set();
    }
}

/// Outcome of the thread's set-up phase, reported back to `start`.
type SetupResult = Result<CaptureInfo, CaptureError>;

/// A running capture thread.
pub struct CaptureHandle {
    info: CaptureInfo,
    shared: Arc<Shared>,
    /// `Some` until `stop` or `drop` joins the thread.
    thread: Option<JoinHandle<Option<RingProducer>>>,
}

impl CaptureHandle {
    /// Start capturing. On failure the producer is handed back with the error.
    ///
    /// Blocks until the device is open and streaming (normally well under 100 ms).
    pub fn start(
        source: &CaptureSource,
        producer: RingProducer,
    ) -> Result<CaptureHandle, (CaptureError, RingProducer)> {
        let stop = match AutoEvent::new() {
            Ok(event) => event,
            Err(e) => {
                let error = CaptureError::Device(format!("cannot create the stop event: {e}"));
                return Err((error, producer));
            }
        };
        let shared = Arc::new(Shared {
            stop,
            stop_requested: AtomicBool::new(false),
            discontinuities: AtomicU64::new(0),
            exit: Mutex::new(None),
        });
        // The producer travels through a channel after the spawn succeeded, so a
        // failed spawn cannot swallow it.
        let (producer_tx, producer_rx) = mpsc::sync_channel::<RingProducer>(1);
        let (setup_tx, setup_rx) = mpsc::sync_channel::<SetupResult>(1);
        let thread_shared = Arc::clone(&shared);
        let thread_source = source.clone();
        let name = match source {
            CaptureSource::TestTone => "tone",
            _ => "capture",
        };
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || thread_main(thread_source, producer_rx, setup_tx, thread_shared));
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                let error = CaptureError::Device(format!("cannot start the capture thread: {e}"));
                return Err((error, producer));
            }
        };
        if let Err(mpsc::SendError(producer)) = producer_tx.send(producer) {
            let _ = thread.join();
            let error = CaptureError::Device("capture thread ended before start-up".into());
            return Err((error, producer));
        }
        match setup_rx.recv() {
            Ok(Ok(info)) => Ok(CaptureHandle {
                info,
                shared,
                thread: Some(thread),
            }),
            Ok(Err(error)) => Err((error, join_thread(thread))),
            Err(_) => {
                // The thread ended without reporting (a panic during set-up).
                let producer = join_thread(thread);
                let detail = match shared.exit.lock().take() {
                    Some(CaptureExit::Failed(detail)) => detail,
                    other => format!("capture thread ended during start-up: {other:?}"),
                };
                Err((CaptureError::Device(detail), producer))
            }
        }
    }

    pub fn info(&self) -> &CaptureInfo {
        &self.info
    }

    /// `Some` once the thread has ended on its own (device lost, failure).
    pub fn exit(&self) -> Option<CaptureExit> {
        self.shared.exit.lock().clone()
    }

    /// WASAPI data-discontinuity flags seen.
    pub fn discontinuities(&self) -> u64 {
        self.shared.discontinuities.load(Ordering::Relaxed)
    }

    /// Stop the thread and get the producer back.
    pub fn stop(mut self) -> (RingProducer, CaptureExit) {
        self.shared.request_stop();
        let Some(thread) = self.thread.take() else {
            unreachable!("the capture thread is joined only by stop() or drop()");
        };
        let producer = join_thread(thread);
        let exit = self
            .shared
            .exit
            .lock()
            .take()
            .unwrap_or(CaptureExit::Stopped);
        (producer, exit)
    }
}

impl Drop for CaptureHandle {
    /// Dropping a handle without `stop` still ends the thread (the producer is then
    /// dropped with it).
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.shared.request_stop();
            let _ = thread.join();
        }
    }
}

/// Join the capture thread and take the producer back.
fn join_thread(thread: JoinHandle<Option<RingProducer>>) -> RingProducer {
    match thread.join() {
        Ok(Some(producer)) => producer,
        // `start` only waits on (or joins) the thread after handing it the producer,
        // and `thread_main` returns it on every path once received.
        Ok(None) => unreachable!("capture thread never received the ring producer"),
        // The body runs under catch_unwind; a panic can only escape from the panic
        // machinery itself, so propagate it.
        Err(payload) => panic::resume_unwind(payload),
    }
}

/// Entry point of the capture (or tone) thread.
fn thread_main(
    source: CaptureSource,
    producer_rx: Receiver<RingProducer>,
    setup: SyncSender<SetupResult>,
    shared: Arc<Shared>,
) -> Option<RingProducer> {
    let mut producer = producer_rx.recv().ok()?;
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| match &source {
        CaptureSource::TestTone => run_tone(&mut producer, &shared, setup),
        _ => run_wasapi(&source, &mut producer, &shared, setup),
    }));
    let exit = outcome.unwrap_or_else(|payload| {
        CaptureExit::Failed(format!(
            "capture thread panicked: {}",
            panic_text(&*payload)
        ))
    });
    match &exit {
        CaptureExit::Stopped => tracing::debug!("capture stopped"),
        CaptureExit::DeviceLost { hresult, detail } => {
            tracing::warn!("capture device lost ({hresult:#010x}): {detail}");
        }
        CaptureExit::Failed(detail) => tracing::warn!("capture failed: {detail}"),
    }
    *shared.exit.lock() = Some(exit);
    Some(producer)
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-text panic payload")
}

/// Report a set-up failure to `start` and end the thread.
fn setup_failed(setup: &SyncSender<SetupResult>, error: CaptureError) -> CaptureExit {
    let exit = CaptureExit::Failed(error.to_string());
    let _ = setup.send(Err(error));
    exit
}

/// The test tone: no device, the generator paced by a high-resolution timer.
fn run_tone(
    producer: &mut RingProducer,
    shared: &Shared,
    setup: SyncSender<SetupResult>,
) -> CaptureExit {
    let timer = match HiResTimer::new() {
        Ok(timer) => timer,
        Err(e) => {
            let error = CaptureError::Device(format!("cannot create the tone timer: {e}"));
            return setup_failed(&setup, error);
        }
    };
    let mut generator = ToneGenerator::new();
    let info = CaptureInfo {
        device_id: tone::DEVICE_ID.into(),
        device_name: tone::DEVICE_NAME.into(),
        sample_rate: tone::RATE,
        channels: 2,
        format: "tone".into(),
    };
    tracing::info!("test tone started");
    let _ = setup.send(Ok(info));
    drop(setup);
    let _mmcss = MmcssGuard::pro_audio();
    tone::pace(producer, &mut generator, &timer, shared.stop.handle())
}

/// WASAPI capture: open the endpoint, report, then run the drain loop.
fn run_wasapi(
    source: &CaptureSource,
    producer: &mut RingProducer,
    shared: &Shared,
    setup: SyncSender<SetupResult>,
) -> CaptureExit {
    #[cfg(test)]
    test_hooks::maybe_panic(source, &setup);
    // Declared first so COM is uninitialised after the stream below is released.
    let _com = match ComGuard::mta() {
        Ok(guard) => guard,
        Err(e) => {
            let error = CaptureError::Device(format!("COM initialisation failed: {e}"));
            return setup_failed(&setup, error);
        }
    };
    let mut stream = match Stream::open(source) {
        Ok(stream) => stream,
        Err(error) => return setup_failed(&setup, error),
    };
    if let Err(e) = stream.client.start_stream() {
        return setup_failed(&setup, device_error(&e));
    }
    let _ = setup.send(Ok(stream.info.clone()));
    drop(setup);
    let _mmcss = MmcssGuard::pro_audio();
    let exit = stream.run(producer, shared);
    let _ = stream.client.stop_stream();
    exit
}

fn device_error(error: &WasapiError) -> CaptureError {
    CaptureError::Device(error.to_string())
}

fn device_lost(error: &WasapiError) -> CaptureExit {
    CaptureExit::DeviceLost {
        hresult: devices::hresult(error),
        detail: error.to_string(),
    }
}

/// Why the drain loop stopped.
enum StreamError {
    Wasapi(WasapiError),
    Resample(ResampleError),
}

impl From<WasapiError> for StreamError {
    fn from(error: WasapiError) -> Self {
        Self::Wasapi(error)
    }
}

impl From<ResampleError> for StreamError {
    fn from(error: ResampleError) -> Self {
        Self::Resample(error)
    }
}

impl StreamError {
    fn into_exit(self) -> CaptureExit {
        match self {
            Self::Wasapi(e) => device_lost(&e),
            Self::Resample(e) => CaptureExit::Failed(format!("resampling failed: {e}")),
        }
    }
}

/// An open, initialised WASAPI capture stream with all buffers preallocated.
struct Stream {
    // Field order is drop order: the capture client before the audio client.
    capture: AudioCaptureClient,
    client: AudioClient,
    event: wasapi::Handle,
    info: CaptureInfo,
    format: SampleFormat,
    channels: usize,
    block_align: usize,
    /// One packet of raw device bytes (buffer size × block align).
    raw: Vec<u8>,
    /// One packet decoded to stereo `f32`.
    stereo: Vec<[f32; 2]>,
    /// Resampled 44.1 kHz frames of one packet.
    frames: Vec<Frame>,
    resampler: StereoResampler,
    /// True until the first packet was read. WASAPI flags that packet as a
    /// discontinuity (the stream state changed), which is not a glitch.
    first_packet: bool,
}

impl Stream {
    /// Resolve the endpoint and set up a shared-mode, event-driven stream on its mix
    /// format (loopback for render endpoints).
    fn open(source: &CaptureSource) -> Result<Self, CaptureError> {
        let enumerator = DeviceEnumerator::new().map_err(|e| device_error(&e))?;
        let (device, device_info) = resolve_source(&enumerator, source)?;
        let mut client = device.get_iaudioclient().map_err(|e| device_error(&e))?;
        let mix = client.get_mixformat().map_err(|e| device_error(&e))?;
        let float = matches!(
            mix.get_subformat().map_err(|e| device_error(&e))?,
            SampleType::Float
        );
        let bits = mix.get_bitspersample();
        let format = SampleFormat::from_parts(float, bits).ok_or_else(|| {
            CaptureError::Device(format!(
                "unsupported mix format: {} {bits}-bit",
                if float { "float" } else { "integer" }
            ))
        })?;
        let channels = usize::from(mix.get_nchannels());
        let block_align = mix.get_blockalign() as usize;
        if channels == 0 || block_align != channels * format.bytes() {
            return Err(CaptureError::Device(format!(
                "inconsistent mix format: {channels} channels, {block_align}-byte frames"
            )));
        }
        let sample_rate = mix.get_samplespersec();
        let mode = StreamMode::EventsShared {
            autoconvert: false,
            buffer_duration_hns: (BUFFER_DURATION.as_nanos() / 100) as i64,
        };
        // Capture direction on a render endpoint makes this a loopback stream.
        client
            .initialize_client(&mix, &Direction::Capture, &mode)
            .map_err(|e| device_error(&e))?;
        let event = client.set_get_eventhandle().map_err(|e| device_error(&e))?;
        let buffer_frames = client.get_buffer_size().map_err(|e| device_error(&e))? as usize;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|e| device_error(&e))?;
        let resampler = StereoResampler::new(sample_rate, buffer_frames)
            .map_err(|e| CaptureError::Device(e.to_string()))?;
        let frames = vec![[0i16; 2]; resampler.max_output_frames(buffer_frames)];
        tracing::info!(
            "capturing {:?} ({}): {} Hz, {} ch, {}, buffer {} frames, {}",
            device_info.display_name,
            device_info.id,
            sample_rate,
            channels,
            format.name(),
            buffer_frames,
            if resampler.is_bypass() {
                "no resampling".to_owned()
            } else {
                format!("resampler delay {} frames", resampler.output_delay())
            }
        );
        Ok(Self {
            capture,
            client,
            event,
            info: CaptureInfo {
                device_id: device_info.id,
                device_name: device_info.display_name,
                sample_rate,
                channels: mix.get_nchannels(),
                format: format.name().into(),
            },
            format,
            channels,
            block_align,
            raw: vec![0; buffer_frames * block_align],
            stereo: vec![[0.0; 2]; buffer_frames],
            frames,
            resampler,
            first_packet: true,
        })
    }

    /// The drain loop. Allocation-, log- and lock-free until it returns.
    fn run(&mut self, producer: &mut RingProducer, shared: &Shared) -> CaptureExit {
        let handles = [shared.stop.handle(), self.event.as_handle()];
        loop {
            let signaled = match wait_any(&handles, Some(WAIT_TIMEOUT)) {
                Ok(Wait::Signaled(0)) => return CaptureExit::Stopped,
                Ok(Wait::Signaled(_)) => true,
                Ok(Wait::Timeout) => false,
                Err(e) => return CaptureExit::Failed(format!("waiting for audio failed: {e}")),
            };
            if shared.stop_requested.load(Ordering::Acquire) {
                return CaptureExit::Stopped;
            }
            let drained = match self.drain(producer, shared) {
                Ok(frames) => frames,
                Err(e) => return e.into_exit(),
            };
            if !signaled
                && drained == 0
                && let Err(e) = self.flush(producer)
            {
                return e.into_exit();
            }
        }
    }

    /// Read every queued packet; returns the device frames consumed.
    fn drain(
        &mut self,
        producer: &mut RingProducer,
        shared: &Shared,
    ) -> Result<usize, StreamError> {
        let mut total = 0;
        while self.capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            let (frames, info) = self.capture.read_from_device(&mut self.raw)?;
            if info.flags.data_discontinuity && !self.first_packet {
                shared.discontinuities.fetch_add(1, Ordering::Relaxed);
            }
            self.first_packet = false;
            let frames = frames as usize;
            if frames == 0 {
                break;
            }
            // `read_from_device` rejects packets larger than the buffer it was given,
            // which is sized from the WASAPI buffer, so this always fits.
            let capacity = self.stereo.len();
            let stereo = &mut self.stereo[..frames.min(capacity)];
            if info.flags.silent {
                stereo.fill([0.0; 2]);
            } else {
                let bytes = &self.raw[..stereo.len() * self.block_align];
                decode_stereo(self.format, self.channels, bytes, stereo);
            }
            let produced = self.resampler.process(stereo, &mut self.frames)?;
            publish(producer, &self.frames[..produced]);
            total += frames;
        }
        Ok(total)
    }

    /// Push the resampler's delayed tail out after capture went quiet.
    fn flush(&mut self, producer: &mut RingProducer) -> Result<(), StreamError> {
        let produced = self.resampler.flush(&mut self.frames)?;
        publish(producer, &self.frames[..produced]);
        Ok(())
    }
}

/// Publish a chunk: peak first, then the frames (the push wakes the sender).
fn publish(producer: &mut RingProducer, frames: &[Frame]) {
    if frames.is_empty() {
        return;
    }
    producer.set_peak(peak(frames));
    producer.push(frames);
}

/// Find the endpoint for a capture source.
fn resolve_source(
    enumerator: &DeviceEnumerator,
    source: &CaptureSource,
) -> Result<(Device, DeviceInfo), CaptureError> {
    match source {
        CaptureSource::DefaultRender => devices::default_render(enumerator)
            .map_err(|e| device_error(&e))?
            .ok_or_else(|| CaptureError::NotFound("default output device".into())),
        CaptureSource::Device { id, name } => devices::resolve(enumerator, id.as_deref(), name)
            .map_err(|e| device_error(&e))?
            .ok_or_else(|| CaptureError::NotFound(name.clone())),
        CaptureSource::TestTone => Err(CaptureError::Device(
            "the test tone has no audio device".into(),
        )),
    }
}

/// Fault injection for the panic-path tests: special endpoint ids that make the
/// capture thread panic before or after reporting a successful start.
#[cfg(test)]
mod test_hooks {
    use super::*;

    pub(super) const PANIC_DURING_SETUP: &str = "winpods-test:panic-during-setup";
    pub(super) const PANIC_AFTER_START: &str = "winpods-test:panic-after-start";

    pub(super) fn maybe_panic(source: &CaptureSource, setup: &SyncSender<SetupResult>) {
        let CaptureSource::Device { id: Some(id), .. } = source else {
            return;
        };
        match id.as_str() {
            PANIC_DURING_SETUP => panic!("injected set-up panic"),
            PANIC_AFTER_START => {
                let info = CaptureInfo {
                    device_id: id.clone(),
                    device_name: "injected".into(),
                    sample_rate: 48_000,
                    channels: 2,
                    format: "f32".into(),
                };
                let _ = setup.send(Ok(info));
                panic!("injected capture panic");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winpods_rt::ring;

    fn start(source: &CaptureSource, producer: RingProducer) -> CaptureHandle {
        match CaptureHandle::start(source, producer) {
            Ok(handle) => handle,
            Err((error, _)) => panic!("capture did not start: {error}"),
        }
    }

    #[test]
    fn panic_payload_text() {
        let s: Box<dyn Any + Send> = Box::new("boom");
        assert_eq!(panic_text(&*s), "boom");
        let s: Box<dyn Any + Send> = Box::new(String::from("bang"));
        assert_eq!(panic_text(&*s), "bang");
        let s: Box<dyn Any + Send> = Box::new(42u8);
        assert_eq!(panic_text(&*s), "non-text panic payload");
    }

    #[test]
    fn handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CaptureHandle>();
    }

    /// Unknown devices fail at start and hand the producer back. Read-only: it only
    /// enumerates endpoints.
    #[test]
    fn unknown_device_returns_producer() {
        let (producer, _consumer) = ring::ring(1_000).unwrap();
        let source = CaptureSource::Device {
            id: Some("{0.0.0.00000000}.{00000000-0000-0000-0000-000000000000}".into()),
            name: "WinPods test: no such device".into(),
        };
        match CaptureHandle::start(&source, producer) {
            Ok(_) => panic!("capture started on a device that does not exist"),
            Err((CaptureError::NotFound(name), producer)) => {
                assert_eq!(name, "WinPods test: no such device");
                assert_eq!(producer.shared().written(), 0);
            }
            Err((other, _)) => panic!("unexpected error {other}"),
        }
    }

    /// The tone runs at real-time pace in 220-frame chunks and stops cleanly.
    #[test]
    fn test_tone_paces_220_frame_chunks() {
        let (producer, mut consumer) = ring::ring(ring::DEFAULT_CAPACITY).unwrap();
        let handle = start(&CaptureSource::TestTone, producer);
        assert_eq!(handle.info().format, "tone");
        assert_eq!(handle.info().sample_rate, 44_100);
        assert_eq!(handle.info().device_id, tone::DEVICE_ID);
        let started = std::time::Instant::now();
        // Mid-beep (the beep is the first 120 ms of every second): about -22 dBFS.
        std::thread::sleep(Duration::from_millis(60));
        let level = consumer.shared().peak();
        assert!(
            (0.05..=0.081).contains(&level),
            "peak {level} during the beep"
        );
        std::thread::sleep(Duration::from_millis(540));
        assert!(handle.exit().is_none());
        assert_eq!(handle.discontinuities(), 0);
        let (producer, exit) = handle.stop();
        let elapsed = started.elapsed().as_secs_f64();
        assert_eq!(exit, CaptureExit::Stopped);
        let shared = producer.shared();
        assert_eq!(shared.last_chunk(), 220);
        let written = shared.written() as f64;
        let expected = elapsed * 44_100.0;
        assert!(
            written > expected * 0.8 && written <= expected + 2.0 * 220.0,
            "{written} frames in {elapsed:.3} s"
        );
        // The first chunk is the start of a beep.
        let mut first = [[0i16; 2]; 220];
        assert!(consumer.pop_exact(&mut first));
        assert!(first.iter().any(|f| f[0] != 0));
        // 600 ms in, the track is silent between beeps.
        assert_eq!(shared.peak(), 0.0);
        // The producer is reusable for the next capture.
        let handle = start(&CaptureSource::TestTone, producer);
        let (_producer, exit) = handle.stop();
        assert_eq!(exit, CaptureExit::Stopped);
    }

    #[test]
    fn panic_during_setup_returns_producer() {
        let (producer, _consumer) = ring::ring(100).unwrap();
        let source = CaptureSource::Device {
            id: Some(test_hooks::PANIC_DURING_SETUP.into()),
            name: String::new(),
        };
        match CaptureHandle::start(&source, producer) {
            Ok(_) => panic!("start succeeded despite the panic"),
            Err((CaptureError::Device(detail), producer)) => {
                assert!(detail.contains("injected set-up panic"), "{detail}");
                assert_eq!(producer.shared().capacity(), 100);
            }
            Err((other, _)) => panic!("unexpected error {other}"),
        }
    }

    #[test]
    fn panic_while_running_is_reported_and_producer_survives() {
        let (producer, _consumer) = ring::ring(100).unwrap();
        let source = CaptureSource::Device {
            id: Some(test_hooks::PANIC_AFTER_START.into()),
            name: String::new(),
        };
        let handle = start(&source, producer);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handle.exit().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let expected =
            CaptureExit::Failed("capture thread panicked: injected capture panic".into());
        assert_eq!(handle.exit(), Some(expected.clone()));
        let (producer, exit) = handle.stop();
        assert_eq!(exit, expected);
        assert_eq!(producer.shared().capacity(), 100);
    }

    #[test]
    fn dropping_the_handle_stops_the_thread() {
        let (producer, consumer) = ring::ring(ring::DEFAULT_CAPACITY).unwrap();
        let handle = start(&CaptureSource::TestTone, producer);
        std::thread::sleep(Duration::from_millis(50));
        drop(handle);
        let written = consumer.shared().written();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(consumer.shared().written(), written, "thread still writing");
    }
}
