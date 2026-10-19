//! The device-wide failure backoff (protocol section 4): after the n-th
//! consecutive failure the next attempt is refused for `min(2^(n-1), 300)`
//! seconds; a success resets the count.

use std::time::{Duration, Instant};

const CAP_SECS: u64 = 300;

#[derive(Debug, Default)]
pub struct Backoff {
    failures: u32,
    until: Option<Instant>,
}

impl Backoff {
    /// `Err(seconds)` while attempts are refused, rounded up.
    pub fn check(&self, now: Instant) -> Result<(), u64> {
        match self.until {
            Some(until) if until > now => {
                let left = until - now;
                Err(left.as_secs() + u64::from(left.subsec_nanos() > 0))
            }
            _ => Ok(()),
        }
    }

    pub fn fail(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let secs = 1u64
            .checked_shl(self.failures - 1)
            .map_or(CAP_SECS, |s| s.min(CAP_SECS));
        self.until = Some(now + Duration::from_secs(secs));
    }

    pub fn succeed(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_doubles_to_the_cap_and_resets() {
        let t = Instant::now();
        let mut b = Backoff::default();
        assert_eq!(b.check(t), Ok(()));
        let expected = [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300];
        for secs in expected {
            b.fail(t);
            assert_eq!(b.check(t), Err(secs));
        }
        for _ in 0..100 {
            b.fail(t);
        }
        assert_eq!(b.check(t), Err(300));
        assert_eq!(b.check(t + Duration::from_secs(300)), Ok(()));
        assert_eq!(b.check(t + Duration::from_millis(299_500)), Err(1));
        b.succeed();
        assert_eq!(b.check(t), Ok(()));
        b.fail(t);
        assert_eq!(b.check(t), Err(1));
    }
}
