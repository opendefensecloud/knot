//! LexoRank-style sort_key generation. Single-bucket base-36 (0-9a-z).
//!
//! Properties:
//! - `between(None, None) -> "m"` (start with a middle anchor so future
//!   inserts on both sides remain cheap).
//! - `between(Some(a), None)` returns something > a.
//! - `between(None, Some(b))` returns something < b.
//! - `between(Some(a), Some(b))` where a < b returns a key in (a, b).
//! - Returned keys never end in '0' (so we can always append a digit).

const MIN: char = '0';
const MAX: char = 'z';
const MID: char = 'm';

/// Base-36 alphabet: '0'..='9' then 'a'..='z'. Index → char.
const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Map a base-36 char to its ordinal [0, 36).
fn ord(c: char) -> u8 {
    match c {
        '0'..='9' => c as u8 - b'0',
        'a'..='z' => c as u8 - b'a' + 10,
        _ => panic!("not a base36 char: {c}"),
    }
}

/// Map an ordinal [0, 36) back to a base-36 char.
fn chr(o: u8) -> char {
    ALPHABET[o as usize] as char
}

/// Midpoint of two base-36 chars, in the base-36 ordinal space.
/// Rounds down; result is always in [ord(a), ord(b)].
fn mid(a: char, b: char) -> char {
    let av = ord(a) as u16;
    let bv = ord(b) as u16;
    chr(((av + bv) / 2) as u8)
}

fn is_base36(s: &str) -> bool {
    s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='z'))
}

pub fn between(a: Option<&str>, b: Option<&str>) -> String {
    match (a, b) {
        (None, None) => MID.to_string(),
        (Some(a), None) => append_or_grow(a),
        (None, Some(b)) => decrement(b),
        (Some(a), Some(b)) => {
            assert!(a < b, "between: a must be < b ({a} >= {b})");
            assert!(is_base36(a) && is_base36(b), "base36 only");
            interpolate(a, b)
        }
    }
}

fn append_or_grow(a: &str) -> String {
    let mut s = a.to_string();
    let last = s.chars().last().unwrap_or(MIN);
    if last < MAX {
        s.push(mid(last, MAX));
    } else {
        s.push(MID);
    }
    s
}

fn decrement(b: &str) -> String {
    let bytes = b.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 1);
    for &c in bytes {
        let ch = c as char;
        if ch > MIN {
            let m = mid(MIN, ch);
            out.push(m as u8);
            if m == MIN {
                out.push(MID as u8);
            }
            return String::from_utf8(out).unwrap();
        }
        out.push(c);
    }
    out.push(MID as u8);
    String::from_utf8(out).unwrap()
}

fn interpolate(a: &str, b: &str) -> String {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < a_chars.len() && i < b_chars.len() && a_chars[i] == b_chars[i] {
        out.push(a_chars[i]);
        i += 1;
    }
    let ac = a_chars.get(i).copied().unwrap_or(MIN);
    let bc = b_chars.get(i).copied().unwrap_or(MAX);
    let m = mid(ac, bc);
    if m != ac && m != bc {
        out.push(m);
        return out;
    }
    out.push(ac);
    i += 1;
    loop {
        let ac = a_chars.get(i).copied().unwrap_or(MIN);
        if ac < MAX {
            let m = mid(ac, MAX);
            if m != ac {
                out.push(m);
                return out;
            }
            // mid floored to ac (e.g. mid('y','z')='y'); copy ac and descend.
        }
        out.push(ac);
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_between(a: Option<&str>, b: Option<&str>) {
        let r = between(a, b);
        if let Some(av) = a {
            assert!(av < r.as_str(), "expected {av} < {r}");
        }
        if let Some(bv) = b {
            assert!(r.as_str() < bv, "expected {r} < {bv}");
        }
        assert!(is_base36(&r), "{r} not base36");
    }

    #[test]
    fn empty_returns_middle() {
        assert_eq!(between(None, None), "m");
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
        let r = between(Some("a"), Some("z"));
        assert!("a" < r.as_str() && r.as_str() < "z");
    }

    #[test]
    fn many_inserts_between_two_anchors_stay_monotone() {
        let mut a = "a".to_string();
        let b = "z";
        for _ in 0..50 {
            let next = between(Some(&a), Some(b));
            assert!(a.as_str() < next.as_str() && next.as_str() < b);
            a = next;
        }
    }
}
