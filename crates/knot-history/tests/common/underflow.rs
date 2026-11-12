//! yrs 0.27.3's skip underflow (`block_store.rs:318`), minimised from what
//! `find_the_skip_underflow` (`tests/replay.rs`, M1.2's property generator
//! with its `has_gaps` guards removed) found: on a debug build it panics
//! applying a reconnect resend to the room doc at seed 237, step 36 (and a
//! client's pull of the server's state at seed 80 first). Rebuilt by hand
//! with plain yrs docs:
//!
//! - client 1 makes paragraph A "keep" and paragraph B "x" (1#0..1#9);
//! - client 2 types "ab" at the end of B's text (T1, 2#0..2#2), then "c" at
//!   its start (T2, 2#2), which depends only on client 1's text, not on T1;
//! - client 2 removes B, so its doc GCs B's text and everything in it, and
//!   squashes 2#0..2#3 into one GC block.
//!
//! The doc applied client 1's paragraphs and T2 only: it holds 2#2 past a
//! gap, behind a Skip over 2#0..2#2 (state vector `2: 0`). The update is
//! client 2's resend against that state vector: one GC block 2#0..2#3 and
//! the delete set. The GC block meets the doc's Skip 2#0..2#2 and runs past
//! its end into 2#2, which the doc holds. yrs trims what a doc holds before
//! integrating (`BlockSet::exclude`), but splitting a GC block
//! (`Block::splice`, `block.rs:355-360`) returns its right half without
//! shortening the left one, so the whole 2#0..2#3 is pushed into the Skip
//! and `skip.next_clock() - block.next_clock()` (2 − 3) underflows: a panic
//! in debug builds, a wrapped Skip that corrupts the block list in release
//! builds.
//!
//! The room state is client 2's full state: it holds the update's content
//! squashed with the blocks after it (the same GC block 2#0..2#3, which
//! covers T2's 2#2), so a probe of it on the doc meets the same Skip.

/// The doc's full state, the update, and the room's full state.
pub fn skip_underflow_case() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (
        unhex(
            "020202000a02440108016306010007010764656661756c740309706172616772617068070001000604\
             000101046b6565708701000309706172616772617068070001060604000107017800",
        ),
        unhex("010102000003020101060302010003"),
        unhex(
            "02010200000305010007010764656661756c740309706172616772617068070001000604000101046b\
             656570810100010002020101060302010003",
        ),
    )
}

fn unhex(s: &str) -> Vec<u8> {
    let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    s.chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex"))
        .collect()
}
