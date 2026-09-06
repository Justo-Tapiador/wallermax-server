//! Per-client-IP rate limiting using the token bucket algorithm.
//!
//! Each client IP owns a bucket with `capacity` tokens. Requests consume one
//! token; tokens regenerate at `refill_per_second`. When the bucket is empty
//! the request is rejected with `429 Too Many Requests` and a `Retry-After`
//! hint (see [`crate::middleware::rate_limit`]).
//!
//! Buckets are kept in a mutex-protected hash map shared by all workers
//! through [`AppState`](crate::state::AppState). Idle buckets are swept
//! opportunistically once the map exceeds [`MAX_TRACKED_IPS`] entries, so
//! memory stays bounded under address-spoofing pressure (an IP that has been
//! idle long enough for its bucket to refill carries no meaningful state).
//!
//! Note: the client IP is the TCP peer address. When running behind a
//! reverse proxy, all traffic appears to come from the proxy; trusted
//! `X-Forwarded-For` support is planned for a later phase.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

/// Maximum number of tracked client IPs before stale buckets are swept.
///
/// Roughly 10k entries occupy a few hundred kilobytes; exceeding this
/// triggers a sweep and, in the pathological case where every entry is
/// still active, a reset of the tracking map.
const MAX_TRACKED_IPS: usize = 10_000;

/// Shared, thread-safe token-bucket rate limiter keyed by client IP.
pub struct RateLimiter {
    capacity: f64,
    refill_per_second: f64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

/// Outcome of an acquisition attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    /// The request is allowed; carries the remaining tokens.
    Allowed { remaining: f64 },
    /// The request must be rejected; carries the wait time for one token.
    Rejected { retry_after_secs: u64 },
}

/// Per-IP token bucket state.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Creates a limiter with the given burst capacity and refill rate.
    pub fn new(capacity: u64, refill_per_second: f64) -> Self {
        Self {
            capacity: capacity as f64,
            refill_per_second,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Bucket capacity, as an integer (used for response headers).
    pub fn capacity(&self) -> u64 {
        self.capacity as u64
    }

    /// Tries to consume one token for `ip` at the current instant.
    pub fn try_acquire(&self, ip: IpAddr) -> Decision {
        self.try_acquire_at(ip, Instant::now())
    }

    /// Tries to consume one token for `ip` at a given instant.
    ///
    /// The instant is a parameter so the bucket math can be unit-tested
    /// deterministically without sleeping.
    pub fn try_acquire_at(&self, ip: IpAddr, now: Instant) -> Decision {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if !buckets.contains_key(&ip) && buckets.len() >= MAX_TRACKED_IPS {
            sweep_stale(&mut buckets, self.capacity, self.refill_per_second);
            if buckets.len() >= MAX_TRACKED_IPS {
                // Every tracked IP is still active; reset tracking rather
                // than grow without bound (extremely unlikely in practice).
                tracing::warn!(
                    tracked = buckets.len(),
                    "rate limiter map reset: too many simultaneously active client IPs"
                );
                buckets.clear();
            }
        }

        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: self.capacity,
            last_refill: now,
        });

        bucket.try_acquire(now, self.capacity, self.refill_per_second)
    }

    /// Number of currently tracked client IPs (observability/testing).
    pub fn tracked_ips(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl Bucket {
    /// Refills the bucket for the elapsed time and consumes one token.
    fn try_acquire(&mut self, now: Instant, capacity: f64, refill_per_second: f64) -> Decision {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * refill_per_second).min(capacity);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Decision::Allowed {
                remaining: self.tokens,
            }
        } else {
            let seconds_until_token = (1.0 - self.tokens) / refill_per_second;
            // `Retry-After` is an integer number of seconds; always hint at
            // least one so clients back off measurably.
            let retry_after_secs = seconds_until_token.ceil().max(1.0) as u64;
            Decision::Rejected { retry_after_secs }
        }
    }
}

/// Removes buckets that have fully refilled by now (no useful state left).
fn sweep_stale(buckets: &mut HashMap<IpAddr, Bucket>, capacity: f64, refill_per_second: f64) {
    // A bucket that would be completely full by now (tokens plus everything
    // regenerated while idle) is indistinguishable from a fresh one.
    let now = Instant::now();

    buckets.retain(|_, bucket| {
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens + elapsed * refill_per_second < capacity
    });
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::*;

    const IP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn limiter() -> RateLimiter {
        RateLimiter::new(3, 1.0)
    }

    fn later(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn allows_bursts_up_to_capacity() {
        let limiter = limiter();
        let t0 = Instant::now();

        assert_eq!(
            limiter.try_acquire_at(IP, t0),
            Decision::Allowed { remaining: 2.0 }
        );
        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 0)),
            Decision::Allowed { remaining: 1.0 }
        );
        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 0)),
            Decision::Allowed { remaining: 0.0 }
        );
        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 0)),
            Decision::Rejected {
                retry_after_secs: 1
            }
        );
    }

    #[test]
    fn refills_over_time() {
        let limiter = limiter();
        let t0 = Instant::now();

        // Drain the bucket.
        for _ in 0..3 {
            let _ = limiter.try_acquire_at(IP, t0);
        }
        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 0)),
            Decision::Rejected {
                retry_after_secs: 1
            }
        );

        // After 0.5s half a token has regenerated: still not enough.
        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 500)),
            Decision::Rejected {
                retry_after_secs: 1
            }
        );

        // After another 0.5s one full token is available again.
        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 1000)),
            Decision::Allowed { remaining: 0.0 }
        );
    }

    #[test]
    fn refill_is_capped_at_capacity() {
        let limiter = limiter();
        let t0 = Instant::now();

        let _ = limiter.try_acquire_at(IP, t0);

        // A very long idle period cannot exceed the capacity.
        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 60_000)),
            Decision::Allowed { remaining: 2.0 }
        );
    }

    #[test]
    fn retry_after_is_at_least_one_second() {
        let limiter = RateLimiter::new(1, 100.0);
        let t0 = Instant::now();

        let _ = limiter.try_acquire_at(IP, t0);
        // 0.01s would be enough for a token, but the hint is rounded up.
        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 1)),
            Decision::Rejected {
                retry_after_secs: 1
            }
        );
    }

    #[test]
    fn buckets_are_isolated_per_ip() {
        let limiter = limiter();
        let t0 = Instant::now();
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

        for _ in 0..3 {
            let _ = limiter.try_acquire_at(IP, t0);
        }

        assert_eq!(
            limiter.try_acquire_at(IP, later(t0, 0)),
            Decision::Rejected {
                retry_after_secs: 1
            }
        );
        assert_eq!(
            limiter.try_acquire_at(other, later(t0, 0)),
            Decision::Allowed { remaining: 2.0 }
        );
        assert_eq!(limiter.tracked_ips(), 2);
    }

    #[test]
    fn stale_buckets_are_swept() {
        let mut buckets = HashMap::new();
        let capacity = 3.0;
        let refill = 1.0;
        let now = Instant::now();

        // Fresh bucket: retained.
        buckets.insert(
            IP,
            Bucket {
                tokens: 0.0,
                last_refill: now,
            },
        );
        // Idle long enough to be fully refilled: swept.
        buckets.insert(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            Bucket {
                tokens: 1.0,
                last_refill: now - Duration::from_secs(5),
            },
        );
        // Still short of a full bucket even after idling: retained.
        buckets.insert(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)),
            Bucket {
                tokens: 2.5,
                last_refill: now - Duration::from_millis(400),
            },
        );

        sweep_stale(&mut buckets, capacity, refill);

        assert_eq!(buckets.len(), 2);
        assert!(buckets.contains_key(&IP));
        assert!(buckets.contains_key(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3))));
    }

    #[test]
    fn tracks_bounded_number_of_ips() {
        let limiter = RateLimiter::new(1, 1000.0);
        let t0 = Instant::now();

        // Simulate more distinct IPs than the tracking bound.
        for i in 0..(MAX_TRACKED_IPS + 50) as u32 {
            let ip = IpAddr::V4(Ipv4Addr::new(10, 0, (i >> 8) as u8, (i & 0xff) as u8));
            let _ = limiter.try_acquire_at(ip, t0);
        }

        assert!(limiter.tracked_ips() <= MAX_TRACKED_IPS);
    }
}
