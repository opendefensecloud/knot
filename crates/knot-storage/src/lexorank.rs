//! Sort keys: the strings that order a page among its siblings.
//!
//! A key is a base-36 fraction over `[0-9a-z]`, compared code unit by code
//! unit (`COLLATE "C"`, the order the sidebar sorts in). A valid key matches
//! `^[0-9a-z]*[1-9a-z]$`. It never ends in `'0'`, so another key always fits
//! between two valid keys.
//!
//! - `between(None, None)` is `"m"`.
//! - `between(Some(a), Some(b))` with `a < b` returns a valid key strictly
//!   between them, `between(Some(a), None)` one above `a`, and
//!   `between(None, Some(b))` one below `b`.
//! - Invalid or unordered neighbours give an `Err`, never a panic.
//! - `spread(n)` gives `n` evenly spaced keys, for rewriting a whole group.
//!
//! The generator is the fractional-index midpoint (rocicorp
//! "fractional-indexing", without its integer head).

/// Base-36 alphabet: '0'..='9' then 'a'..='z'. Index → char.
const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// The first key of an empty group, leaving room on both sides.
const MID: &str = "m";

/// Why no key could be generated between two neighbours.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// A neighbour is not a valid key: empty, a character outside
    /// `[0-9a-z]`, or a trailing `'0'`.
    #[error("invalid sort key {0:?}")]
    Invalid(String),
    /// The lower neighbour is not below the upper one.
    #[error("sort keys are not in order")]
    Unordered,
}

/// Whether `k` matches `^[0-9a-z]*[1-9a-z]$`.
pub fn is_valid(k: &str) -> bool {
    k.bytes()
        .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
        && k.bytes().last().is_some_and(|c| c != b'0')
}

/// A valid key strictly between `a` and `b`, where `None` is an open end.
///
/// `Invalid` when a neighbour is not a valid key, `Unordered` when `a >= b`.
pub fn between(a: Option<&str>, b: Option<&str>) -> Result<String, KeyError> {
    for k in [a, b].into_iter().flatten() {
        if !is_valid(k) {
            return Err(KeyError::Invalid(k.to_string()));
        }
    }
    match (a, b) {
        (Some(a), Some(b)) if a >= b => Err(KeyError::Unordered),
        (None, None) => Ok(MID.to_string()),
        _ => Ok(midpoint(a.unwrap_or("").as_bytes(), b.map(str::as_bytes))),
    }
}

/// The value in [0, 36) of a byte of a validated key.
fn digit(c: u8) -> usize {
    match c {
        b'0'..=b'9' => (c - b'0') as usize,
        b'a'..=b'z' => (c - b'a') as usize + 10,
        _ => unreachable!("keys are validated before use"),
    }
}

/// The midpoint of valid keys `a < b`. `a` reads as padded with `'0'`, so an
/// empty `a` is the bottom end; a missing `b` is the top end.
fn midpoint(mut a: &[u8], mut b: Option<&[u8]>) -> String {
    let mut out = Vec::new();
    if let Some(upper) = b {
        // Keep the common prefix. It is shorter than `b`: otherwise `b` is a
        // prefix of `a` (so `b <= a`) or `a` plus '0's (so it ends in '0').
        let n = upper
            .iter()
            .zip(a.iter().chain(std::iter::repeat(&b'0')))
            .take_while(|(x, y)| x == y)
            .count();
        out.extend_from_slice(&upper[..n]);
        a = a.get(n..).unwrap_or_default();
        b = Some(&upper[n..]);
    }
    loop {
        // Past the prefix, a's first digit is below b's.
        let da = a.first().map_or(0, |&c| digit(c));
        let db = b.map_or(ALPHABET.len(), |b| digit(b[0]));
        if db - da > 1 {
            // A digit strictly between the two. It is above da, so not '0'.
            out.push(ALPHABET[(da + db).div_ceil(2)]);
            break;
        }
        if let Some(b) = b.filter(|b| b.len() > 1) {
            // b's first digit alone: below b, which goes on, and above a.
            out.push(b[0]);
            break;
        }
        // Adjacent digits: keep a's, then find room above the rest of `a`.
        out.push(ALPHABET[da]);
        a = a.get(1..).unwrap_or_default();
        b = None;
    }
    String::from_utf8(out).expect("keys are ASCII")
}

/// `n` evenly spaced valid keys in ascending order, for rewriting a group.
///
/// With `w` the smallest width where `36^w > 2(n+1)`, key `i` (from 1) is
/// `floor(i * 36^w / (n+1))` written as `w` base-36 digits, left-padded with
/// `'0'`, with its trailing `'0'`s stripped. At a fixed width, stripping
/// keeps the keys distinct and in order. The heal migration's SQL computes
/// the same keys.
pub fn spread(n: usize) -> Vec<String> {
    let slots = n as u128 + 1;
    let mut width = 1u32;
    while 36u128.pow(width) <= 2 * slots {
        width += 1;
    }
    let space = 36u128.pow(width);
    (1..slots)
        .map(|i| {
            let mut v = i * space / slots;
            let mut key = vec![b'0'; width as usize];
            for c in key.iter_mut().rev() {
                *c = ALPHABET[(v % 36) as usize];
                v /= 36;
            }
            while key.last() == Some(&b'0') {
                key.pop();
            }
            String::from_utf8(key).expect("keys are ASCII")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_between(a: Option<&str>, b: Option<&str>) {
        let r = between(a, b).unwrap();
        if let Some(av) = a {
            assert!(av < r.as_str(), "expected {av} < {r}");
        }
        if let Some(bv) = b {
            assert!(r.as_str() < bv, "expected {r} < {bv}");
        }
        assert!(is_valid(&r), "{r} not valid");
    }

    #[test]
    fn empty_returns_middle() {
        assert_eq!(between(None, None).unwrap(), "m");
    }

    #[test]
    fn after_only_extends_or_grows() {
        check_between(Some("m"), None);
        check_between(Some("z"), None);
    }

    #[test]
    fn before_only_decrements() {
        check_between(None, Some("m"));
        check_between(None, Some("a"));
    }

    #[test]
    fn adjacent_chars_descend_into_suffix() {
        check_between(Some("a"), Some("b"));
        check_between(Some("m"), Some("n"));
    }

    #[test]
    fn distant_chars_pick_midpoint() {
        let r = between(Some("a"), Some("z")).unwrap();
        assert!("a" < r.as_str() && r.as_str() < "z");
    }

    #[test]
    fn many_inserts_between_two_anchors_stay_monotone() {
        let mut a = "a".to_string();
        let b = "z";
        for _ in 0..50 {
            let next = between(Some(&a), Some(b)).unwrap();
            assert!(a.as_str() < next.as_str() && next.as_str() < b);
            a = next;
        }
    }

    #[test]
    fn invalid_inputs_are_errors_not_panics() {
        for bad in ["", "0", "00", "m0", "M", "-"] {
            let err = Err(KeyError::Invalid(bad.to_string()));
            assert_eq!(between(Some(bad), None), err, "after {bad:?}");
            assert_eq!(between(None, Some(bad)), err, "before {bad:?}");
            assert_eq!(between(Some(bad), Some("z")), err, "{bad:?} as a");
            assert_eq!(between(Some("1"), Some(bad)), err, "{bad:?} as b");
        }
        assert_eq!(between(Some("m"), Some("m")), Err(KeyError::Unordered));
        assert_eq!(between(Some("n"), Some("m")), Err(KeyError::Unordered));
    }

    #[test]
    fn spread_is_sorted_valid_distinct() {
        for n in [1, 2, 10, 35, 36, 100, 5000] {
            let keys = spread(n);
            assert_eq!(keys.len(), n, "spread({n})");
            assert!(keys.iter().all(|k| is_valid(k)), "spread({n}): {keys:?}");
            assert!(
                keys.windows(2).all(|w| w[0] < w[1]),
                "spread({n}) is not strictly ascending"
            );
        }
        // The heal migration's SQL reproduces this formula, so pin it.
        assert_eq!(spread(1), ["i"]);
        assert_eq!(spread(2), ["c", "o"]);
        assert_eq!(
            spread(10),
            ["3", "6", "9", "d", "g", "j", "m", "q", "t", "w"]
        );
        let all_digits: Vec<String> = ALPHABET[1..]
            .iter()
            .map(|&c| (c as char).to_string())
            .collect();
        assert_eq!(spread(35), all_digits);
        let s36 = spread(36);
        assert_eq!((s36[0].as_str(), s36[35].as_str()), ("0z", "z"));
        assert!(spread(5000).iter().all(|k| k.len() <= 3));
    }
}
