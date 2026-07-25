//! A small token-bucket rate limiter for bandwidth caps.
//!
//! Peer tasks call [`RateLimiter::acquire`] with the payload size before
//! processing it; when a cap is configured the call sleeps just long enough
//! to keep the average at the limit. `None` means unlimited.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    /// bytes/second; `None` = unlimited.
    limit: Option<u64>,
    bucket: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(limit: Option<u64>) -> Self {
        RateLimiter {
            limit: limit.filter(|&l| l > 0),
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                last_refill: Instant::now(),
            }),
        }
    }

    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// Wait until `bytes` fit within the configured budget.
    pub async fn acquire(&self, bytes: u64) {
        let Some(limit) = self.limit else { return };
        let limit = limit as f64;
        // Allow short bursts of up to half a second of budget.
        let capacity = (limit / 2.0).max(f64::from(crate::picker::BLOCK_SIZE));

        loop {
            let wait = {
                let mut bucket = self.bucket.lock().unwrap();
                let now = Instant::now();
                let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
                bucket.tokens = (bucket.tokens + elapsed * limit).min(capacity);
                bucket.last_refill = now;

                if bucket.tokens >= bytes as f64 {
                    bucket.tokens -= bytes as f64;
                    return;
                }
                let missing = bytes as f64 - bucket.tokens;
                Duration::from_secs_f64((missing / limit).clamp(0.005, 1.0))
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_never_waits() {
        let limiter = RateLimiter::new(None);
        let start = Instant::now();
        for _ in 0..1000 {
            limiter.acquire(1 << 20).await;
        }
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn limit_slows_throughput() {
        // 1 MiB/s budget, ask for ~1.5 MiB beyond the burst: must take time.
        let limiter = RateLimiter::new(Some(1024 * 1024));
        let start = Instant::now();
        let mut transferred = 0u64;
        while transferred < 2 * 1024 * 1024 {
            limiter.acquire(64 * 1024).await;
            transferred += 64 * 1024;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1200),
            "2 MiB at 1 MiB/s finished too fast: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn zero_limit_means_unlimited() {
        let limiter = RateLimiter::new(Some(0));
        assert_eq!(limiter.limit(), None);
        limiter.acquire(u64::MAX / 2).await;
    }
}
