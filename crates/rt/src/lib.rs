//! Windows real-time building blocks shared by the audio and AirPlay crates.
//!
//! Everything a time-critical thread needs lives here so the other crates never
//! touch raw Win32 timing APIs:
//!
//! * [`clock::mono_ns`] — one monotonic nanosecond clock (QueryPerformanceCounter
//!   based) used for capture timestamps, PTP receive times, the send timeline and
//!   anchors, so all of them share a single clock domain;
//! * [`event::AutoEvent`], [`timer::HiResTimer`] and [`wait::wait_any`] — event-driven
//!   wake-ups that do not depend on the system timer resolution (Windows 11 ignores
//!   `timeBeginPeriod` for hidden windows, which is WinPods' normal state);
//! * [`mmcss::MmcssGuard`] — registers a thread with MMCSS ("Pro Audio");
//! * [`power::ThrottleGuard`] — opts the process out of EcoQoS / timer coalescing
//!   while streaming;
//! * [`ring`] — the single-producer/single-consumer audio ring between the capture
//!   thread and the sender thread.

pub mod clock;
pub mod event;
pub mod mmcss;
pub mod power;
pub mod ring;
pub mod timer;
pub mod wait;

pub use clock::mono_ns;
pub use event::AutoEvent;
pub use mmcss::{MmcssGuard, ThreadPriority, set_current_thread_priority};
pub use power::ThrottleGuard;
pub use ring::{Frame, RingConsumer, RingProducer, RingShared, ring};
pub use timer::HiResTimer;
pub use wait::{Wait, wait_any};

/// Re-export so dependants can name handles without depending on `windows` directly.
pub use windows::Win32::Foundation::HANDLE;
