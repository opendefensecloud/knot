//! Properties of the sort-key generator, driven through the public
//! `sort_key_between` that the routes call.
//!
//! Each test failed against the old generator:
//! - repeated "New page" went `m, b, 5, 2, 1, 0`, then returned `"0m"` twice;
//! - when `a` was a prefix of `b`, it ignored `b`'s suffix
//!   (`between("m", "m01") == "m0h"`, above `b`), so "insert after" the same
//!   page returned its neighbour on the 7th try;
//! - every append added one character.

use knot_storage::sort_key_between;

const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// A valid key: `^[0-9a-z]*[1-9a-z]$`.
fn valid(k: &str) -> bool {
    k.bytes()
        .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
        && k.bytes().last().is_some_and(|c| c != b'0')
}

/// `sort_key_between(a, b)`, asserting the result is valid and strictly
/// between its bounds (an open bound is unbounded).
fn check(a: Option<&str>, b: Option<&str>) -> String {
    let r = sort_key_between(a, b);
    assert!(
        valid(&r),
        "between({a:?}, {b:?}) = {r:?} is not a valid key"
    );
    assert!(
        a.is_none_or(|lo| lo < r.as_str()),
        "between({a:?}, {b:?}) = {r:?} is not above a"
    );
    assert!(
        b.is_none_or(|hi| r.as_str() < hi),
        "between({a:?}, {b:?}) = {r:?} is not below b"
    );
    r
}

/// Every string over `alphabet` with 1 to `max_len` characters.
fn strings(alphabet: &[u8], max_len: usize) -> Vec<String> {
    let mut all = Vec::new();
    let mut current = vec![String::new()];
    for _ in 0..max_len {
        current = current
            .iter()
            .flat_map(|p| alphabet.iter().map(move |&c| format!("{p}{}", c as char)))
            .collect();
        all.extend(current.iter().cloned());
    }
    all
}

/// "New page" puts the page first at its level: `between(None, first)`.
/// Each key is strictly below the previous first, so all are distinct and
/// descending.
#[test]
fn top_inserts_never_collide() {
    let mut first = sort_key_between(None, None);
    for _ in 0..2000 {
        first = check(None, Some(&first));
    }
}

/// "Insert after" the same page, again and again: each new key goes between
/// the anchor and the key right after it, which is the previous insert.
#[test]
fn inserting_after_the_same_key_never_collides() {
    let anchor = sort_key_between(None, None);
    let mut next: Option<String> = None;
    for _ in 0..50 {
        next = Some(check(Some(&anchor), next.as_deref()));
    }
}

/// `b` extends `a` with zero or more `'0'`s and one more digit, up to three
/// characters: `("m", "m1")`, `("m", "m01")`, `("01", "01z")`, …
#[test]
fn prefix_family_stays_inside() {
    for a in strings(ALPHABET, 2).iter().filter(|k| valid(k)) {
        for zeros in 0..=2 - a.len() {
            for &c in &ALPHABET[1..] {
                let b = format!("{a}{}{}", "0".repeat(zeros), c as char);
                check(Some(a), Some(&b));
            }
        }
    }
}

/// Every valid pair over a small alphabet that keeps both edges and the
/// middle, with `a` up to 2 and `b` up to 3 characters, plus each key as a
/// lone bound. On the old generator, pairs of up to 2 characters stayed
/// inside; the third character of `b` exposes `between("m", "m01") == "m0h"`.
#[test]
fn exhaustive_small_alphabet() {
    let alphabet = b"0129amyz";
    let a_keys: Vec<String> = strings(alphabet, 2)
        .into_iter()
        .filter(|k| valid(k))
        .collect();
    let b_keys: Vec<String> = strings(alphabet, 3)
        .into_iter()
        .filter(|k| valid(k))
        .collect();
    let mut failures = Vec::new();
    let mut probe = |a: Option<&str>, b: Option<&str>| {
        let r = sort_key_between(a, b);
        let inside = a.is_none_or(|lo| lo < r.as_str()) && b.is_none_or(|hi| r.as_str() < hi);
        if !valid(&r) || !inside {
            failures.push(format!("between({a:?}, {b:?}) = {r:?}"));
        }
    };
    for a in &a_keys {
        for b in b_keys.iter().filter(|b| a < *b) {
            probe(Some(a), Some(b));
        }
    }
    for k in &b_keys {
        probe(None, Some(k));
        probe(Some(k), None);
    }
    assert!(
        failures.is_empty(),
        "{} wrong results, e.g. {:?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );
}

/// xorshift64: a deterministic generator, so a failing seed reproduces.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// Random sessions on one sibling level, 400 operations each: "New page"
/// (first at its level), "insert after" a random page, and moving a random
/// page to a random slot. Every key lands strictly between its neighbours,
/// so the level stays unique and ordered.
#[test]
fn simulation_2000_seeds() {
    for seed in 1..=2000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut level: Vec<String> = Vec::new();
        for _ in 0..400 {
            let pos = match rng.below(3) {
                0 => 0,
                1 => rng.below(level.len() + 1),
                _ => {
                    if !level.is_empty() {
                        level.remove(rng.below(level.len()));
                    }
                    rng.below(level.len() + 1)
                }
            };
            let a = pos.checked_sub(1).map(|i| level[i].as_str());
            let b = level.get(pos).map(String::as_str);
            let key = check(a, b);
            level.insert(pos, key);
        }
        assert!(
            level.windows(2).all(|w| w[0] < w[1]),
            "seed {seed}: level is not strictly ascending"
        );
    }
}

/// Appending at the end of a level (drop "into", from-template, restore).
/// The old generator added one character per append.
#[test]
fn appends_grow_slowly() {
    let mut last = sort_key_between(None, None);
    for _ in 0..1000 {
        last = check(Some(&last), None);
    }
    assert!(
        last.len() <= 210,
        "1000 appends gave a {}-character key",
        last.len()
    );
}
