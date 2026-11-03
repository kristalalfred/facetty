use std::time::{Duration, Instant};

/// Claude: a token bucket of up to `burst` events, refilled at `per_second`.
#[derive(Debug)]
pub struct Rate {
    per_second: f64,
    burst: f64,
    tokens: f64,
    last: Option<Instant>,
}

impl Rate {
    pub fn new(per_second: f64, burst: f64) -> Self {
        Self {
            per_second,
            burst,
            tokens: burst,
            last: None,
        }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        self.tokens = self.tokens_at(now);
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Claude: how long from `now` until [`allow`](Self::allow) succeeds.
    pub fn ready_in(&self, now: Instant) -> Duration {
        let missing = 1.0 - self.tokens_at(now);
        if missing <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(missing / self.per_second)
        }
    }

    fn tokens_at(&self, now: Instant) -> f64 {
        let elapsed = self.last.map_or(0.0, |last| {
            now.saturating_duration_since(last).as_secs_f64()
        });
        (self.tokens + elapsed * self.per_second).min(self.burst)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn allows_a_burst_then_the_refill_rate() {
        let start = Instant::now();
        let mut rate = Rate::new(2.0, 3.0);
        let allowed = (0..10).filter(|_| rate.allow(start)).count();
        assert_eq!(allowed, 3);
        assert!(!rate.allow(start + Duration::from_millis(400)));
        assert!(rate.allow(start + Duration::from_millis(500)));
        assert_eq!(
            rate.ready_in(start + Duration::from_millis(500)),
            Duration::from_millis(500)
        );
        let later = start + Duration::from_secs(60);
        assert_eq!(rate.ready_in(later), Duration::ZERO);
        let allowed = (0..10).filter(|_| rate.allow(later)).count();
        assert_eq!(allowed, 3);
    }
}
