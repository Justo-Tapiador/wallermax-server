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
//!
//! [`LoginThrottle`] is the credential-level companion: it counts *failed
//! login attempts* (not requests) and locks brute-force sources out.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

// ── Login brute-force throttling ──────────────────────────────────────

/// Consecutive failures on one (IP, username) pair before the lockout.
pub const LOGIN_MAX_FAILURES: u32 = 5;
/// Base lockout for the first trip; each consecutive trip doubles it.
const LOGIN_LOCKOUT_BASE: Duration = Duration::from_secs(30);
/// Ceiling for the doubling lockout.
const LOGIN_LOCKOUT_MAX: Duration = Duration::from_secs(15 * 60);
/// Failures (any usernames) from one IP inside [`LOGIN_SPRAY_WINDOW`]
/// before the whole address is locked: username spraying must not get a
/// per-username budget.
pub const LOGIN_SPRAY_FAILURES: usize = 20;
/// Sliding window that counts a single IP's failures.
const LOGIN_SPRAY_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Lockout for a spraying address.
const LOGIN_SPRAY_LOCKOUT: Duration = Duration::from_secs(5 * 60);
/// How long an idle (unlocked) pair entry is worth keeping.
const LOGIN_IDLE_HORIZON: Duration = Duration::from_secs(15 * 60);
/// Bound on tracked keys per map (see [`RateLimiter`]'s sweep rationale).
const MAX_TRACKED_LOGIN_KEYS: usize = 10_000;

/// Failed-login throttling: the credential-level companion of the
/// request-level [`RateLimiter`].
///
/// The token bucket bounds *every* request; this one bounds the
/// *guessable* ones. Two granularities, one struct:
///
/// - **(client IP, username)** — [`LOGIN_MAX_FAILURES`] consecutive
///   failures lock the pair for [`LOGIN_LOCKOUT_BASE`], doubling per
///   consecutive lockout (capped at [`LOGIN_LOCKOUT_MAX`]). A
///   successful login clears the pair's slate.
/// - **client IP** alone — [`LOGIN_SPRAY_FAILURES`] failures against
///   *any* usernames inside [`LOGIN_SPRAY_WINDOW`] lock the whole
///   address for [`LOGIN_SPRAY_LOCKOUT`], so distributing guesses over
///   many usernames buys nothing.
///
/// Like the buckets above, the maps are swept when they grow past
/// [`MAX_TRACKED_LOGIN_KEYS`] entries, so a spoofed-address flood
/// cannot grow memory without bound.
///
/// Determinism: every decision has an `_at` twin that takes the
/// instant, so the lockout math is unit-tested without sleeping.
pub struct LoginThrottle {
    pairs: Mutex<HashMap<(IpAddr, String), PairEntry>>,
    sprays: Mutex<HashMap<IpAddr, SprayEntry>>,
}

/// Failure bookkeeping for one (IP, username) pair.
#[derive(Debug, Clone, Copy)]
struct PairEntry {
    failures: u32,
    lockouts: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// Sliding-window bookkeeping for one address.
#[derive(Debug, Default)]
struct SprayEntry {
    recent: VecDeque<Instant>,
    locked_until: Option<Instant>,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginThrottle {
    /// An empty throttle.
    pub fn new() -> Self {
        Self {
            pairs: Mutex::new(HashMap::new()),
            sprays: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a login attempt from `ip` for `username` may proceed.
    ///
    /// Pure check: it consumes nothing (only failures advance state),
    /// so a rejected attempt stays rejected until its lockout expires.
    pub fn check(&self, ip: IpAddr, username: &str) -> Decision {
        self.check_at(ip, username, Instant::now())
    }

    /// [`Self::check`] at a given instant (deterministic tests).
    pub fn check_at(&self, ip: IpAddr, username: &str, now: Instant) -> Decision {
        let username = username.to_ascii_lowercase();

        // The address-wide spray lock wins over everything: while it
        // holds, no username from this address may proceed.
        {
            let sprays = self
                .sprays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = sprays.get(&ip) {
                if let Some(locked_until) = entry.locked_until {
                    if locked_until > now {
                        return Decision::Rejected {
                            retry_after_secs: seconds_until(locked_until, now),
                        };
                    }
                }
            }
        }

        let pairs = self
            .pairs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(entry) = pairs.get(&(ip, username)) {
            if let Some(locked_until) = entry.locked_until {
                if locked_until > now {
                    return Decision::Rejected {
                        retry_after_secs: seconds_until(locked_until, now),
                    };
                }
            }
            return Decision::Allowed {
                remaining: (LOGIN_MAX_FAILURES.saturating_sub(entry.failures)) as f64,
            };
        }
        Decision::Allowed {
            remaining: LOGIN_MAX_FAILURES as f64,
        }
    }

    /// Records a failed login attempt from `ip` for `username`.
    pub fn record_failure(&self, ip: IpAddr, username: &str) {
        self.record_failure_at(ip, username, Instant::now());
    }

    /// [`Self::record_failure`] at a given instant (deterministic tests).
    pub fn record_failure_at(&self, ip: IpAddr, username: &str, now: Instant) {
        let username = username.to_ascii_lowercase();

        // The pair lockout...
        {
            let mut pairs = self
                .pairs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !pairs.contains_key(&(ip, username.clone())) && pairs.len() >= MAX_TRACKED_LOGIN_KEYS
            {
                sweep_stale_pairs(&mut pairs, now);
                if pairs.len() >= MAX_TRACKED_LOGIN_KEYS {
                    tracing::warn!(
                        tracked = pairs.len(),
                        "login throttle pair map reset: too many simultaneously active keys"
                    );
                    pairs.clear();
                }
            }
            let entry = pairs.entry((ip, username)).or_insert(PairEntry {
                failures: 0,
                lockouts: 0,
                locked_until: None,
                last_seen: now,
            });
            entry.last_seen = now;
            // A failure while already locked cannot normally arrive
            // (attempts are checked first); extend nothing if it does.
            if entry.locked_until.is_some_and(|until| until > now) {
                return;
            }
            entry.failures += 1;
            if entry.failures >= LOGIN_MAX_FAILURES {
                entry.lockouts += 1;
                entry.locked_until = Some(now + lockout_for(entry.lockouts));
                entry.failures = 0;
            }
        }

        // ...and the address-wide spray counter.
        {
            let mut sprays = self
                .sprays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !sprays.contains_key(&ip) && sprays.len() >= MAX_TRACKED_LOGIN_KEYS {
                sweep_stale_sprays(&mut sprays, now);
                if sprays.len() >= MAX_TRACKED_LOGIN_KEYS {
                    sprays.clear();
                }
            }
            let entry = sprays.entry(ip).or_default();
            if entry.locked_until.is_some_and(|until| until > now) {
                return;
            }
            entry.recent.push_back(now);
            while entry
                .recent
                .front()
                .is_some_and(|stamp| now.saturating_duration_since(*stamp) > LOGIN_SPRAY_WINDOW)
            {
                entry.recent.pop_front();
            }
            if entry.recent.len() >= LOGIN_SPRAY_FAILURES {
                entry.locked_until = Some(now + LOGIN_SPRAY_LOCKOUT);
                entry.recent.clear();
                tracing::warn!(
                    client_ip = %ip,
                    "login throttled: the address failed logins across many usernames"
                );
            }
        }
    }

    /// Records a successful login: the (IP, username) pair's slate is
    /// wiped. The address-wide spray history is deliberately kept —
    /// one honest user behind a NAT is not a spraying pattern's exit.
    pub fn record_success(&self, ip: IpAddr, username: &str) {
        let username = username.to_ascii_lowercase();
        self.pairs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(ip, username));
    }

    /// Number of tracked keys across both maps (observability/testing).
    pub fn tracked_keys(&self) -> usize {
        let pairs = self
            .pairs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        let sprays = self
            .sprays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        pairs + sprays
    }
}

/// The lockout for the `n`-th consecutive trip: 30s, 60s, 120s... capped.
fn lockout_for(consecutive_lockouts: u32) -> Duration {
    let factor = 1_u64
        .checked_shl(consecutive_lockouts.saturating_sub(1).min(6))
        .unwrap_or(64);
    LOGIN_LOCKOUT_BASE
        .saturating_mul(factor as u32)
        .min(LOGIN_LOCKOUT_MAX)
}

/// `Retry-After` seconds until `until` (always at least one).
fn seconds_until(until: Instant, now: Instant) -> u64 {
    until.saturating_duration_since(now).as_secs().max(1)
}

/// Drops pair entries with no lock and no activity inside the horizon.
fn sweep_stale_pairs(pairs: &mut HashMap<(IpAddr, String), PairEntry>, now: Instant) {
    pairs.retain(|_, entry| {
        entry.locked_until.is_some_and(|until| until > now)
            || now.saturating_duration_since(entry.last_seen) < LOGIN_IDLE_HORIZON
    });
}

/// Drops spray entries with no lock and an empty window.
fn sweep_stale_sprays(sprays: &mut HashMap<IpAddr, SprayEntry>, now: Instant) {
    sprays.retain(|_, entry| {
        entry.locked_until.is_some_and(|until| until > now)
            || entry
                .recent
                .back()
                .is_some_and(|stamp| now.saturating_duration_since(*stamp) < LOGIN_SPRAY_WINDOW)
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

    // ── LoginThrottle ────────────────────────────────────────────────

    const OTHER_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

    fn secs_later(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn five_failures_lock_the_ip_username_pair() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();

        for _ in 0..LOGIN_MAX_FAILURES {
            throttle.record_failure_at(IP, "alice", t0);
        }

        match throttle.check_at(IP, "alice", t0) {
            Decision::Rejected { retry_after_secs } => {
                assert!(retry_after_secs >= 30, "base lockout: {retry_after_secs}");
            }
            decision => panic!("expected a lockout, got {decision:?}"),
        }

        // The pair is the unit: another username from the same address
        // is still below the spray threshold and may proceed.
        assert!(matches!(
            throttle.check_at(IP, "bob", t0),
            Decision::Allowed { .. }
        ));
        // And another address is untouched.
        assert!(matches!(
            throttle.check_at(OTHER_IP, "alice", t0),
            Decision::Allowed { .. }
        ));
    }

    #[test]
    fn usernames_are_matched_case_insensitively() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();

        for _ in 0..LOGIN_MAX_FAILURES {
            throttle.record_failure_at(IP, "Alice", t0);
        }

        // "alice" is the same pair as "Alice".
        assert!(matches!(
            throttle.check_at(IP, "alice", t0),
            Decision::Rejected { .. }
        ));
    }

    #[test]
    fn lockouts_expire_and_escalate() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();

        // First trip: 30 seconds.
        for _ in 0..LOGIN_MAX_FAILURES {
            throttle.record_failure_at(IP, "alice", t0);
        }
        assert!(matches!(
            throttle.check_at(IP, "alice", secs_later(t0, 29)),
            Decision::Rejected { .. }
        ));
        assert!(matches!(
            throttle.check_at(IP, "alice", secs_later(t0, 31)),
            Decision::Allowed { .. }
        ));

        // Second consecutive trip: doubled.
        for _ in 0..LOGIN_MAX_FAILURES {
            throttle.record_failure_at(IP, "alice", secs_later(t0, 31));
        }
        assert!(matches!(
            throttle.check_at(IP, "alice", secs_later(t0, 31 + 59)),
            Decision::Rejected { .. }
        ));
        assert!(matches!(
            throttle.check_at(IP, "alice", secs_later(t0, 31 + 61)),
            Decision::Allowed { .. }
        ));
    }

    #[test]
    fn a_successful_login_clears_the_pair() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();

        for _ in 0..(LOGIN_MAX_FAILURES - 1) {
            throttle.record_failure_at(IP, "alice", t0);
        }
        throttle.record_success(IP, "alice");

        // The slate is wiped: it takes a full new budget to trip again.
        for _ in 0..(LOGIN_MAX_FAILURES - 1) {
            throttle.record_failure_at(IP, "alice", t0);
        }
        assert!(matches!(
            throttle.check_at(IP, "alice", t0),
            Decision::Allowed { .. }
        ));
    }

    #[test]
    fn username_spraying_locks_the_whole_address() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();

        for i in 0..LOGIN_SPRAY_FAILURES {
            throttle.record_failure_at(IP, &format!("victim-{i}"), t0);
        }

        // Even a brand-new username from the spraying address is out...
        assert!(matches!(
            throttle.check_at(IP, "fresh-target", t0),
            Decision::Rejected { .. }
        ));
        // ...while other addresses keep their budget.
        assert!(matches!(
            throttle.check_at(OTHER_IP, "fresh-target", t0),
            Decision::Allowed { .. }
        ));
    }

    #[test]
    fn the_spray_window_and_lock_expire_together() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();

        for i in 0..LOGIN_SPRAY_FAILURES {
            throttle.record_failure_at(IP, &format!("victim-{i}"), t0);
        }
        assert!(matches!(
            throttle.check_at(IP, "anyone", t0),
            Decision::Rejected { .. }
        ));

        // Five minutes later the spray lock has expired AND the window
        // has slid past every counted failure.
        let after = t0 + LOGIN_SPRAY_WINDOW + LOGIN_SPRAY_LOCKOUT + Duration::from_secs(1);
        assert!(matches!(
            throttle.check_at(IP, "anyone", after),
            Decision::Allowed { .. }
        ));
    }

    #[test]
    fn login_keys_stay_bounded() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();

        // Far more distinct pairs than the tracking bound.
        for i in 0..(MAX_TRACKED_LOGIN_KEYS + 50) as u32 {
            let ip = IpAddr::V4(Ipv4Addr::new(10, 0, (i >> 8) as u8, (i & 0xff) as u8));
            throttle.record_failure_at(ip, "someone", t0);
        }

        assert!(throttle.tracked_keys() <= 2 * MAX_TRACKED_LOGIN_KEYS);
    }
}
