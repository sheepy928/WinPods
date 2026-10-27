//! A single monotonic clock for the whole process.

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

/// Monotonic nanoseconds since the first use of this clock in the process.
///
/// Backed by `std::time::Instant`, which is QueryPerformanceCounter on Windows. The
/// value fits a `u64` for ~584 years, so callers never need to handle wrap-around.
pub fn mono_ns() -> u64 {
    epoch().elapsed().as_nanos() as u64
}

/// Convert an `Instant` to the [`mono_ns`] timescale (0 for instants before the epoch).
pub fn instant_ns(t: Instant) -> u64 {
    t.checked_duration_since(epoch())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Pin the epoch early (call at process start) so values stay small and comparable.
pub fn init() {
    let _ = epoch();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn monotonic_and_advancing() {
        init();
        let a = mono_ns();
        std::thread::sleep(Duration::from_millis(5));
        let b = mono_ns();
        assert!(b > a);
        assert!(b - a >= 4_000_000, "advanced only {} ns", b - a);
    }

    #[test]
    fn instant_conversion_matches() {
        init();
        let now = Instant::now();
        let ns = instant_ns(now);
        let direct = mono_ns();
        assert!(direct >= ns);
        assert!(direct - ns < 50_000_000);
    }
}
