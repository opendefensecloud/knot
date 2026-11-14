//! A keyed token bucket.
//!
//! Each key has `burst` tokens, refilled continuously at `per_second`. A
//! request takes tokens ([`KeyedBuckets::try_take`]); the collab read loops
//! instead charge after the fact ([`KeyedBuckets::charge`], tokens may go
//! negative) and wait off the debt ([`KeyedBuckets::deficit`]).
//!
//! Memory stays bounded without a background task: every
//! [`PRUNE_EVERY_NEW_KEYS`] new keys, buckets that are full again (idle) are
//! removed, and a map never holds more than [`MAX_KEYS`] keys: past that,
//! new keys share one overflow bucket. Before a key without a bucket of its
//! own is judged by the overflow bucket (on a take, a refusal or a read), a
//! full map is pruned when an at-cap prune is due, so an attack that filled
//! the map and drained the overflow doesn't outlast its idle keys.
//!
//! Time is passed in (`tokio::time::Instant`), so tests drive it directly
//! and paused-time tests work too.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use tokio::time::Instant;

/// Most keys one map holds. Past it, new keys share one overflow bucket.
pub const MAX_KEYS: usize = 100_000;
/// Idle (full) buckets are pruned after this many new keys.
pub const PRUNE_EVERY_NEW_KEYS: usize = 1024;
/// A full map is pruned early once this many keys without a bucket of their
/// own were looked up since the last prune (or after
/// [`PRUNE_AT_CAP_MIN_INTERVAL`]): it overflows only when its keys are really
/// active, and a flood of new keys costs at most one prune per this many keys
/// plus one per interval.
pub const PRUNE_AT_CAP_MIN_NEW_KEYS: usize = 64;
/// A full map is pruned early, for a key without a bucket of its own, at
/// least this long after the last prune, however few such keys came.
pub const PRUNE_AT_CAP_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// A bucket's size and refill speed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    /// Tokens a full bucket holds.
    pub burst: f64,
    /// Tokens added per second.
    pub per_second: f64,
}

impl Rate {
    /// The lowest any scaled HTTP rate goes: one token, one per minute.
    pub const MINIMUM: Rate = Rate {
        burst: 1.0,
        per_second: 1.0 / 60.0,
    };

    /// `burst` tokens, refilled at `per_minute` per minute. A zero rate
    /// never refills (its waits are [`Duration::MAX`]).
    pub fn per_minute(burst: u32, per_minute: u32) -> Rate {
        Rate {
            burst: f64::from(burst),
            per_second: f64::from(per_minute) / 60.0,
        }
    }

    /// `burst` tokens, refilled at `per_second` per second.
    pub fn per_second(burst: f64, per_second: f64) -> Rate {
        Rate { burst, per_second }
    }

    /// Both components multiplied by `scale`, never below `floor`.
    pub fn scaled(self, scale: f64, floor: Rate) -> Rate {
        Rate {
            burst: (self.burst * scale).max(floor.burst),
            per_second: (self.per_second * scale).max(floor.per_second),
        }
    }

    /// Time to refill `tokens` (saturating; a zero rate waits forever).
    fn time_to_refill(self, tokens: f64) -> Duration {
        if tokens <= 0.0 {
            return Duration::ZERO;
        }
        Duration::try_from_secs_f64(tokens / self.per_second).unwrap_or(Duration::MAX)
    }
}

/// Whole seconds until a refused request may succeed, for `Retry-After` and
/// `details.retry_after_secs`: rounded up, at least 1.
pub fn retry_after_secs(wait: Duration) -> u64 {
    let whole_seconds = wait
        .as_secs()
        .saturating_add(u64::from(wait.subsec_nanos() > 0));
    whole_seconds.max(1)
}

/// A refused [`KeyedBuckets::try_take_noting_start`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Refusal {
    /// How long until the bucket holds the tokens asked for.
    pub wait: Duration,
    /// The first refusal since the key's last successful take.
    pub newly_limited: bool,
}

/// One bucket's level at a moment.
#[derive(Clone, Copy, Debug)]
struct BucketState {
    /// May be negative (debt from [`KeyedBuckets::charge`]).
    tokens: f64,
    updated_at: Instant,
    /// The last take was refused (for [`Refusal::newly_limited`]).
    limited: bool,
}

impl BucketState {
    fn full(rate: Rate, now: Instant) -> Self {
        Self {
            tokens: rate.burst,
            updated_at: now,
            limited: false,
        }
    }

    /// The level at `now`: refilled since the last update, capped at the
    /// burst. A `now` before the last update refills nothing.
    fn refilled(self, rate: Rate, now: Instant) -> Self {
        let elapsed_secs = now.saturating_duration_since(self.updated_at).as_secs_f64();
        Self {
            tokens: (self.tokens + elapsed_secs * rate.per_second).min(rate.burst),
            updated_at: now.max(self.updated_at),
            limited: self.limited,
        }
    }

    fn is_full(self, rate: Rate) -> bool {
        self.tokens >= rate.burst
    }
}

/// Token buckets per key, all with one [`Rate`].
///
/// A std `Mutex` guards the map: critical sections are a hash lookup and a
/// few float operations, and the lock is never held across an `.await`.
pub struct KeyedBuckets<K> {
    rate: Rate,
    inner: Mutex<Inner<K>>,
}

struct Inner<K> {
    by_key: HashMap<K, BucketState>,
    /// Shared by every new key once `by_key` is at its cap.
    overflow: Option<BucketState>,
    /// New keys stored since the last prune (for the periodic prune).
    new_keys_since_prune: usize,
    /// Lookups of a key without a bucket of its own while the map was at
    /// its cap, since the last prune (for the at-cap prune).
    misses_at_cap_since_prune: usize,
    last_prune: Option<Instant>,
    max_keys: usize,
}

impl<K: Hash + Eq + Clone> Inner<K> {
    fn new(max_keys: usize) -> Self {
        Self {
            by_key: HashMap::new(),
            overflow: None,
            new_keys_since_prune: 0,
            misses_at_cap_since_prune: 0,
            last_prune: None,
            max_keys,
        }
    }

    fn at_key_cap(&self) -> bool {
        self.by_key.len() >= self.max_keys
    }

    /// Drop the buckets that are full again (idle) at `now`.
    fn prune(&mut self, rate: Rate, now: Instant) {
        self.by_key
            .retain(|_, stored| !stored.refilled(rate, now).is_full(rate));
        self.new_keys_since_prune = 0;
        self.misses_at_cap_since_prune = 0;
        self.last_prune = Some(now);
    }

    /// Run before `key` is judged. When `key` has no bucket of its own and
    /// the map is at its cap, prune first if an at-cap prune is due, so the
    /// key gets room rather than the overflow bucket whenever idle buckets
    /// fill the map. Reads and refusals count too, not only stores: after an
    /// attack, new keys may only read or be refused, and must still reach a
    /// prune.
    fn prune_at_cap_if_due(&mut self, key: &K, rate: Rate, now: Instant) {
        if !self.at_key_cap() || self.by_key.contains_key(key) {
            return;
        }
        self.misses_at_cap_since_prune += 1;
        let enough_misses = self.misses_at_cap_since_prune >= PRUNE_AT_CAP_MIN_NEW_KEYS;
        let interval_passed = self.last_prune.is_none_or(|last_prune| {
            now.saturating_duration_since(last_prune) >= PRUNE_AT_CAP_MIN_INTERVAL
        });
        if enough_misses || interval_passed {
            self.prune(rate, now);
        }
    }

    /// `key`'s level at `now`, without storing anything: its own bucket, the
    /// overflow bucket when it has none and the map is at its cap, else full.
    /// Callers run [`Inner::prune_at_cap_if_due`] first.
    fn peek(&self, key: &K, rate: Rate, now: Instant) -> BucketState {
        let stored = match self.by_key.get(key) {
            Some(state) => Some(*state),
            None if self.at_key_cap() => self.overflow,
            None => None,
        };
        stored.map_or_else(
            || BucketState::full(rate, now),
            |state| state.refilled(rate, now),
        )
    }

    /// Store `state` for `key`: in its own slot when the key is known or
    /// there is room, otherwise in the overflow bucket. Counts new keys and
    /// runs the periodic prune when due. Where the state goes is decided
    /// before that prune, from the map [`Inner::peek`] read it from: a state
    /// read from the overflow bucket goes back there even when the prune
    /// frees room, so a key never inherits the overflow bucket's debt.
    fn store(&mut self, key: &K, state: BucketState, rate: Rate, now: Instant) {
        if let Some(existing) = self.by_key.get_mut(key) {
            *existing = state;
            return;
        }
        let judged_by_overflow = self.at_key_cap();
        self.new_keys_since_prune += 1;
        if self.new_keys_since_prune >= PRUNE_EVERY_NEW_KEYS {
            self.prune(rate, now);
        }
        if judged_by_overflow {
            self.overflow = Some(state);
        } else {
            self.by_key.insert(key.clone(), state);
        }
    }

    /// [`Inner::prune_at_cap_if_due`], then [`Inner::peek`].
    fn prepare_and_peek(&mut self, key: &K, rate: Rate, now: Instant) -> BucketState {
        self.prune_at_cap_if_due(key, rate, now);
        self.peek(key, rate, now)
    }

    /// Note that `key`'s last take was refused, in the bucket that judged it.
    /// Stores no new key and changes no tokens: a refused key always has a
    /// bucket already (its own, or the overflow bucket on a full map), since
    /// a fresh bucket is full and a full bucket refuses nothing.
    fn mark_limited(&mut self, key: &K) {
        if let Some(existing) = self.by_key.get_mut(key) {
            existing.limited = true;
        } else if self.at_key_cap()
            && let Some(overflow) = self.overflow.as_mut()
        {
            overflow.limited = true;
        }
    }
}

impl<K: Hash + Eq + Clone> KeyedBuckets<K> {
    pub fn new(rate: Rate) -> Self {
        Self::with_max_keys(rate, MAX_KEYS)
    }

    /// Like [`KeyedBuckets::new`] with a smaller key cap (tests).
    pub fn with_max_keys(rate: Rate, max_keys: usize) -> Self {
        Self {
            rate,
            inner: Mutex::new(Inner::new(max_keys.max(1))),
        }
    }

    pub fn rate(&self) -> Rate {
        self.rate
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner<K>> {
        // A panic elsewhere can't leave a bucket half-written; keep using it.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take `cost` tokens from `key`'s bucket, or say how long until it
    /// holds them. A refusal takes nothing. A `cost` above the burst is
    /// taken from a full bucket, into debt like [`KeyedBuckets::charge`], so
    /// the wait returned for it (until full) is truthful.
    pub fn try_take(&self, key: &K, cost: f64, now: Instant) -> Result<(), Duration> {
        self.try_take_noting_start(key, cost, now)
            .map_err(|refusal| refusal.wait)
    }

    /// [`KeyedBuckets::try_take`], also saying whether a refusal is the first
    /// since `key`'s last successful take, so a caller can log once when a
    /// key starts being limited rather than on every refused request.
    pub fn try_take_noting_start(&self, key: &K, cost: f64, now: Instant) -> Result<(), Refusal> {
        let mut inner = self.lock_inner();
        let mut state = inner.prepare_and_peek(key, self.rate, now);
        let needed = cost.min(self.rate.burst);
        if state.tokens >= needed {
            state.tokens -= cost;
            state.limited = false;
            inner.store(key, state, self.rate, now);
            Ok(())
        } else {
            let newly_limited = !state.limited;
            inner.mark_limited(key);
            Err(Refusal {
                wait: self.rate.time_to_refill(needed - state.tokens),
                newly_limited,
            })
        }
    }

    /// Subtract `cost` tokens from `key`'s bucket; it may go negative (debt).
    pub fn charge(&self, key: &K, cost: f64, now: Instant) {
        let mut inner = self.lock_inner();
        let mut state = inner.prepare_and_peek(key, self.rate, now);
        state.tokens -= cost;
        inner.store(key, state, self.rate, now);
    }

    /// How long until `key`'s bucket is out of debt; zero when it isn't.
    pub fn deficit(&self, key: &K, now: Instant) -> Duration {
        let state = self.lock_inner().prepare_and_peek(key, self.rate, now);
        self.rate.time_to_refill(-state.tokens)
    }

    /// How long until `key`'s bucket holds `needed` tokens (capped at the
    /// burst); zero when it already does.
    pub fn time_until_tokens(&self, key: &K, needed: f64, now: Instant) -> Duration {
        let state = self.lock_inner().prepare_and_peek(key, self.rate, now);
        self.rate
            .time_to_refill(needed.min(self.rate.burst) - state.tokens)
    }

    /// Keys with their own bucket (the overflow bucket excluded).
    pub fn len(&self) -> usize {
        self.lock_inner().by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Durations from float arithmetic: equal to within a microsecond.
    #[track_caller]
    fn assert_close(actual: Duration, expected: Duration) {
        let difference = actual.abs_diff(expected);
        assert!(
            difference <= Duration::from_micros(1),
            "expected {expected:?}, got {actual:?}"
        );
    }

    #[track_caller]
    fn assert_refused_for(result: Result<(), Duration>, expected_wait: Duration) {
        match result {
            Ok(()) => panic!("expected a refusal for {expected_wait:?}, got Ok"),
            Err(wait) => assert_close(wait, expected_wait),
        }
    }

    fn ten_per_ten_seconds() -> Rate {
        Rate::per_second(10.0, 1.0)
    }

    #[test]
    fn per_minute_converts_to_per_second() {
        assert_eq!(
            Rate::per_minute(120, 120),
            Rate {
                burst: 120.0,
                per_second: 2.0
            }
        );
    }

    #[test]
    fn a_fresh_key_has_a_full_burst_and_no_more() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        for _ in 0..10 {
            assert_eq!(buckets.try_take(&"user", 1.0, start), Ok(()));
        }
        assert_refused_for(
            buckets.try_take(&"user", 1.0, start),
            Duration::from_secs(1),
        );
    }

    #[test]
    fn tokens_refill_at_the_rate() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        for _ in 0..10 {
            buckets.try_take(&"user", 1.0, start).unwrap();
        }
        let half_a_second_later = start + Duration::from_millis(500);
        assert_refused_for(
            buckets.try_take(&"user", 1.0, half_a_second_later),
            Duration::from_millis(500),
        );
        let one_second_later = start + Duration::from_secs(1);
        assert_eq!(buckets.try_take(&"user", 1.0, one_second_later), Ok(()));
        assert!(buckets.try_take(&"user", 1.0, one_second_later).is_err());
    }

    #[test]
    fn refill_stops_at_the_burst() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        buckets.try_take(&"user", 1.0, start).unwrap();
        let an_hour_later = start + Duration::from_secs(3600);
        for _ in 0..10 {
            buckets.try_take(&"user", 1.0, an_hour_later).unwrap();
        }
        assert!(buckets.try_take(&"user", 1.0, an_hour_later).is_err());
    }

    #[test]
    fn try_take_waits_for_the_missing_tokens() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        buckets.try_take(&"user", 9.0, start).unwrap();
        assert_refused_for(
            buckets.try_take(&"user", 3.0, start),
            Duration::from_secs(2),
        );
        // A refused take takes nothing.
        assert_eq!(buckets.try_take(&"user", 1.0, start), Ok(()));
    }

    #[test]
    fn a_cost_above_the_burst_waits_until_full() {
        let buckets = KeyedBuckets::new(Rate::per_second(5.0, 1.0));
        let start = Instant::now();
        buckets.try_take(&"user", 3.0, start).unwrap();
        assert_refused_for(
            buckets.try_take(&"user", 8.0, start),
            Duration::from_secs(3),
        );
    }

    #[test]
    fn keys_are_independent() {
        let buckets = KeyedBuckets::new(Rate::per_second(1.0, 1.0));
        let start = Instant::now();
        buckets.try_take(&"first", 1.0, start).unwrap();
        assert!(buckets.try_take(&"first", 1.0, start).is_err());
        assert_eq!(buckets.try_take(&"second", 1.0, start), Ok(()));
    }

    #[test]
    fn charge_goes_into_debt_and_deficit_waits_it_off() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        assert_eq!(buckets.deficit(&"user", start), Duration::ZERO);
        buckets.charge(&"user", 15.0, start);
        assert_close(buckets.deficit(&"user", start), Duration::from_secs(5));
        assert_close(
            buckets.deficit(&"user", start + Duration::from_secs(2)),
            Duration::from_secs(3),
        );
        assert_eq!(
            buckets.deficit(&"user", start + Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn a_charge_within_the_tokens_leaves_no_deficit() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        buckets.charge(&"user", 10.0, start);
        assert_eq!(buckets.deficit(&"user", start), Duration::ZERO);
        assert!(buckets.try_take(&"user", 1.0, start).is_err());
    }

    #[test]
    fn debt_also_delays_try_take() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        buckets.charge(&"user", 12.0, start);
        assert_refused_for(
            buckets.try_take(&"user", 1.0, start),
            Duration::from_secs(3),
        );
    }

    #[test]
    fn time_until_tokens_counts_from_the_current_level() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        assert_eq!(
            buckets.time_until_tokens(&"user", 1.0, start),
            Duration::ZERO
        );
        buckets.charge(&"user", 10.0, start);
        assert_close(
            buckets.time_until_tokens(&"user", 1.0, start),
            Duration::from_secs(1),
        );
        assert_close(
            buckets.time_until_tokens(&"user", 50.0, start),
            Duration::from_secs(10),
        );
    }

    #[test]
    fn reading_a_key_does_not_store_it() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        let _ = buckets.deficit(&"user", start);
        let _ = buckets.time_until_tokens(&"user", 1.0, start);
        assert!(buckets.is_empty());
    }

    #[test]
    fn scale_multiplies_burst_and_rate() {
        let scaled = Rate::per_minute(10, 10).scaled(2.0, Rate::MINIMUM);
        assert_eq!(scaled.burst, 20.0);
        assert!((scaled.per_second - 20.0 / 60.0).abs() < 1e-12);
    }

    #[test]
    fn scale_never_goes_below_the_floor() {
        let scaled = Rate::per_minute(10, 10).scaled(0.01, Rate::MINIMUM);
        assert_eq!(scaled, Rate::MINIMUM);
        let byte_floor = Rate::per_second(4.0 * 1024.0 * 1024.0, 64.0 * 1024.0);
        let scaled_bytes =
            Rate::per_second(32.0 * 1024.0 * 1024.0, 1024.0 * 1024.0).scaled(0.001, byte_floor);
        assert_eq!(scaled_bytes, byte_floor);
    }

    #[test]
    fn idle_buckets_are_pruned_and_active_ones_kept() {
        let buckets = KeyedBuckets::new(ten_per_ten_seconds());
        let start = Instant::now();
        buckets.try_take(&u64::MAX, 1.0, start).unwrap(); // idle once refilled
        let later = start + Duration::from_secs(60);
        buckets.try_take(&(u64::MAX - 1), 5.0, later).unwrap(); // still active
        for new_key in 0..PRUNE_EVERY_NEW_KEYS as u64 {
            buckets.try_take(&new_key, 1.0, later).unwrap();
        }
        assert_eq!(buckets.len(), PRUNE_EVERY_NEW_KEYS + 1);
        assert_refused_for(
            buckets.try_take(&(u64::MAX - 1), 6.0, later),
            Duration::from_secs(1),
        );
    }

    #[test]
    fn keys_past_the_cap_share_one_overflow_bucket() {
        let buckets = KeyedBuckets::with_max_keys(Rate::per_second(2.0, 1.0), 2);
        let start = Instant::now();
        buckets.try_take(&1, 1.0, start).unwrap();
        buckets.try_take(&2, 1.0, start).unwrap();
        buckets.try_take(&3, 2.0, start).unwrap();
        assert_eq!(buckets.len(), 2, "the map never grows past its cap");
        assert!(
            buckets.try_take(&4, 1.0, start).is_err(),
            "a new key past the cap shares the drained overflow bucket"
        );
        assert_eq!(
            buckets.try_take(&1, 1.0, start),
            Ok(()),
            "known keys keep theirs"
        );
    }

    #[test]
    fn a_full_map_prunes_idle_keys_before_overflowing() {
        let buckets =
            KeyedBuckets::with_max_keys(Rate::per_second(2.0, 1.0), PRUNE_AT_CAP_MIN_NEW_KEYS);
        let start = Instant::now();
        for old_key in 0..PRUNE_AT_CAP_MIN_NEW_KEYS as u64 {
            buckets.try_take(&old_key, 1.0, start).unwrap();
        }
        let later = start + Duration::from_secs(10);
        buckets.try_take(&u64::MAX, 2.0, later).unwrap();
        assert_eq!(buckets.len(), 1, "the idle keys made room");
        assert_eq!(
            buckets.try_take(&(u64::MAX - 1), 2.0, later),
            Ok(()),
            "nothing went to overflow"
        );
    }

    /// A map of two keys left idle at `start + 2.5 s`, with the overflow
    /// bucket drained at `start + 1.5 s` (half a token back by then).
    fn full_map_of_idle_keys_with_a_drained_overflow() -> (KeyedBuckets<u64>, Instant) {
        let buckets = KeyedBuckets::with_max_keys(Rate::per_second(2.0, 0.5), 2);
        let start = Instant::now();
        buckets.try_take(&1, 1.0, start).unwrap();
        buckets.try_take(&2, 1.0, start).unwrap();
        let overflow_drained_at = start + Duration::from_millis(1500);
        buckets.try_take(&3, 2.0, overflow_drained_at).unwrap();
        assert_eq!(buckets.len(), 2, "key 3 went to the overflow bucket");
        (buckets, start + Duration::from_millis(2500))
    }

    #[test]
    fn a_refused_take_on_a_full_map_of_idle_keys_prunes_first() {
        let (buckets, keys_idle_at) = full_map_of_idle_keys_with_a_drained_overflow();
        assert_eq!(
            buckets.try_take(&4, 1.0, keys_idle_at),
            Ok(()),
            "the idle keys are pruned before the drained overflow is read"
        );
        assert_eq!(buckets.len(), 1, "the new key has its own bucket");
    }

    #[test]
    fn reads_on_a_full_map_of_idle_keys_prune_first() {
        let (buckets, keys_idle_at) = full_map_of_idle_keys_with_a_drained_overflow();
        assert_eq!(
            buckets.time_until_tokens(&4, 2.0, keys_idle_at),
            Duration::ZERO,
            "a new key reads a full bucket of its own, not the overflow"
        );
        assert!(buckets.is_empty(), "the read pruned and stored nothing");
    }

    #[test]
    fn a_periodic_prune_during_a_store_keeps_overflow_state_in_the_overflow_bucket() {
        // A key judged by the overflow bucket must be stored back there even
        // when the periodic prune inside the store frees room: otherwise it
        // inherits the other keys' debt as its own bucket.
        let rate = Rate::per_second(10.0, 1.0);
        let filler_count = PRUNE_EVERY_NEW_KEYS - 2;
        let max_keys = PRUNE_EVERY_NEW_KEYS + filler_count;
        let buckets = KeyedBuckets::with_max_keys(rate, max_keys);
        let start = Instant::now();
        // Keys that will be idle (full) again one second after `start`.
        for warmup_index in 0..PRUNE_EVERY_NEW_KEYS - 1 {
            buckets
                .try_take(&format!("warmup-{warmup_index}"), 1.0, start)
                .unwrap();
        }
        // The 1024th new key runs the periodic prune (nothing idle yet).
        let last_prune_at = start + Duration::from_millis(500);
        buckets
            .try_take(&"trigger".to_string(), 1.0, last_prune_at)
            .unwrap();
        for filler_index in 0..filler_count {
            buckets
                .try_take(&format!("filler-{filler_index}"), 1.0, last_prune_at)
                .unwrap();
        }
        assert_eq!(buckets.len(), max_keys, "the map is full");
        // The overflow bucket goes into debt (no at-cap prune is due yet).
        let overflow_debt_at = last_prune_at + Duration::from_millis(100);
        buckets.charge(&"attacker".to_string(), 15.0, overflow_debt_at);
        // The warmup keys are idle now; this store is the 1024th new key
        // since the last prune, so its periodic prune frees their room.
        let victim_at = last_prune_at + Duration::from_millis(600);
        buckets.charge(&"victim".to_string(), 1.0, victim_at);
        assert!(buckets.len() < max_keys, "the periodic prune freed room");
        assert_eq!(
            buckets.deficit(&"victim".to_string(), victim_at),
            Duration::ZERO,
            "the victim's charge went to the overflow bucket, not its own slot"
        );
    }

    #[test]
    fn a_cost_above_the_burst_is_taken_from_a_full_bucket_into_debt() {
        let buckets = KeyedBuckets::new(Rate::per_second(5.0, 1.0));
        let start = Instant::now();
        assert_eq!(buckets.try_take(&"user", 8.0, start), Ok(()));
        assert_close(buckets.deficit(&"user", start), Duration::from_secs(3));
        assert_refused_for(
            buckets.try_take(&"user", 8.0, start),
            Duration::from_secs(8),
        );
    }

    #[test]
    fn retry_after_saturates_on_an_endless_wait() {
        assert_eq!(retry_after_secs(Duration::MAX), u64::MAX);
        let never_refills = KeyedBuckets::new(Rate::per_minute(1, 0));
        let start = Instant::now();
        never_refills.try_take(&"user", 1.0, start).unwrap();
        let wait = never_refills.try_take(&"user", 1.0, start).unwrap_err();
        assert_eq!(retry_after_secs(wait), u64::MAX);
    }

    #[test]
    fn only_the_first_refusal_of_a_limited_stretch_is_new() {
        let buckets = KeyedBuckets::new(Rate::per_second(1.0, 1.0));
        let start = Instant::now();
        assert_eq!(buckets.try_take_noting_start(&"user", 1.0, start), Ok(()));
        let first_refusal = buckets
            .try_take_noting_start(&"user", 1.0, start)
            .unwrap_err();
        assert!(first_refusal.newly_limited);
        assert_close(first_refusal.wait, Duration::from_secs(1));
        let second_refusal = buckets
            .try_take_noting_start(&"user", 1.0, start)
            .unwrap_err();
        assert!(!second_refusal.newly_limited);
        // A success ends the stretch; the next refusal starts a new one.
        let one_second_later = start + Duration::from_secs(1);
        assert_eq!(
            buckets.try_take_noting_start(&"user", 1.0, one_second_later),
            Ok(())
        );
        let refusal_after_success = buckets
            .try_take_noting_start(&"user", 1.0, one_second_later)
            .unwrap_err();
        assert!(refusal_after_success.newly_limited);
    }

    #[test]
    fn noting_a_refusal_takes_nothing_and_stores_no_new_key() {
        let buckets = KeyedBuckets::new(Rate::per_second(2.0, 1.0));
        let start = Instant::now();
        buckets.try_take_noting_start(&"user", 2.0, start).unwrap();
        assert!(buckets.try_take_noting_start(&"user", 1.0, start).is_err());
        let half_a_second_later = start + Duration::from_millis(500);
        // Still 0.5 tokens: the refusal took nothing.
        let refusal = buckets
            .try_take_noting_start(&"user", 1.0, half_a_second_later)
            .unwrap_err();
        assert_close(refusal.wait, Duration::from_millis(500));
        assert_eq!(buckets.len(), 1);
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds_at_least_one() {
        assert_eq!(retry_after_secs(Duration::ZERO), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_secs(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1001)), 2);
        assert_eq!(retry_after_secs(Duration::from_millis(6200)), 7);
        assert_eq!(retry_after_secs(Duration::from_secs(7)), 7);
    }
}
