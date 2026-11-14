//! Per-key login throttling — leaky bucket with capacity 5, drains 1/min.
//!
//! Tracks two independent keyspaces from the caller: IP and email. After 5
//! failures within 5 min the throttle returns `Allow::No`. Successful
//! logins do NOT touch the throttle (Plan 3 keeps it dumb on purpose).
//!
//! Time is injected via the `Clock` trait so tests don't need `tokio::time`.
//!
//! Memory is bounded without a background task: every
//! [`PRUNE_EVERY_NEW_KEYS`] new keys, fully drained keys are dropped; the map
//! never holds more than [`MAX_KEYS`] keys; and keys are truncated to
//! [`MAX_KEY_BYTES`]. A failure for a new key on a full map first drops the
//! drained keys and, when that frees nothing, evicts the sixteenth of the
//! keys whose last failure is oldest. There
//! is no shared bucket that new keys fall back to, so filling the map can't
//! lock everyone out of sign-in; and since the oldest failures go first, a
//! key is forgotten only after about [`MAX_KEYS`] newer failures elsewhere.

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
    /// When the last failure was recorded (eviction order on a full map).
    last_failed_at: Instant,
}

impl Bucket {
    /// Returns the post-drain token count and the timestamp that should be
    /// stored as `last_drained` (only the consumed whole intervals are
    /// credited — sub-interval remainder is preserved across calls).
    fn drained(&self, now: Instant) -> (u32, Instant) {
        let elapsed = now.saturating_duration_since(self.last_drained);
        let intervals = (elapsed.as_secs() / DRAIN_PER.as_secs()) as u32;
        let remaining = self.tokens.saturating_sub(intervals);
        // Advance `last_drained` only by the consumed whole intervals.
        let advance = DRAIN_PER * intervals;
        (remaining, self.last_drained + advance)
    }
}

/// Longest key the throttle stores; longer keys are cut (on a char
/// boundary), so a client can't grow memory with huge email strings.
pub const MAX_KEY_BYTES: usize = 320;
/// Most keys one throttle holds.
pub const MAX_KEYS: usize = 100_000;
/// Drained keys are pruned after this many new keys.
const PRUNE_EVERY_NEW_KEYS: usize = 1024;
/// A full map with no drained keys evicts `max_keys / EVICT_DIVISOR` keys at
/// once, so the O(n) eviction pass runs at most once per that many new keys.
const EVICT_DIVISOR: usize = 16;

/// Cut `key` to at most [`MAX_KEY_BYTES`], on a char boundary.
fn bounded_key(key: &str) -> &str {
    if key.len() <= MAX_KEY_BYTES {
        return key;
    }
    let mut end = MAX_KEY_BYTES;
    while !key.is_char_boundary(end) {
        end -= 1;
    }
    &key[..end]
}

struct Buckets {
    by_key: HashMap<String, Bucket>,
    /// New keys stored since the last prune (for the periodic prune).
    new_keys_since_prune: usize,
    max_keys: usize,
}

impl Buckets {
    fn new(max_keys: usize) -> Self {
        Self {
            by_key: HashMap::new(),
            new_keys_since_prune: 0,
            max_keys,
        }
    }

    fn prune(&mut self, now: Instant) {
        self.by_key.retain(|_, bucket| bucket.drained(now).0 > 0);
        self.new_keys_since_prune = 0;
    }

    fn at_key_cap(&self) -> bool {
        self.by_key.len() >= self.max_keys
    }

    /// Free room on a full map: drop the drained keys, and when that frees
    /// nothing, evict the keys whose last failure is oldest.
    fn make_room(&mut self, now: Instant) {
        self.prune(now);
        if !self.at_key_cap() {
            return;
        }
        let evict_count = (self.max_keys / EVICT_DIVISOR).clamp(1, self.by_key.len());
        let mut keys_by_last_failure: Vec<(Instant, &String)> = self
            .by_key
            .iter()
            .map(|(key, bucket)| (bucket.last_failed_at, key))
            .collect();
        keys_by_last_failure
            .select_nth_unstable_by_key(evict_count - 1, |(last_failed_at, _)| *last_failed_at);
        let evicted_keys: Vec<String> = keys_by_last_failure[..evict_count]
            .iter()
            .map(|(_, key)| (*key).clone())
            .collect();
        for evicted_key in evicted_keys {
            self.by_key.remove(&evicted_key);
        }
    }

    /// Store `bucket` as the new state for `key`, in its own slot. Counts new
    /// keys, runs the periodic prune when due, and makes room on a full map.
    fn store(&mut self, key: &str, bucket: Bucket, now: Instant) {
        if let Some(existing) = self.by_key.get_mut(key) {
            *existing = bucket;
            return;
        }
        self.new_keys_since_prune += 1;
        if self.new_keys_since_prune >= PRUNE_EVERY_NEW_KEYS {
            self.prune(now);
        }
        if self.at_key_cap() {
            self.make_room(now);
        }
        self.by_key.insert(key.to_string(), bucket);
    }
}

pub struct Throttle<C: Clock = SystemClock> {
    clock: Arc<C>,
    buckets: Mutex<Buckets>,
}

impl Throttle<SystemClock> {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }
}

impl Default for Throttle<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> Throttle<C> {
    pub fn with_clock(clock: Arc<C>) -> Self {
        Self::with_clock_and_max_keys(clock, MAX_KEYS)
    }

    /// Like [`Throttle::with_clock`] with a smaller key cap (tests).
    pub fn with_clock_and_max_keys(clock: Arc<C>, max_keys: usize) -> Self {
        Self {
            clock,
            buckets: Mutex::new(Buckets::new(max_keys.max(1))),
        }
    }

    fn lock_buckets(&self) -> std::sync::MutexGuard<'_, Buckets> {
        // A panic elsewhere can't leave a bucket half-written; keep using it.
        self.buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Check whether `key` may attempt a login. Does NOT record a failure;
    /// the caller invokes `record_failure` after a failed credential check.
    pub fn check(&self, key: &str) -> Allow {
        let key = bounded_key(key);
        let buckets = self.lock_buckets();
        let now = self.clock.now();
        let remaining = buckets
            .by_key
            .get(key)
            .map_or(0, |bucket| bucket.drained(now).0);
        if remaining >= CAPACITY {
            Allow::No
        } else {
            Allow::Yes
        }
    }

    /// Record a failed login for `key`. Returns the new failure count
    /// (1..=CAPACITY, capped).
    pub fn record_failure(&self, key: &str) -> u32 {
        let key = bounded_key(key);
        let mut buckets = self.lock_buckets();
        let now = self.clock.now();
        let (remaining, new_last_drained) = match buckets.by_key.get(key) {
            Some(bucket) => bucket.drained(now),
            None => (0, now),
        };
        let new_count = (remaining + 1).min(CAPACITY);
        buckets.store(
            key,
            Bucket {
                tokens: new_count,
                last_drained: new_last_drained,
                last_failed_at: now,
            },
            now,
        );
        new_count
    }

    /// Reset a key (called on successful login).
    pub fn reset(&self, key: &str) {
        let key = bounded_key(key);
        self.lock_buckets().by_key.remove(key);
    }

    /// Number of keys with a bucket.
    pub fn len(&self) -> usize {
        self.lock_buckets().by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClock {
        base: Instant,
        offset_nanos: std::sync::atomic::AtomicU64,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                offset_nanos: std::sync::atomic::AtomicU64::new(0),
            })
        }
        fn advance(&self, by: Duration) {
            self.offset_nanos
                .fetch_add(by.as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            let off = self.offset_nanos.load(std::sync::atomic::Ordering::Relaxed);
            self.base + Duration::from_nanos(off)
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

    #[test]
    fn slow_trickle_eventually_blocks() {
        // An attacker submitting failed logins slightly faster than the
        // drain rate (every 59 s vs. the 60 s drain interval) must
        // eventually hit the cap — the sub-minute remainders accumulate
        // across calls instead of resetting `last_drained = now` each
        // time. Under the bug, the bucket would oscillate at ~2 tokens
        // forever; under the fix it grows by 1 token every ~60 attempts.
        let clock = FakeClock::new();
        let t = Throttle::with_clock(clock.clone());
        let mut blocked = false;
        for _ in 0..200 {
            t.record_failure("k");
            if t.check("k") == Allow::No {
                blocked = true;
                break;
            }
            clock.advance(Duration::from_secs(59));
        }
        assert!(
            blocked,
            "slow-trickle attacker at 59 s spacing must eventually be blocked"
        );
    }

    #[test]
    fn drain_preserves_sub_interval_remainder() {
        // Two calls 30 s apart, then a third 30 s later (total 60 s).
        // With the bug, each call snaps `last_drained = now`, so the
        // third call sees only 30 s elapsed → no drain → tokens=3.
        // With the fix, `last_drained` stays at the original timestamp
        // until a full interval is consumed → third call sees 60 s
        // elapsed → 1 token drained → tokens=2.
        let clock = FakeClock::new();
        let t = Throttle::with_clock(clock.clone());
        assert_eq!(t.record_failure("k"), 1);
        clock.advance(Duration::from_secs(30));
        assert_eq!(t.record_failure("k"), 2);
        clock.advance(Duration::from_secs(30));
        assert_eq!(
            t.record_failure("k"),
            2,
            "sub-interval remainder must accumulate so a full minute drains 1 token"
        );
    }

    /// Fail `key` until its bucket is full, then let it drain completely.
    fn fill_then_drain(throttle: &Throttle<FakeClock>, clock: &FakeClock, key: &str) {
        for _ in 0..CAPACITY {
            throttle.record_failure(key);
        }
        clock.advance(DRAIN_PER * CAPACITY);
    }

    #[test]
    fn drained_keys_are_pruned_after_enough_new_keys() {
        let clock = FakeClock::new();
        let throttle = Throttle::with_clock(clock.clone());
        fill_then_drain(&throttle, &clock, "idle");
        for _ in 0..CAPACITY {
            throttle.record_failure("active");
        }
        assert_eq!(throttle.len(), 2);
        for new_key_index in 0..PRUNE_EVERY_NEW_KEYS {
            throttle.record_failure(&format!("new-{new_key_index}"));
        }
        assert_eq!(
            throttle.len(),
            PRUNE_EVERY_NEW_KEYS + 1,
            "the drained key is gone, the active one and the new ones stay"
        );
        assert_eq!(
            throttle.check("active"),
            Allow::No,
            "active key kept its count"
        );
    }

    #[test]
    fn a_full_map_of_active_keys_never_refuses_a_new_key() {
        // An attacker fills the map with keys at capacity and keeps failing
        // with further new keys: a new user must still be allowed (there is
        // no shared bucket for keys without their own).
        let clock = FakeClock::new();
        let throttle = Throttle::with_clock_and_max_keys(clock, 3);
        for attacker_key in ["attacker-a", "attacker-b", "attacker-c"] {
            for _ in 0..CAPACITY {
                throttle.record_failure(attacker_key);
            }
        }
        for _ in 0..CAPACITY {
            throttle.record_failure("attacker-overflowing");
        }
        assert_eq!(throttle.len(), 3, "the map never grows past its cap");
        assert_eq!(
            throttle.check("fresh-legit-user"),
            Allow::Yes,
            "a key without a bucket has no failures"
        );
        assert_eq!(
            throttle.check("attacker-overflowing"),
            Allow::No,
            "the newest key got a bucket of its own"
        );
    }

    #[test]
    fn a_full_map_evicts_the_keys_whose_last_failure_is_oldest() {
        let clock = FakeClock::new();
        let max_keys = EVICT_DIVISOR * 2; // evicts two keys at a time
        let throttle = Throttle::with_clock_and_max_keys(clock.clone(), max_keys);
        for key_index in 0..max_keys {
            for _ in 0..CAPACITY {
                throttle.record_failure(&format!("key-{key_index}"));
            }
            clock.advance(Duration::from_millis(10));
        }
        // The first key fails again, so it is no longer among the oldest.
        throttle.record_failure("key-0");
        throttle.record_failure("newcomer");
        assert_eq!(throttle.len(), max_keys - 1, "two evicted, one added");
        assert_eq!(throttle.check("key-0"), Allow::No, "recently failed: kept");
        assert_eq!(
            throttle.check("key-1"),
            Allow::Yes,
            "oldest failure: evicted"
        );
        assert_eq!(throttle.check("key-2"), Allow::Yes, "next oldest: evicted");
        assert_eq!(throttle.check("key-3"), Allow::No, "newer failures: kept");
        assert_eq!(throttle.record_failure("newcomer"), 2, "has its own bucket");
    }

    #[test]
    fn a_full_map_drops_drained_keys_before_evicting() {
        let clock = FakeClock::new();
        let max_keys = EVICT_DIVISOR * 4;
        let throttle = Throttle::with_clock_and_max_keys(clock.clone(), max_keys);
        for key_index in 0..max_keys - 1 {
            throttle.record_failure(&format!("old-{key_index}"));
        }
        clock.advance(DRAIN_PER - Duration::from_secs(1));
        for _ in 0..CAPACITY {
            throttle.record_failure("active");
        }
        clock.advance(Duration::from_secs(1)); // the old keys are drained now
        throttle.record_failure("fresh");
        assert_eq!(
            throttle.len(),
            2,
            "the drained keys made room; the active one stayed"
        );
        assert_eq!(throttle.check("active"), Allow::No);
    }

    #[test]
    fn long_keys_are_truncated_to_max_key_bytes() {
        let clock = FakeClock::new();
        let throttle = Throttle::with_clock(clock);
        let shared_prefix = "e".repeat(MAX_KEY_BYTES);
        let first_long_key = format!("{shared_prefix}-first");
        let second_long_key = format!("{shared_prefix}-second");
        for _ in 0..CAPACITY {
            throttle.record_failure(&first_long_key);
        }
        assert_eq!(
            throttle.check(&second_long_key),
            Allow::No,
            "keys that differ only past MAX_KEY_BYTES share a bucket"
        );
        throttle.reset(&second_long_key);
        assert_eq!(
            throttle.check(&first_long_key),
            Allow::Yes,
            "reset truncates too"
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let clock = FakeClock::new();
        let throttle = Throttle::with_clock(clock);
        // 'é' is two bytes: an odd prefix puts byte 320 inside a character.
        let multibyte_key = format!("x{}", "é".repeat(MAX_KEY_BYTES));
        throttle.record_failure(&multibyte_key);
        assert_eq!(throttle.record_failure(&multibyte_key), 2);
    }

    #[test]
    fn record_failure_returns_capped_count() {
        let clock = FakeClock::new();
        let t = Throttle::with_clock(clock);
        for n in 1..=5 {
            assert_eq!(t.record_failure("k"), n);
        }
        // Sixth and beyond cap at 5.
        assert_eq!(t.record_failure("k"), 5);
    }
}
