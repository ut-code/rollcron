use rand::Rng;
use std::time::Duration;

use crate::config::RetryConfig;

/// Default jitter ratio when not explicitly configured (25% of base delay)
const AUTO_JITTER_RATIO: u32 = 25;

pub fn calculate_backoff(retry: &RetryConfig, attempt: u32) -> Duration {
    let base_delay = retry.delay.saturating_mul(2u32.saturating_pow(attempt));
    let jitter_max =
        retry.jitter.unwrap_or_else(|| retry.delay.saturating_mul(AUTO_JITTER_RATIO) / 100);
    base_delay.saturating_add(generate_jitter(jitter_max))
}

pub fn generate_jitter(max: Duration) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let millis = max.as_millis();
    if millis == 0 {
        return Duration::ZERO;
    }
    let jitter_millis = rand::thread_rng().gen_range(0..=millis);
    Duration::from_millis(jitter_millis as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_backoff_calculation() {
        let retry = RetryConfig {
            max: 5,
            delay: Duration::from_secs(1),
            jitter: None,
        };
        let backoff_0 = calculate_backoff(&retry, 0);
        assert!(backoff_0 >= Duration::from_secs(1));
        assert!(backoff_0 <= Duration::from_millis(1250));

        let backoff_1 = calculate_backoff(&retry, 1);
        assert!(backoff_1 >= Duration::from_secs(2));
        assert!(backoff_1 <= Duration::from_millis(2250));

        let backoff_2 = calculate_backoff(&retry, 2);
        assert!(backoff_2 >= Duration::from_secs(4));
        assert!(backoff_2 <= Duration::from_millis(4250));

        let backoff_3 = calculate_backoff(&retry, 3);
        assert!(backoff_3 >= Duration::from_secs(8));
        assert!(backoff_3 <= Duration::from_millis(8250));
    }

    #[test]
    fn generate_jitter_bounds() {
        let max = Duration::from_millis(100);
        for _ in 0..10 {
            let jitter = generate_jitter(max);
            assert!(jitter <= max);
        }
    }

    #[test]
    fn generate_jitter_zero() {
        let jitter = generate_jitter(Duration::ZERO);
        assert_eq!(jitter, Duration::ZERO);
    }

    #[test]
    fn backoff_with_jitter() {
        let retry = RetryConfig {
            max: 3,
            delay: Duration::from_secs(1),
            jitter: Some(Duration::from_millis(500)),
        };

        for _ in 0..5 {
            let backoff = calculate_backoff(&retry, 0);
            assert!(backoff >= Duration::from_secs(1));
            assert!(backoff <= Duration::from_millis(1500));
        }
    }

    #[test]
    fn backoff_auto_inferred_jitter() {
        let retry = RetryConfig {
            max: 3,
            delay: Duration::from_secs(10),
            jitter: None,
        };

        for _ in 0..10 {
            let backoff = calculate_backoff(&retry, 0);
            assert!(backoff >= Duration::from_secs(10));
            assert!(backoff <= Duration::from_millis(12500));
        }

        for _ in 0..10 {
            let backoff = calculate_backoff(&retry, 1);
            assert!(backoff >= Duration::from_secs(20));
            assert!(backoff <= Duration::from_millis(22500));
        }
    }
}
