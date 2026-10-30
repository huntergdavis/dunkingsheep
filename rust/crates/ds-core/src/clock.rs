//! Time: one place to fake it, and the two formats the wire uses.
//!
//! The Python calls `time.time()` in 145 places inside the daemon tree; in Rust
//! they become [`now_s`] so a test can replace the clock instead of sleeping.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch as `f64`, the Python's `time.time()`.
pub fn now_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Whole seconds since the epoch, for lease arithmetic.
pub fn now_u64() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A monotonic instant, for measuring how long something took.
///
/// `Instant` ignores wall-clock jumps (NTP, suspend), which is why a duration is
/// measured with it and stored as seconds only in the event row.
pub fn stamp() -> std::time::Instant {
    std::time::Instant::now()
}

/// Seconds elapsed since a [`stamp`].
pub fn elapsed_s(stamp: std::time::Instant) -> f64 {
    stamp.elapsed().as_secs_f64()
}

/// Milliseconds elapsed since a [`stamp`], as the event rows record it.
pub fn elapsed_ms(stamp: std::time::Instant) -> i64 {
    stamp.elapsed().as_millis() as i64
}

/// An epoch-seconds value as the ISO-8601 UTC string the stores and GitHub use.
pub fn iso_from_s(seconds: f64) -> String {
    let whole = seconds.max(0.0) as i64;
    let nanos = ((seconds.max(0.0) - whole as f64) * 1e9) as u32;
    chrono::DateTime::from_timestamp(whole, nanos.min(999_999_999))
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Now, in that format.
pub fn iso_now() -> String {
    iso_from_s(now_s())
}

/// A duration in whole seconds, rounding up (the Python's `ceil` for leases).
pub fn ceil_s(duration: Duration) -> u64 {
    if duration.subsec_nanos() == 0 {
        duration.as_secs()
    } else {
        duration.as_secs() + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_renders_as_iso_utc() {
        assert_eq!("1970-01-01T00:00:00Z", iso_from_s(0.0));
        assert_eq!("2026-10-30T08:00:00Z", iso_from_s(1793347200.0));
    }

    #[test]
    fn a_fractional_second_is_truncated_not_rounded() {
        assert_eq!(iso_from_s(0.9), iso_from_s(0.0));
    }

    #[test]
    fn now_is_a_plausible_epoch_and_increases() {
        let first = now_s();
        assert!(first > 1_700_000_000.0, "clock is before 2023: {first}");
        let second = now_s();
        assert!(second >= first);
        assert_eq!(now_u64() as f64, now_u64() as f64);
    }

    #[test]
    fn elapsed_is_measured_and_ceil_rounds_up() {
        let started = stamp();
        std::thread::sleep(Duration::from_millis(5));
        assert!(elapsed_ms(started) >= 5);
        assert!(elapsed_s(started) > 0.0);
        assert_eq!(1, ceil_s(Duration::from_millis(1)));
        assert_eq!(2, ceil_s(Duration::from_secs(2)));
    }

    #[test]
    fn iso_now_is_the_shape_github_uses() {
        let text = iso_now();
        assert_eq!(20, text.len(), "{text}");
        assert!(text.ends_with('Z') && text.contains('T'), "{text}");
    }
}
