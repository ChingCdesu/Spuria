//! Per-IP token-bucket rate limiter (plan §6 "限流", P4).
//!
//! Shared by the signaling and relay servers to bound new-connection rate from
//! any single source address. Lock-based with a tiny critical section; the
//! connection-accept path is low-frequency so contention is negligible.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A hard bound also protects against a rotating source-IP population.
const MAX_BUCKETS: usize = 50_000;
const PRUNE_INTERVAL: Duration = Duration::from_secs(1);

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
struct State {
    buckets: HashMap<IpAddr, Bucket>,
    next_prune: Option<Instant>,
}

/// Token-bucket limiter keyed by source IP.
pub struct RateLimiter {
    inner: Mutex<State>,
    capacity: f64,
    refill_per_sec: f64,
}

impl RateLimiter {
    /// `capacity` = max burst; `refill_per_sec` = sustained allowed rate.
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            inner: Mutex::new(State::default()),
            capacity: capacity.max(1.0),
            refill_per_sec: refill_per_sec.max(0.001),
        }
    }

    /// Consume one token for `ip`; returns `true` if allowed.
    pub fn allow(&self, ip: IpAddr) -> bool {
        self.allow_at(ip, Instant::now())
    }

    fn allow_at(&self, ip: IpAddr, now: Instant) -> bool {
        let mut state = self.inner.lock().unwrap();
        if state.buckets.len() >= MAX_BUCKETS && state.next_prune.map_or(true, |next| now >= next) {
            let cap = self.capacity;
            state.buckets.retain(|_, b| {
                b.tokens + now.duration_since(b.last).as_secs_f64() * self.refill_per_sec < cap
            });
            state.next_prune = Some(now + PRUNE_INTERVAL);
        }
        if state.buckets.len() >= MAX_BUCKETS && !state.buckets.contains_key(&ip) {
            return false;
        }

        let bucket = state.buckets.entry(ip).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_then_throttles() {
        let rl = RateLimiter::new(3.0, 0.0001); // ~no refill during the test
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(rl.allow(ip));
        assert!(rl.allow(ip));
        assert!(rl.allow(ip));
        assert!(
            !rl.allow(ip),
            "4th request in a burst of 3 should be denied"
        );
        // A different IP has its own bucket.
        assert!(rl.allow("10.0.0.2".parse().unwrap()));
    }

    #[test]
    fn expired_buckets_are_reclaimed_but_recent_buckets_stay_throttled() {
        let rl = RateLimiter::new(1.0, 1.0);
        let now = Instant::now();
        let active: IpAddr = "255.255.255.254".parse().unwrap();
        {
            let mut state = rl.inner.lock().unwrap();
            for n in 0..(MAX_BUCKETS - 1) as u32 {
                state.buckets.insert(
                    std::net::Ipv4Addr::from(n).into(),
                    Bucket {
                        tokens: 0.0,
                        last: now - Duration::from_secs(2),
                    },
                );
            }
            state.buckets.insert(
                active,
                Bucket {
                    tokens: 0.0,
                    last: now,
                },
            );
        }
        assert!(!rl.allow_at(active, now));
        assert_eq!(rl.inner.lock().unwrap().buckets.len(), 1);
        assert!(rl.allow_at("255.255.255.253".parse().unwrap(), now));
    }

    #[test]
    fn bounded_table_rejects_new_ips_without_repeated_full_scans() {
        let rl = RateLimiter::new(1.0, 0.001);
        let now = Instant::now();
        for n in 0..MAX_BUCKETS as u32 {
            assert!(rl.allow_at(std::net::Ipv4Addr::from(n).into(), now));
        }
        let next: IpAddr = "255.255.255.254".parse().unwrap();
        assert!(!rl.allow_at(next, now));
        let deadline = rl.inner.lock().unwrap().next_prune;
        assert!(!rl.allow_at(next, now + Duration::from_millis(10)));
        let state = rl.inner.lock().unwrap();
        assert_eq!(state.buckets.len(), MAX_BUCKETS);
        assert_eq!(state.next_prune, deadline);
    }
}
