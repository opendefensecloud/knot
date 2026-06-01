//! Per-key login throttling — leaky bucket with capacity 5, drains 1/min.
//!
//! Tracks two independent keyspaces from the caller: IP and email. After 5
//! failures within 5 min the throttle returns `Allow::No`. Successful
//! logins do NOT touch the throttle (Plan 3 keeps it dumb on purpose).
//!
//! Time is injected via the `Clock` trait so tests don't need `tokio::time`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allow {
    /// Caller may proceed with the authentication attempt.
    Yes,
    /// Caller is over budget; return the generic "invalid credentials"
    /// response. The handler should also sleep 1 s before responding.
    No,
}

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

const CAPACITY: u32 = 5;
const DRAIN_PER: Duration = Duration::from_secs(60); // 1 token / minute

struct Bucket {
    tokens: u32,
    last_drained: Instant,
}

pub struct Throttle<C: Clock = SystemClock> {
    clock: Arc<C>,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Throttle<SystemClock> {
    pub fn new() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            buckets: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for Throttle<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> Throttle<C> {
    pub fn with_clock(clock: Arc<C>) -> Self {
        Self {
            clock,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether `key` may attempt a login. Does NOT record a failure;
    /// the caller invokes `record_failure` after a failed credential check.
    pub fn check(&self, key: &str) -> Allow {
        let map = self.buckets.lock().expect("throttle mutex");
        let now = self.clock.now();
        let drained = drain(map.get(key), now);
        if drained >= CAPACITY {
            Allow::No
        } else {
            Allow::Yes
        }
    }

    /// Record a failed login for `key`. Returns the new failure count
    /// (1..=CAPACITY, capped).
    pub fn record_failure(&self, key: &str) -> u32 {
        let mut map = self.buckets.lock().expect("throttle mutex");
        let now = self.clock.now();
        let drained = drain(map.get(key), now);
        let new_count = (drained + 1).min(CAPACITY);
        map.insert(
            key.to_string(),
            Bucket {
                tokens: new_count,
                last_drained: now,
            },
        );
        new_count
    }

    /// Reset a key (called on successful login).
    pub fn reset(&self, key: &str) {
        let mut map = self.buckets.lock().expect("throttle mutex");
        map.remove(key);
    }
}

fn drain(b: Option<&Bucket>, now: Instant) -> u32 {
    let Some(b) = b else { return 0 };
    let elapsed = now.saturating_duration_since(b.last_drained);
    let drained = (elapsed.as_secs() / DRAIN_PER.as_secs()) as u32;
    b.tokens.saturating_sub(drained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct FakeClock {
        base: Instant,
        offset: Cell<Duration>,
    }
    // Cell isn't Sync, but the test mutates the offset only from the test
    // thread; we never actually share across threads in these tests.
    #[allow(clippy::non_send_fields_in_send_ty)]
    unsafe impl Send for FakeClock {}
    unsafe impl Sync for FakeClock {}

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                offset: Cell::new(Duration::ZERO),
            })
        }
        fn advance(&self, by: Duration) {
            self.offset.set(self.offset.get() + by);
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.base + self.offset.get()
        }
    }

    #[test]
    fn fresh_key_is_allowed() {
        let t = Throttle::new();
        assert_eq!(t.check("ip:1.2.3.4"), Allow::Yes);
    }

    #[test]
    fn five_failures_blocks_sixth() {
        let clock = FakeClock::new();
        let t = Throttle::with_clock(clock);
        for _ in 0..5 {
            t.record_failure("k");
        }
        assert_eq!(t.check("k"), Allow::No);
    }

    #[test]
    fn under_threshold_is_allowed() {
        let clock = FakeClock::new();
        let t = Throttle::with_clock(clock);
        for _ in 0..4 {
            t.record_failure("k");
        }
        assert_eq!(t.check("k"), Allow::Yes);
    }

    #[test]
    fn drains_one_token_per_minute() {
        let clock = FakeClock::new();
        let t = Throttle::with_clock(clock.clone());
        for _ in 0..5 {
            t.record_failure("k");
        }
        assert_eq!(t.check("k"), Allow::No);
        clock.advance(Duration::from_secs(60));
        assert_eq!(t.check("k"), Allow::Yes, "one token should have drained");
    }

    #[test]
    fn reset_clears_bucket() {
        let clock = FakeClock::new();
        let t = Throttle::with_clock(clock);
        for _ in 0..5 {
            t.record_failure("k");
        }
        t.reset("k");
        assert_eq!(t.check("k"), Allow::Yes);
    }
}
