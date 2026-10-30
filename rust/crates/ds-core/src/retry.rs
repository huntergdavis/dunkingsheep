//! Bounded retry with backoff (T02).
//!
//! The tree retries three things by hand: the socket connect (a 20-agent burst overflows a
//! short listen backlog), the guarded send ladder, and the delivery recovery. Each has a
//! bounded attempt count and a short sleep - never an unbounded loop, because a retry that
//! cannot give up is how a lane hangs instead of reporting.
//!
//! This is the one shape they share, so a fourth caller does not invent a fifth policy.

use std::time::Duration;

/// How many attempts and how long to wait between them.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, including the first. `1` means "do not retry".
    pub attempts: u32,
    /// The first wait; each later wait multiplies by `backoff`.
    pub base_delay: Duration,
    /// Multiplier applied to the wait after each failure (`1.0` = constant).
    pub backoff: f64,
    /// The wait never grows past this.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // The socket connect's numbers: three attempts, 50 ms doubling, capped at 200 ms.
        RetryPolicy {
            attempts: 3,
            base_delay: Duration::from_millis(50),
            backoff: 2.0,
            max_delay: Duration::from_millis(200),
        }
    }
}

impl RetryPolicy {
    /// The wait before attempt `attempt` (1-based).
    pub fn delay(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let mut delay = self.base_delay;
        for _ in 1..attempt - 1 {
            delay = delay.mul_f64(self.backoff);
            if delay >= self.max_delay {
                return self.max_delay;
            }
        }
        delay.min(self.max_delay)
    }
}

/// Run `operation` until it succeeds, the attempts run out, or `retryable` says stop.
///
/// `retryable` decides which errors are worth another attempt: a refused connection is,
/// a "command already ran" is not, and a port that retried everything would duplicate work.
pub fn retry<T, E, F, R>(policy: RetryPolicy, mut operation: F, retryable: R) -> Result<T, E>
where
    F: FnMut(u32) -> Result<T, E>,
    R: Fn(&E) -> bool,
{
    let attempts = policy.attempts.max(1);
    let mut last: Option<E> = None;
    for attempt in 1..=attempts {
        let delay = policy.delay(attempt);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        match operation(attempt) {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !retryable(&error) || attempt == attempts {
                    return Err(error);
                }
                last = Some(error);
            }
        }
    }
    Err(last.expect("the loop either returns or records an error"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_policy_matches_the_socket_connect_numbers() {
        let policy = RetryPolicy::default();
        assert_eq!(3, policy.attempts);
        assert_eq!(Duration::ZERO, policy.delay(1));
        assert_eq!(Duration::from_millis(50), policy.delay(2));
        assert_eq!(Duration::from_millis(100), policy.delay(3));
        assert_eq!(Duration::from_millis(200), policy.delay(4));
        assert_eq!(Duration::from_millis(200), policy.delay(9), "capped");
    }

    #[test]
    fn it_stops_after_the_attempts_are_spent() {
        let policy = RetryPolicy {
            base_delay: Duration::ZERO,
            ..RetryPolicy::default()
        };
        let mut calls = 0;
        let result: Result<(), &str> = retry(
            policy,
            |_attempt| {
                calls += 1;
                Err("still refused")
            },
            |_| true,
        );
        assert_eq!(Err("still refused"), result);
        assert_eq!(3, calls, "bounded, not an unbounded loop");
    }

    #[test]
    fn it_does_not_retry_an_error_the_caller_calls_permanent() {
        let policy = RetryPolicy {
            base_delay: Duration::ZERO,
            ..RetryPolicy::default()
        };
        let mut calls = 0;
        let result: Result<(), &str> = retry(
            policy,
            |_attempt| {
                calls += 1;
                Err("already ran")
            },
            |error| *error != "already ran",
        );
        assert_eq!(Err("already ran"), result);
        assert_eq!(1, calls, "a write that may have run is never repeated");
    }

    #[test]
    fn it_returns_the_first_success() {
        let policy = RetryPolicy {
            base_delay: Duration::ZERO,
            ..RetryPolicy::default()
        };
        let mut calls = 0;
        let result: Result<u32, &str> = retry(
            policy,
            |attempt| {
                calls += 1;
                if attempt < 2 {
                    Err("not yet")
                } else {
                    Ok(attempt)
                }
            },
            |_| true,
        );
        assert_eq!(Ok(2), result);
        assert_eq!(2, calls);
    }
}
