//! One backoff, for socket connects and SQLite busy.
//!
//! Ported from the ad-hoc `attempt` counters that appear 300+ times in the Python
//! tree (RUST_PORT.md section 6). The rule the fleet learned the hard way
//! (t705/t693): a retry must be about IDEMPOTENCY, and the caller decides that -
//! this module never decides for you.

use std::time::Duration;

use crate::error::DunkError;

/// How many attempts, and how long to wait between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Backoff {
    /// `attempts` tries in total, doubling from `base`, never longer than `cap`.
    pub fn new(attempts: u32, base: Duration, cap: Duration) -> Self {
        Backoff { attempts: attempts.max(1), base, cap }
    }

    /// The short default: three tries, 20 ms doubling, 500 ms cap.
    ///
    /// Chosen to fit inside the client's deadline, not to hide a slow host: the
    /// accepted fix for a saturated box is patience (a longer deadline), not more
    /// retries (t693).
    pub fn short() -> Self {
        Backoff::new(3, Duration::from_millis(20), Duration::from_millis(500))
    }

    /// The wait before attempt `attempt` (1-based: the first attempt waits nothing).
    pub fn delay(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let factor = 1u32 << (attempt - 2).min(16);
        let wait = self.base.saturating_mul(factor);
        wait.min(self.cap)
    }
}

/// Run `body` up to `backoff.attempts` times, retrying only when `retryable` says so.
///
/// `retryable` is a parameter rather than a property of the error on purpose: the
/// same error can be safe to repeat for a read and never for a write (the client's
/// IDEMPOTENT_READS rule, t705).
pub fn with_backoff<T, F, R>(backoff: Backoff, mut retryable: R, mut body: F) -> Result<T, DunkError>
where
    F: FnMut(u32) -> Result<T, DunkError>,
    R: FnMut(&DunkError) -> bool,
{
    let mut last: Option<DunkError> = None;
    for attempt in 1..=backoff.attempts {
        let wait = backoff.delay(attempt);
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        match body(attempt) {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !retryable(&error) || attempt == backoff.attempts {
                    return Err(error);
                }
                last = Some(error);
            }
        }
    }
    Err(last.unwrap_or_else(|| DunkError::Refused("retry loop ended without an attempt".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_from_the_base_and_stop_at_the_cap() {
        let backoff = Backoff::new(6, Duration::from_millis(10), Duration::from_millis(35));
        assert_eq!(Duration::ZERO, backoff.delay(1));
        assert_eq!(Duration::from_millis(10), backoff.delay(2));
        assert_eq!(Duration::from_millis(20), backoff.delay(3));
        assert_eq!(Duration::from_millis(35), backoff.delay(4), "capped");
        assert_eq!(Duration::from_millis(35), backoff.delay(9));
    }

    #[test]
    fn a_retryable_error_is_retried_and_the_last_one_is_returned() {
        let backoff = Backoff::new(3, Duration::from_millis(1), Duration::from_millis(5));
        let mut attempts = Vec::new();
        let error: DunkError = with_backoff(
            backoff,
            |error: &DunkError| error.retryable(),
            |attempt| -> Result<(), DunkError> {
                attempts.push(attempt);
                Err(DunkError::Busy)
            },
        )
        .unwrap_err();
        assert_eq!(vec![1, 2, 3], attempts);
        assert!(error.retryable());
    }

    #[test]
    fn a_non_retryable_error_is_returned_immediately() {
        let backoff = Backoff::new(5, Duration::from_millis(1), Duration::from_millis(5));
        let mut attempts = 0;
        let error: DunkError = with_backoff(
            backoff,
            |error: &DunkError| error.retryable(),
            |_| -> Result<(), DunkError> {
                attempts += 1;
                Err(DunkError::Refused("not permitted for role".into()))
            },
        )
        .unwrap_err();
        assert_eq!(1, attempts, "a refusal must not be retried");
        assert_eq!("not permitted for role", error.to_string());
    }

    #[test]
    fn a_later_attempt_can_succeed() {
        let backoff = Backoff::new(3, Duration::from_millis(1), Duration::from_millis(5));
        let mut attempts = 0;
        let value: u32 = with_backoff(
            backoff,
            |error: &DunkError| error.retryable(),
            |_| -> Result<u32, DunkError> {
                attempts += 1;
                if attempts < 3 {
                    Err(DunkError::Retryable("connect refused".into()))
                } else {
                    Ok(attempts)
                }
            },
        )
        .unwrap();
        assert_eq!(3, value);
    }

    #[test]
    fn the_decider_is_the_callers_not_the_errors() {
        // The t705 rule: the same error is retryable for a read, never for a write.
        let backoff = Backoff::new(3, Duration::from_millis(1), Duration::from_millis(5));
        let mut write_attempts = 0;
        let _: Result<(), DunkError> = with_backoff(backoff, |_| false, |_| -> Result<(), DunkError> {
            write_attempts += 1;
            Err(DunkError::Busy)
        });
        assert_eq!(1, write_attempts);
    }
}
