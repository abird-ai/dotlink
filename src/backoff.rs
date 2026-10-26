use std::time::Duration;

/// Exponential backoff with an optional jitter band.
///
/// `delay(attempt)` doubles `base` from attempt 1, clamps to `cap`, then adds
/// uniform jitter in `0..=jitter`. Attempts past the cap cost the same, so a
/// caller can keep incrementing without wrapping.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    /// `Duration::ZERO` for a fully deterministic schedule.
    pub jitter: Duration,
}

impl Backoff {
    pub(crate) fn delay(&self, attempt: u32) -> Duration {
        // `checked_shl` turns an absurd attempt count into the cap instead of
        // panicking on a shift overflow.
        let shift = 1_u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        let step = self.base.saturating_mul(shift).min(self.cap);
        if self.jitter.is_zero() {
            return step;
        }
        let jitter_nanos = self.jitter.as_nanos().min(u64::MAX as u128) as u64;
        step.saturating_add(Duration::from_nanos(fastrand::u64(0..=jitter_nanos)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_doubles_from_the_base_up_to_the_cap() {
        let backoff = Backoff {
            base: Duration::from_millis(250),
            cap: Duration::from_secs(5),
            jitter: Duration::from_millis(250),
        };
        let band = |attempt: u32, floor: u64, ceiling: u64| {
            let delay = backoff.delay(attempt).as_millis() as u64;
            assert!(
                (floor..=ceiling).contains(&delay),
                "attempt {attempt}: {delay} ms"
            );
        };
        band(0, 250, 500);
        band(1, 250, 500);
        band(2, 500, 750);
        band(3, 1_000, 1_250);
        band(10, 5_000, 5_250);
        band(100, 5_000, 5_250);
        band(u32::MAX, 5_000, 5_250);
    }

    #[test]
    fn cap_is_reachable_however_wide_it_is() {
        let wide = Backoff {
            base: Duration::from_millis(100),
            cap: Duration::from_secs(60),
            jitter: Duration::ZERO,
        };
        assert_eq!(wide.delay(1), Duration::from_millis(100));
        assert_eq!(wide.delay(10), Duration::from_millis(51_200));
        assert_eq!(wide.delay(11), Duration::from_secs(60));
    }

    #[test]
    fn sub_millisecond_jitter_is_not_silently_zero() {
        let backoff = Backoff {
            base: Duration::from_millis(100),
            cap: Duration::from_secs(1),
            jitter: Duration::from_nanos(1),
        };
        let delays: Vec<u64> = (0..100)
            .map(|_| backoff.delay(1).as_nanos() as u64)
            .collect();
        assert!(delays.iter().any(|delay| *delay > 100_000_000));
    }

    #[test]
    fn zero_jitter_is_deterministic() {
        let backoff = Backoff {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(30),
            jitter: Duration::ZERO,
        };
        let delays: Vec<u64> = (1..=7).map(|a| backoff.delay(a).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
    }
}
