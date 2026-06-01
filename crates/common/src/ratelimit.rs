//! Per-IP token-bucket rate limiter (plan §6 "限流", P4).
//!
//! Shared by the signaling and relay servers to bound new-connection rate from
//! any single source address. Lock-based with a tiny critical section; the
//! connection-accept path is low-frequency so contention is negligible.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

/// Drop idle buckets once the table grows past this many entries.
const PRUNE_THRESHOLD: usize = 50_000;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token-bucket limiter keyed by source IP.
pub struct RateLimiter {
    inner: Mutex<HashMap<IpAddr, Bucket>>,
    capacity: f64,
    refill_per_sec: f64,
}

impl RateLimiter {
    /// `capacity` = max burst; `refill_per_sec` = sustained allowed rate.
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity: capacity.max(1.0),
            refill_per_sec: refill_per_sec.max(0.001),
        }
    }

    /// Consume one token for `ip`; returns `true` if allowed.
    pub fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap();

        if map.len() > PRUNE_THRESHOLD {
            let cap = self.capacity;
            map.retain(|_, b| b.tokens < cap); // keep only actively-throttled IPs
        }

        let bucket = map.entry(ip).or_insert(Bucket {
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
}
