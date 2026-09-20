//! Time, injected.
//!
//! Nothing outside this module may call `Instant::now` or `SystemTime::now`. Two reasons:
//! the scheduler becomes testable without sleeping, and the monotonic/wall distinction stays
//! honest. Intervals (stall, backoff) are monotonic so an NTP step cannot fire them early;
//! wall time is for display and for `retry.due_at`, which has to survive a restart.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Monotonic milliseconds since an arbitrary origin. Only differences are meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mono(pub u64);

impl Mono {
    pub fn saturating_since(self, earlier: Mono) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Milliseconds since the Unix epoch. Safe to persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wall(pub i64);

pub trait Clock: Send + Sync + 'static {
    fn mono(&self) -> Mono;
    fn wall(&self) -> Wall;
}

pub struct SystemClock {
    origin: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { origin: std::time::Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn mono(&self) -> Mono {
        Mono(self.origin.elapsed().as_millis() as u64)
    }

    fn wall(&self) -> Wall {
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Wall(d.as_millis() as i64)
    }
}

/// Test clock. Time only moves when a test moves it.
pub struct FakeClock {
    inner: Mutex<(u64, i64)>,
}

impl FakeClock {
    pub fn new() -> Self {
        // A non-zero wall origin keeps timestamps recognisable in test output.
        Self { inner: Mutex::new((0, 1_770_000_000_000)) }
    }

    pub fn advance_ms(&self, ms: u64) {
        let mut g = self.inner.lock().unwrap();
        g.0 = g.0.saturating_add(ms);
        g.1 = g.1.saturating_add(ms as i64);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn mono(&self) -> Mono {
        Mono(self.inner.lock().unwrap().0)
    }

    fn wall(&self) -> Wall {
        Wall(self.inner.lock().unwrap().1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_advances_both_scales_together() {
        let c = FakeClock::new();
        let m0 = c.mono();
        let w0 = c.wall();
        c.advance_ms(1_500);
        assert_eq!(c.mono().saturating_since(m0), 1_500);
        assert_eq!(c.wall().0 - w0.0, 1_500);
    }

    #[test]
    fn mono_difference_saturates_rather_than_wrapping() {
        // Guards against a reordered comparison underflowing into a huge elapsed value,
        // which would read as an instant stall.
        assert_eq!(Mono(5).saturating_since(Mono(9)), 0);
    }
}
