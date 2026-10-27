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
//!
//! Everything after the device is open is device-independent: a `DrainLoop` reads a
//! `PacketSource` (the WASAPI capture client in production) through a
//! `PacketConverter` (silent packets, discontinuities, decoding, resampling, idle
//! reset), so the loop is unit-tested against scripted packets.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use wasapi::{
    AudioCaptureClient, AudioClient, BufferFlags, Device, DeviceEnumerator, Direction, SampleType,
    StreamMode, WasapiError,
};
use winpods_rt::{AutoEvent, Frame, HANDLE, HiResTimer, MmcssGuard, RingProducer, Wait, wait_any};

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
/// Longest wait for a packet before the loop drains anyway. A timeout with nothing
/// queued means capture went quiet: the resampler is then reset, dropping the ~1.3 ms
/// tail it still holds instead of publishing it, so the next sound starts from a
/// clean filter.
///
/// The reset lands around the sender's own stall. The sender pads with silence after
/// `max(latency - render - 5 ms clamped to 5..=250 ms, 40 ms)` of quiet
/// (`StallPolicy::stall_ns` in winpods-airplay), which is at or before these 200 ms
/// for latencies up to about 275 ms (the default is 100 ms). There a published tail
/// would land after the silence already sent, out of place. With a longer latency
/// the sender stalls at 201-250 ms, after the reset, and the tail could still have
/// been padded in with the remainder; it is lost instead. Losing ~1.3 ms of a sound's
/// end is accepted so capture does not depend on the sender's timing.
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
    fn new() -> windows::core::Result<Self> {
        Ok(Self {
            stop: AutoEvent::new()?,
            stop_requested: AtomicBool::new(false),
            discontinuities: AtomicU64::new(0),
            exit: Mutex::new(None),
        })
    }

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
        let shared = match Shared::new() {
            Ok(shared) => Arc::new(shared),
            Err(e) => {
                let error = CaptureError::Device(format!("cannot create the stop event: {e}"));
                return Err((error, producer));
            }
        };
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
    let _mmcss = join_mmcss("tone");
    tone::pace(producer, &mut generator, &timer, shared.stop.handle())
}

/// Join the MMCSS "Pro Audio" task for the calling thread. A refusal is logged here,
/// before the thread's loop starts: as a warning the first time, then at debug level.
fn join_mmcss(thread: &str) -> Option<MmcssGuard> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let guard = MmcssGuard::pro_audio();
    if guard.is_none() {
        // AvSetMmThreadCharacteristicsW leaves the reason in the thread's last error.
        let error = std::io::Error::last_os_error();
        let detail = match error.raw_os_error() {
            Some(0) | None => String::new(),
            Some(_) => format!(" ({error})"),
        };
        if WARNED.swap(true, Ordering::Relaxed) {
            tracing::debug!("{thread} thread: MMCSS \"Pro Audio\" registration failed{detail}");
        } else {
            tracing::warn!(
                "{thread} thread: MMCSS \"Pro Audio\" registration failed{detail}; it runs at \
                 normal priority"
            );
        }
    }
    guard
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
    let _mmcss = join_mmcss("capture");
    let exit = stream
        .drain
        .run(stream.event.as_handle(), producer, shared, WAIT_TIMEOUT);
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
#[derive(Debug)]
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
    /// Every WASAPI error ends capture as a lost device (the engine restarts it); a
    /// resampler error is an internal failure.
    fn into_exit(self) -> CaptureExit {
        match self {
            Self::Wasapi(e) => device_lost(&e),
            Self::Resample(e) => CaptureExit::Failed(format!("resampling failed: {e}")),
        }
    }
}

/// The packet flags the conversion acts on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PacketFlags {
    /// `AUDCLNT_BUFFERFLAGS_SILENT`: the packet is silence, whatever its bytes hold.
    silent: bool,
    /// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`: audio was lost before this packet.
    discontinuity: bool,
}

impl From<&BufferFlags> for PacketFlags {
    fn from(flags: &BufferFlags) -> Self {
        Self {
            silent: flags.silent,
            discontinuity: flags.data_discontinuity,
        }
    }
}

/// The two capture-client calls the drain loop makes. Behind a trait so the loop
/// runs unchanged against scripted packets in tests; [`AudioCaptureClient`] is the
/// only production implementation.
trait PacketSource {
    /// Frames in the next queued packet; 0 when nothing is queued.
    fn next_packet_frames(&mut self) -> Result<usize, WasapiError>;
    /// Copy the next packet to the front of `buffer`; returns its frame count and
    /// flags.
    fn read_packet(&mut self, buffer: &mut [u8]) -> Result<(usize, PacketFlags), WasapiError>;
}

impl PacketSource for AudioCaptureClient {
    fn next_packet_frames(&mut self) -> Result<usize, WasapiError> {
        Ok(self.get_next_packet_size()?.unwrap_or(0) as usize)
    }

    fn read_packet(&mut self, buffer: &mut [u8]) -> Result<(usize, PacketFlags), WasapiError> {
        let (frames, info) = self.read_from_device(buffer)?;
        Ok((frames as usize, PacketFlags::from(&info.flags)))
    }
}

/// Device-independent handling of captured packets: decode the mix format to stereo
/// `f32`, resample to 44.1 kHz `i16`, count discontinuities and reset the resampler
/// when capture goes quiet. Every buffer is allocated in [`PacketConverter::new`].
struct PacketConverter {
    format: SampleFormat,
    channels: usize,
    block_align: usize,
    /// One packet decoded to stereo `f32` (sized for the largest packet).
    stereo: Vec<[f32; 2]>,
    /// Resampled 44.1 kHz frames of one packet.
    frames: Vec<Frame>,
    resampler: StereoResampler,
    /// True until the first packet was converted. WASAPI flags that packet as a
    /// discontinuity (the stream state changed), which is not a glitch.
    first_packet: bool,
}

impl PacketConverter {
    /// A converter for packets of up to `max_packet_frames` frames in `format` with
    /// `channels` interleaved channels at `sample_rate`.
    fn new(
        format: SampleFormat,
        channels: usize,
        sample_rate: u32,
        max_packet_frames: usize,
    ) -> Result<Self, CaptureError> {
        if channels == 0 {
            return Err(CaptureError::Device("mix format has no channels".into()));
        }
        let max_packet_frames = max_packet_frames.max(1);
        let resampler = StereoResampler::new(sample_rate, max_packet_frames)
            .map_err(|e| CaptureError::Device(e.to_string()))?;
        let frames = vec![[0; 2]; resampler.max_output_frames(max_packet_frames)];
        Ok(Self {
            format,
            channels,
            block_align: channels * format.bytes(),
            stereo: vec![[0.0; 2]; max_packet_frames],
            frames,
            resampler,
            first_packet: true,
        })
    }

    /// Bytes of the largest packet: the size of the raw read buffer.
    fn max_packet_bytes(&self) -> usize {
        self.stereo.len() * self.block_align
    }

    /// Convert one packet of `frames` device frames held at the front of `raw` and
    /// return the 44.1 kHz frames it produced (possibly none).
    ///
    /// A silent packet becomes zeros whatever its bytes hold. A discontinuity is
    /// added to `discontinuities` unless it is flagged on the stream's first packet.
    /// `frames` is clamped to what both `raw` and the preallocated buffers hold
    /// (WASAPI never returns more than the read buffer, which is sized the same way).
    fn packet(
        &mut self,
        frames: usize,
        flags: PacketFlags,
        raw: &[u8],
        discontinuities: &AtomicU64,
    ) -> Result<&[Frame], ResampleError> {
        if flags.discontinuity && !self.first_packet {
            discontinuities.fetch_add(1, Ordering::Relaxed);
        }
        self.first_packet = false;
        let frames = frames
            .min(self.stereo.len())
            .min(raw.len() / self.block_align);
        let stereo = &mut self.stereo[..frames];
        if flags.silent {
            stereo.fill([0.0; 2]);
        } else {
            let bytes = &raw[..frames * self.block_align];
            decode_stereo(self.format, self.channels, bytes, stereo);
        }
        let produced = self.resampler.process(stereo, &mut self.frames)?;
        Ok(&self.frames[..produced])
    }

    /// End of one wake of the capture loop. When the wait timed out and no packet
    /// was drained, capture went quiet: reset the resampler (see [`WAIT_TIMEOUT`])
    /// rather than publish its tail ~200 ms late, where (at the usual latencies) the
    /// sender would put it after the silence it already sent. Returns true when it
    /// reset (once per sound: not again until more audio was converted). Never
    /// allocates.
    fn after_wake(&mut self, signaled: bool, drained: usize) -> bool {
        if signaled || drained > 0 || !self.resampler.has_tail() {
            return false;
        }
        self.resampler.reset();
        true
    }
}

/// The device-independent capture loop: drains a [`PacketSource`] through a
/// [`PacketConverter`] into the ring.
struct DrainLoop<S> {
    source: S,
    /// One packet of raw device bytes (largest packet × block align).
    raw: Vec<u8>,
    converter: PacketConverter,
}

impl<S: PacketSource> DrainLoop<S> {
    fn new(source: S, converter: PacketConverter) -> Self {
        Self {
            source,
            raw: vec![0; converter.max_packet_bytes()],
            converter,
        }
    }

    /// Wait for `packet_event` (at most `timeout` each time) and handle every wake
    /// until stop is requested or an error ends capture. Allocation-, log- and
    /// lock-free until it returns.
    fn run(
        &mut self,
        packet_event: HANDLE,
        producer: &mut RingProducer,
        shared: &Shared,
        timeout: Duration,
    ) -> CaptureExit {
        let handles = [shared.stop.handle(), packet_event];
        loop {
            let signaled = match wait_any(&handles, Some(timeout)) {
                Ok(Wait::Signaled(0)) => return CaptureExit::Stopped,
                Ok(Wait::Signaled(_)) => true,
                Ok(Wait::Timeout) => false,
                Err(e) => return CaptureExit::Failed(format!("waiting for audio failed: {e}")),
            };
            if shared.stop_requested.load(Ordering::Acquire) {
                return CaptureExit::Stopped;
            }
            if let Err(e) = self.wake(signaled, producer, &shared.discontinuities) {
                return e.into_exit();
            }
        }
    }

    /// One wake: drain every queued packet, then reset the resampler if the wait
    /// timed out with nothing queued. Returns the device frames drained.
    fn wake(
        &mut self,
        signaled: bool,
        producer: &mut RingProducer,
        discontinuities: &AtomicU64,
    ) -> Result<usize, StreamError> {
        let drained = self.drain(producer, discontinuities)?;
        self.converter.after_wake(signaled, drained);
        Ok(drained)
    }

    /// Read, convert and publish every queued packet; returns the device frames
    /// consumed.
    fn drain(
        &mut self,
        producer: &mut RingProducer,
        discontinuities: &AtomicU64,
    ) -> Result<usize, StreamError> {
        let mut total = 0;
        while self.source.next_packet_frames()? > 0 {
            let (frames, flags) = self.source.read_packet(&mut self.raw)?;
            let converted = self
                .converter
                .packet(frames, flags, &self.raw, discontinuities)?;
            publish(producer, converted);
            if frames == 0 {
                break;
            }
            total += frames;
        }
        Ok(total)
    }
}

/// An open, initialised WASAPI capture stream with all buffers preallocated.
struct Stream {
    // Field order is drop order: the capture client (inside the loop) before the
    // audio client, and the client before its event handle.
    drain: DrainLoop<AudioCaptureClient>,
    client: AudioClient,
    event: wasapi::Handle,
    info: CaptureInfo,
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
        let converter = PacketConverter::new(format, channels, sample_rate, buffer_frames)?;
        tracing::info!(
            "capturing {:?} ({}): {} Hz, {} ch, {}, buffer {} frames, {}",
            device_info.display_name,
            device_info.id,
            sample_rate,
            channels,
            format.name(),
            buffer_frames,
            if converter.resampler.is_bypass() {
                "no resampling".to_owned()
            } else {
                format!(
                    "resampler delay {} frames",
                    converter.resampler.output_delay()
                )
            }
        );
        Ok(Self {
            drain: DrainLoop::new(capture, converter),
            client,
            event,
            info: CaptureInfo {
                device_id: device_info.id,
                device_name: device_info.display_name,
                sample_rate,
                channels: mix.get_nchannels(),
                format: format.name().into(),
            },
        })
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
    use winpods_rt::{RingShared, ring};

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

    // ---- The device-independent capture loop, driven by scripted packets. ----

    /// HRESULT of `AUDCLNT_E_DEVICE_INVALIDATED` (the `windows` crate defines it only
    /// behind the `Win32_Media_Audio` feature, which this crate does not enable).
    const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;

    const PLAIN: PacketFlags = PacketFlags {
        silent: false,
        discontinuity: false,
    };
    const SILENT: PacketFlags = PacketFlags {
        silent: true,
        discontinuity: false,
    };
    const DISCONTINUITY: PacketFlags = PacketFlags {
        silent: false,
        discontinuity: true,
    };

    fn device_invalidated() -> WasapiError {
        WasapiError::Windows(windows::core::Error::from_hresult(windows::core::HRESULT(
            AUDCLNT_E_DEVICE_INVALIDATED,
        )))
    }

    fn f32_bytes(frames: &[[f32; 2]]) -> Vec<u8> {
        frames
            .as_flattened()
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    fn constant(level: f32, frames: usize) -> Vec<[f32; 2]> {
        vec![[level, level]; frames]
    }

    /// A 1 kHz sine at half scale, 48 kHz.
    fn sine_48k(frames: usize) -> Vec<[f32; 2]> {
        (0..frames)
            .map(|n| {
                let phase = std::f64::consts::TAU * 1_000.0 * n as f64 / 48_000.0;
                let v = (0.5 * phase.sin()) as f32;
                [v, v]
            })
            .collect()
    }

    /// Stereo `f32` converter for packets of up to 480 frames.
    fn converter(rate: u32) -> PacketConverter {
        PacketConverter::new(SampleFormat::F32, 2, rate, 480).unwrap()
    }

    #[test]
    fn silent_packets_become_zeros_whatever_their_bytes() {
        let counter = AtomicU64::new(0);
        let raw = f32_bytes(&constant(0.5, 64));
        // At 44.1 kHz (bypass) output frames map 1:1 to input frames.
        let mut conv = converter(44_100);
        let out = conv.packet(64, SILENT, &raw, &counter).unwrap();
        assert_eq!(out.len(), 64);
        assert!(out.iter().all(|f| *f == [0, 0]), "{out:?}");
        // The same bytes without the flag are audio.
        let out = conv.packet(64, PLAIN, &raw, &counter).unwrap();
        assert!(out.iter().all(|f| *f == [16_384, 16_384]), "{out:?}");
        // Through the resampler and from an integer mix format, too.
        let loud: Vec<u8> = [i16::MAX; 2 * 480]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut conv = PacketConverter::new(SampleFormat::I16, 2, 48_000, 480).unwrap();
        let out = conv.packet(480, SILENT, &loud, &counter).unwrap();
        assert!(!out.is_empty());
        assert!(out.iter().all(|f| *f == [0, 0]));
        let out = conv.packet(480, SILENT, &loud, &counter).unwrap();
        assert!(
            out.iter().all(|f| *f == [0, 0]),
            "the filter held only zeros"
        );
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn discontinuities_are_counted_except_on_the_first_packet() {
        let counter = AtomicU64::new(0);
        let count = || counter.load(Ordering::Relaxed);
        let raw = f32_bytes(&constant(0.25, 32));
        let mut conv = converter(44_100);
        conv.packet(32, DISCONTINUITY, &raw, &counter).unwrap();
        assert_eq!(count(), 0, "the first packet's flag is not a glitch");
        conv.packet(32, PLAIN, &raw, &counter).unwrap();
        assert_eq!(count(), 0);
        conv.packet(32, DISCONTINUITY, &raw, &counter).unwrap();
        assert_eq!(count(), 1);
        let both = PacketFlags {
            silent: true,
            discontinuity: true,
        };
        conv.packet(32, both, &raw, &counter).unwrap();
        assert_eq!(count(), 2);
        // A packet that read empty still reports its discontinuity.
        assert!(
            conv.packet(0, DISCONTINUITY, &raw, &counter)
                .unwrap()
                .is_empty()
        );
        assert_eq!(count(), 3);
        // Only the very first packet is exempt, whatever its own flags.
        let mut conv = converter(44_100);
        conv.packet(32, PLAIN, &raw, &counter).unwrap();
        conv.packet(32, DISCONTINUITY, &raw, &counter).unwrap();
        assert_eq!(count(), 4);
    }

    /// The idle timeout comes 200 ms after capture went quiet (at or after the
    /// sender's stall for latencies up to ~275 ms): the resampler's tail is dropped
    /// there, never published late or prepended to the next sound.
    #[test]
    fn idle_timeout_resets_the_resampler_instead_of_publishing_its_tail() {
        let counter = AtomicU64::new(0);
        let sine = f32_bytes(&sine_48k(480));
        let zeros = f32_bytes(&constant(0.0, 480));
        // Without a reset the next sound would start with the sine's delayed end.
        let mut conv = converter(48_000);
        conv.packet(480, PLAIN, &sine, &counter).unwrap();
        let next = conv.packet(480, PLAIN, &zeros, &counter).unwrap();
        assert!(next.iter().any(|f| f[0] != 0), "the filter held a tail");

        let mut conv = converter(48_000);
        assert!(!conv.packet(480, PLAIN, &sine, &counter).unwrap().is_empty());
        // A wake that was signaled or drained packets keeps the filter as it is.
        assert!(!conv.after_wake(true, 0));
        assert!(!conv.after_wake(false, 480));
        assert!(!conv.after_wake(true, 480));
        assert!(conv.resampler.has_tail());
        // An idle timeout resets it, once.
        assert!(conv.after_wake(false, 0));
        assert!(!conv.resampler.has_tail());
        assert!(!conv.after_wake(false, 0));
        // The next sound starts clean.
        let next = conv.packet(480, PLAIN, &zeros, &counter).unwrap();
        assert!(!next.is_empty());
        assert!(next.iter().all(|f| *f == [0, 0]), "stale tail {next:?}");
        // New audio arms it again.
        assert!(conv.after_wake(false, 0));
        // Without resampling nothing is held back.
        let mut conv = converter(44_100);
        conv.packet(480, PLAIN, &sine, &counter).unwrap();
        assert!(!conv.after_wake(false, 0));
    }

    #[test]
    fn oversized_packets_are_clamped_to_the_buffers() {
        let counter = AtomicU64::new(0);
        let mut conv = PacketConverter::new(SampleFormat::F32, 2, 44_100, 64).unwrap();
        assert_eq!(conv.max_packet_bytes(), 64 * 8);
        // More frames than the converter was sized for...
        let raw = f32_bytes(&constant(0.5, 100));
        assert_eq!(conv.packet(1_000, PLAIN, &raw, &counter).unwrap().len(), 64);
        // ...or than the bytes hold (only whole frames count).
        let raw = f32_bytes(&constant(0.5, 10));
        assert_eq!(conv.packet(1_000, PLAIN, &raw, &counter).unwrap().len(), 10);
        let partial = &raw[..raw.len() - 3];
        assert_eq!(conv.packet(64, PLAIN, partial, &counter).unwrap().len(), 9);
        assert!(PacketConverter::new(SampleFormat::F32, 0, 48_000, 64).is_err());
        assert!(PacketConverter::new(SampleFormat::F32, 2, 0, 64).is_err());
    }

    #[test]
    fn packet_flags_from_wasapi() {
        assert_eq!(PacketFlags::from(&BufferFlags::new(0)), PLAIN);
        assert_eq!(PacketFlags::from(&BufferFlags::new(1)), DISCONTINUITY);
        assert_eq!(PacketFlags::from(&BufferFlags::new(2)), SILENT);
        let both = PacketFlags {
            silent: true,
            discontinuity: true,
        };
        assert_eq!(PacketFlags::from(&BufferFlags::new(3)), both);
        // Timestamp errors are not acted on.
        assert_eq!(PacketFlags::from(&BufferFlags::new(4)), PLAIN);
    }

    #[test]
    fn wasapi_errors_end_capture_as_device_lost() {
        let error = device_invalidated();
        let detail = error.to_string();
        assert_eq!(
            StreamError::from(error).into_exit(),
            CaptureExit::DeviceLost {
                hresult: 0x8889_0004_u32 as i32,
                detail,
            }
        );
        // A wasapi error without an HRESULT still means the stream is gone (E_FAIL).
        let error = WasapiError::DataLengthTooShort {
            received: 1,
            expected: 2,
        };
        let detail = error.to_string();
        assert_eq!(
            StreamError::from(error).into_exit(),
            CaptureExit::DeviceLost {
                hresult: 0x8000_4005_u32 as i32,
                detail,
            }
        );
        // Resampler errors are internal failures, not a lost device.
        assert_eq!(
            StreamError::from(ResampleError::InvalidRate(0)).into_exit(),
            CaptureExit::Failed("resampling failed: invalid input sample rate: 0 Hz".into())
        );
    }

    /// What the scripted source answers to one `next_packet_frames` poll.
    enum Poll {
        /// A queued packet of stereo `f32` frames.
        Packet(Vec<[f32; 2]>, PacketFlags),
        /// Nothing queued.
        Empty,
        /// One frame announced, but the read returns no frames (with these flags).
        ReadsEmpty(PacketFlags),
        /// One frame announced, but reading it fails with a device error.
        ReadFails,
    }

    /// A [`PacketSource`] replaying a script. Once the script is exhausted every poll
    /// fails with `AUDCLNT_E_DEVICE_INVALIDATED`, as when the device is unplugged.
    struct ScriptedSource {
        script: std::vec::IntoIter<Poll>,
        pending: Option<Poll>,
        ring: Arc<RingShared>,
        /// Frames the ring had received when each poll happened.
        written_at_poll: Vec<u64>,
    }

    impl ScriptedSource {
        fn new(script: Vec<Poll>, ring: &Arc<RingShared>) -> Self {
            Self {
                script: script.into_iter(),
                pending: None,
                ring: Arc::clone(ring),
                // Preallocated so polling does not allocate.
                written_at_poll: Vec::with_capacity(64),
            }
        }
    }

    impl PacketSource for ScriptedSource {
        fn next_packet_frames(&mut self) -> Result<usize, WasapiError> {
            self.written_at_poll.push(self.ring.written());
            let poll = self.script.next().ok_or_else(device_invalidated)?;
            let frames = match &poll {
                Poll::Packet(frames, _) => frames.len(),
                Poll::Empty => 0,
                Poll::ReadsEmpty(_) | Poll::ReadFails => 1,
            };
            self.pending = Some(poll);
            Ok(frames)
        }

        fn read_packet(&mut self, buffer: &mut [u8]) -> Result<(usize, PacketFlags), WasapiError> {
            match self.pending.take() {
                Some(Poll::Packet(frames, flags)) => {
                    let samples = frames.as_flattened();
                    let (slots, _) = buffer.as_chunks_mut::<4>();
                    for (dst, sample) in slots.iter_mut().zip(samples) {
                        *dst = sample.to_le_bytes();
                    }
                    Ok((frames.len(), flags))
                }
                Some(Poll::ReadsEmpty(flags)) => Ok((0, flags)),
                Some(Poll::ReadFails) => Err(device_invalidated()),
                Some(Poll::Empty) | None => Ok((0, PacketFlags::default())),
            }
        }
    }

    fn drain_loop(
        rate: u32,
        script: Vec<Poll>,
        ring: &Arc<RingShared>,
    ) -> DrainLoop<ScriptedSource> {
        DrainLoop::new(ScriptedSource::new(script, ring), converter(rate))
    }

    #[test]
    fn a_wake_drains_every_queued_packet_without_allocating() {
        let (mut producer, mut consumer) = ring::ring(4_096).unwrap();
        let script = vec![
            Poll::Packet(constant(0.25, 100), DISCONTINUITY),
            Poll::Packet(constant(0.5, 50), DISCONTINUITY),
            Poll::Packet(constant(0.5, 30), SILENT),
            Poll::Empty,
        ];
        let mut drain = drain_loop(44_100, script, producer.shared());
        let counter = AtomicU64::new(0);
        let before = crate::test_alloc::allocations();
        let drained = drain.wake(true, &mut producer, &counter).unwrap();
        assert_eq!(
            crate::test_alloc::allocations(),
            before,
            "the wake allocated"
        );
        assert_eq!(drained, 180);
        assert_eq!(drain.source.written_at_poll, [0, 100, 150, 180]);
        assert_eq!(counter.load(Ordering::Relaxed), 1, "first packet exempt");
        let shared = producer.shared();
        assert_eq!(shared.written(), 180);
        assert_eq!(shared.last_chunk(), 30);
        assert_eq!(shared.peak(), 0.0, "the last chunk was silent");
        let mut chunk = [[0i16; 2]; 100];
        assert!(consumer.pop_exact(&mut chunk));
        assert!(chunk.iter().all(|f| *f == [8_192, 8_192]));
        assert!(consumer.pop_exact(&mut chunk[..50]));
        assert!(chunk[..50].iter().all(|f| *f == [16_384, 16_384]));
        assert!(consumer.pop_exact(&mut chunk[..30]));
        assert!(chunk[..30].iter().all(|f| *f == [0, 0]));

        // Resampling and the idle reset do not allocate either.
        let (mut producer, _consumer) = ring::ring(4_096).unwrap();
        let script = vec![Poll::Packet(sine_48k(480), PLAIN), Poll::Empty, Poll::Empty];
        let mut drain = drain_loop(48_000, script, producer.shared());
        let before = crate::test_alloc::allocations();
        assert_eq!(drain.wake(true, &mut producer, &counter).unwrap(), 480);
        assert_eq!(drain.wake(false, &mut producer, &counter).unwrap(), 0);
        assert_eq!(
            crate::test_alloc::allocations(),
            before,
            "resampling allocated"
        );
        let polls = &drain.source.written_at_poll;
        assert!(polls[1] > 0, "{polls:?}");
        assert_eq!(
            producer.shared().written(),
            polls[2],
            "the idle wake publishes nothing"
        );
        assert!(!drain.converter.resampler.has_tail());
    }

    /// A packet that is announced but reads empty ends the drain instead of spinning;
    /// the next wake picks up what is still queued.
    #[test]
    fn a_packet_that_reads_empty_ends_the_drain() {
        let (mut producer, _consumer) = ring::ring(4_096).unwrap();
        let script = vec![
            Poll::Packet(constant(0.5, 10), PLAIN),
            Poll::ReadsEmpty(DISCONTINUITY),
            Poll::Packet(constant(0.5, 20), PLAIN),
            Poll::Empty,
        ];
        let mut drain = drain_loop(44_100, script, producer.shared());
        let counter = AtomicU64::new(0);
        assert_eq!(drain.wake(true, &mut producer, &counter).unwrap(), 10);
        assert_eq!(drain.source.written_at_poll.len(), 2);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(drain.wake(true, &mut producer, &counter).unwrap(), 20);
        assert_eq!(producer.shared().written(), 30);
    }

    /// The loop end to end: a signaled wake drains the packet, the idle timeouts
    /// publish nothing more (the first one resets the resampler), and a WASAPI error
    /// ends capture as `DeviceLost` with its HRESULT.
    #[test]
    fn run_resets_the_resampler_when_idle_and_ends_with_device_lost() {
        let (mut producer, _consumer) = ring::ring(4_096).unwrap();
        let script = vec![
            Poll::Packet(sine_48k(480), PLAIN), // wake 1 (signaled)
            Poll::Empty,                        // wake 1: queue drained
            Poll::Empty,                        // wake 2 (timeout): reset
            Poll::Empty,                        // wake 3 (timeout): nothing to reset
        ]; // wake 4: AUDCLNT_E_DEVICE_INVALIDATED
        let mut drain = drain_loop(48_000, script, producer.shared());
        let shared = Shared::new().unwrap();
        let packet_event = AutoEvent::new().unwrap();
        packet_event.set();
        let timeout = Duration::from_millis(5);
        let exit = drain.run(packet_event.handle(), &mut producer, &shared, timeout);
        assert_eq!(
            exit,
            CaptureExit::DeviceLost {
                hresult: AUDCLNT_E_DEVICE_INVALIDATED,
                detail: device_invalidated().to_string(),
            }
        );
        let polls = &drain.source.written_at_poll;
        assert_eq!(polls.len(), 5, "{polls:?}");
        let packet = polls[1];
        assert_eq!(polls[0], 0);
        assert!(packet > 0, "{polls:?}");
        assert_eq!(
            polls[2..],
            [packet; 3],
            "nothing is published after the packet: {polls:?}"
        );
        assert_eq!(producer.shared().written(), packet);
        assert!(
            !drain.converter.resampler.has_tail(),
            "the idle timeout reset it"
        );
        assert_eq!(shared.discontinuities.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn read_errors_end_capture_as_device_lost() {
        let (mut producer, _consumer) = ring::ring(4_096).unwrap();
        let script = vec![Poll::Packet(constant(0.5, 10), PLAIN), Poll::ReadFails];
        let mut drain = drain_loop(44_100, script, producer.shared());
        let shared = Shared::new().unwrap();
        let packet_event = AutoEvent::new().unwrap();
        packet_event.set();
        let exit = drain.run(packet_event.handle(), &mut producer, &shared, WAIT_TIMEOUT);
        assert!(
            matches!(
                exit,
                CaptureExit::DeviceLost {
                    hresult: AUDCLNT_E_DEVICE_INVALIDATED,
                    ..
                }
            ),
            "{exit:?}"
        );
        assert_eq!(
            producer.shared().written(),
            10,
            "audio before the error is published"
        );
        assert_eq!(drain.source.written_at_poll.len(), 2);
    }

    #[test]
    fn a_stop_request_ends_the_loop_before_polling() {
        let (mut producer, _consumer) = ring::ring(4_096).unwrap();
        let script = vec![Poll::Packet(sine_48k(480), PLAIN)];
        let mut drain = drain_loop(48_000, script, producer.shared());
        let shared = Shared::new().unwrap();
        let packet_event = AutoEvent::new().unwrap();
        packet_event.set();
        shared.request_stop();
        let exit = drain.run(packet_event.handle(), &mut producer, &shared, WAIT_TIMEOUT);
        assert_eq!(exit, CaptureExit::Stopped);
        assert!(drain.source.written_at_poll.is_empty());
        assert_eq!(producer.shared().written(), 0);
    }
}
