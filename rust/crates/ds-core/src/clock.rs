//! Time (T02): one place that reads the clock, so tests can move it.
//!
//! The Python uses `time.time()` in 140 places and `time.monotonic()` for durations.
//! A port that calls `SystemTime::now()` inline cannot be tested, so every module takes
//! a `Clock`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A source of wall-clock seconds, as `time.time()` returns them.
pub trait Clock: Send + Sync {
    /// Seconds since the unix epoch, with the sub-second part.
    fn now(&self) -> f64;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }
}

/// A clock a test moves by hand.
#[derive(Debug, Default, Clone)]
pub struct FixedClock {
    at: std::sync::Arc<std::sync::Mutex<f64>>,
}

impl FixedClock {
    /// A clock stopped at `at`.
    pub fn at(at: f64) -> Self {
        FixedClock {
            at: std::sync::Arc::new(std::sync::Mutex::new(at)),
        }
    }

    /// Move it forward, as a test's `time.sleep` would.
    pub fn advance(&self, seconds: f64) {
        if let Ok(mut at) = self.at.lock() {
            *at += seconds;
        }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> f64 {
        self.at.lock().map(|at| *at).unwrap_or(0.0)
    }
}

/// `human_age(seconds)` as the CLI prints it: `12s`, `5m`, `3h`, `2d`.
///
/// The fleet reads these in status lines and task lists, so the shape is part of the
/// interface, not cosmetics.
pub fn human_age(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    let s = seconds as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// A duration as whole seconds, floored at zero.
pub fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64().max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixed_clock_moves_only_when_told() {
        let clock = FixedClock::at(1_000.0);
        assert_eq!(1_000.0, clock.now());
        clock.advance(30.0);
        assert_eq!(1_030.0, clock.now());
    }

    #[test]
    fn ages_render_the_way_the_status_lines_read() {
        assert_eq!("12s", human_age(12.0));
        assert_eq!("5m", human_age(300.0));
        assert_eq!("3h", human_age(3.0 * 3600.0));
        assert_eq!("2d", human_age(2.0 * 86_400.0));
        assert_eq!("0s", human_age(-5.0));
    }
}
