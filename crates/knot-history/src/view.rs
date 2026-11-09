//! A past version of a page, as the history viewer shows it (M1.4): the
//! entry's replay rendered as ProseMirror JSON, plain ([`pm_json`]) or with
//! the entry's own changes marked ([`pm_json_diff`]), and the input caps
//! that keep a view within the builder's memory rule.
//!
//! This part is pure (no database, no tokio): [`render`] runs on the
//! replay thread (`isolate::run_isolated`) under a compute permit.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use serde_json::Value;
use yrs::IdSet;

use crate::diff::{DiffError, entry_diff};
use crate::pass::{WindowError, WindowInput, replay_window};
use crate::pm_json::{pm_json, pm_json_diff};
use crate::replay::{ReplayMode, Replayer};

/// M1.3's doc-memory factor F, which `DEFAULT_HISTORY_REPLAY_MAX_BYTES`
/// was set by: a build's peak heap plus its input, over its input, at
/// most 40, so `workers × 40 × cap` bounds the builder's replay memory.
pub const BUILD_MEMORY_FACTOR: u64 = 40;

/// The Show changes view's memory factor: (input + peak heap above the
/// start) / input of a `changes=1` render, which holds the skip-GC replay,
/// the union doc, `D_lo`, the JSON `Value` and its serialization at once.
/// Set by M1.4 Task 3's spike (`cargo run --release -p knot-history
/// --example replay_throughput 7`).
///
/// On workload 6's 4 MiB pages, a session of a few rows measured 59.7
/// (interleaved one-character typing) and 5.1 (pastes). But the render
/// emits one text node per changed *item*: yrs 0.27.3's `DiffAssembler`
/// starts a chunk at every added or removed item (`types/text.rs:617-631`),
/// and each node costs about 3.5 KiB of heap. So a session whose one row
/// deletes the whole typing page (400,020 struck text nodes) peaked
/// 1,321 MiB above its start, F = 317.1 (321.1 at 1.36 MiB); one that
/// undoes one client's typing (every third character) measured 173.6; and
/// `tests/diff_memory.rs`'s page, whose client ids 1-3 encode denser than
/// real 32-bit ids, with a third of it highlighted, 327.5. Pastes stay
/// near 5 in every shape.
///
/// F grows on smaller pages, because fixed costs weigh more. So the factor
/// is set where the rule binds, on a typing page of the diff cap's own
/// size. With 360 the diff cap of the default 4 MiB replay cap is
/// 4 MiB × 40 / 360 = 0.44 MiB. There, the whole-page deletion measured
/// F = 338.5 and held 151.8 MiB (input + peak), within the builder's
/// 40 × 4 MiB = 160 MiB. 330 would give a 0.48 MiB cap, where it measured
/// 343.4 and 168.6 MiB.
///
/// The render time grows with texts × deleted blocks. The whole-page
/// deletion took 26.2 s at 4.18 MiB and 2.8 s at 1.36 MiB, but only 0.38 s
/// at the diff cap, well under the 10 s the spike allows.
pub const VIEW_MEMORY_FACTOR: u64 = 360;

/// The plain view's and the restore's memory factor (M1.4 Task 3's spike,
/// as [`VIEW_MEMORY_FACTOR`]), so the plain cap is the full replay cap.
///
/// On workload 6's 4 MiB pages (interleaved typing / pastes): a plain view
/// of a session's window measured 39.1 / 4.6; a baseline view of the state
/// 39.1 / 4.6; and a restore (the replay, `encode_state`, and the Markdown
/// round trip of `apply_update`, `to_markdown::serialise` and
/// `from_markdown::parse`) 39.1 / 6.7. A 4 MiB page whose every other word
/// is bold, one text node per word, measured 16.4. The decoded checkpoint
/// dominates, as in M1.3's F. Each took at most 0.14 s, well within
/// `RESTORE_TIMEOUT` − 5 s.
pub const PLAIN_MEMORY_FACTOR: u64 = 40;

/// The largest replay input a view may load (the diff cap with `changes`,
/// the plain cap without): `replay_max_bytes × BUILD_MEMORY_FACTOR /
/// max(F, BUILD_MEMORY_FACTOR)` with F = [`VIEW_MEMORY_FACTOR`] or
/// [`PLAIN_MEMORY_FACTOR`], so a view holding a compute permit takes no
/// more than the builder's `40 × replay_max_bytes`. Never above
/// `replay_max_bytes`; saturates at `i64::MAX` (the database's sum type).
pub fn view_max_input_bytes(replay_max_bytes: u64, changes: bool) -> i64 {
    let f = if changes {
        VIEW_MEMORY_FACTOR
    } else {
        PLAIN_MEMORY_FACTOR
    };
    let cap = u128::from(replay_max_bytes) * u128::from(BUILD_MEMORY_FACTOR)
        / u128::from(f.max(BUILD_MEMORY_FACTOR));
    i64::try_from(cap).unwrap_or(i64::MAX)
}

/// What a view replays.
// Built once per render and moved into the replay thread; a box would only
// add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum ViewInput {
    /// A baseline's state, rendered as it is.
    State(Vec<u8>),
    /// An entry's window: the checkpoint at or before its start, the rows
    /// through its end.
    Window(WindowInput),
}

/// Why a version could not be rendered at all.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The baseline state does not decode or apply.
    #[error("the state does not apply: {0}")]
    State(String),
    /// The window replay failed or was cancelled.
    #[error("the replay failed: {0}")]
    Window(#[from] WindowError),
    /// The diff step was cancelled ([`DiffError::Cancelled`], the only
    /// variant it carries: every other diff failure falls back to the plain
    /// render, [`Rendered::diff_error`]).
    #[error("the render failed: {0}")]
    Diff(#[from] DiffError),
}

/// A rendered version.
#[derive(Debug)]
pub struct Rendered {
    /// ProseMirror JSON, `{"type": "doc", "content": [...]}`.
    pub doc: Value,
    /// Whether the entry's own changes are marked in `doc`.
    pub changes: bool,
    /// Why the diff step failed or panicked, when changes were asked for
    /// and `doc` is the plain render instead.
    pub diff_error: Option<String>,
}

/// Test hooks; production code leaves them at their default.
#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RenderHooks {
    /// Panics inside the diff step.
    pub panic_in_diff: bool,
    /// Sleeps inside the diff step, before the union doc is built; like
    /// the union build it stands for, the sleep does not see a cancel.
    pub hold_in_diff: Duration,
}

/// Renders `input`. A [`ViewInput::State`] renders plain (`changes` is
/// ignored). A [`ViewInput::Window`] is replayed with its own `progress`;
/// with `changes` (the entry's insert and delete sets) the diff step
/// ([`entry_diff`], then [`pm_json_diff`]) runs under `catch_unwind`, and
/// when it fails, other than by a cancel, or panics, the version is the
/// plain render of the same replay with the reason in
/// [`Rendered::diff_error`]: the diff step only reads the replay, so the
/// replay is intact after the unwind.
pub fn render(
    input: ViewInput,
    changes: Option<(IdSet, IdSet)>,
    hooks: RenderHooks,
) -> Result<Rendered, RenderError> {
    let window = match input {
        ViewInput::State(bytes) => {
            let r = Replayer::from_state(ReplayMode::KeepDeleted, &bytes)
                .map_err(|e| RenderError::State(e.to_string()))?;
            return Ok(plain(r.doc(), None));
        }
        ViewInput::Window(w) => w,
    };
    let progress = window.progress.clone();
    let replay = replay_window(window)?;
    let h = replay.doc();
    let Some((inserted, deleted)) = changes else {
        return Ok(plain(h, None));
    };
    let diffed = catch_unwind(AssertUnwindSafe(|| {
        if hooks.panic_in_diff {
            panic!("test hook: panic in the diff");
        }
        if !hooks.hold_in_diff.is_zero() {
            std::thread::sleep(hooks.hold_in_diff);
        }
        let d = entry_diff(h, &inserted, &deleted)?;
        pm_json_diff(h, &d, &progress)
    }));
    match diffed {
        Ok(Ok(doc)) => Ok(Rendered {
            doc,
            changes: true,
            diff_error: None,
        }),
        Ok(Err(DiffError::Cancelled)) => Err(RenderError::Diff(DiffError::Cancelled)),
        Ok(Err(e)) => Ok(plain(h, Some(e.to_string()))),
        Err(panic) => Ok(plain(
            h,
            Some(format!("the diff panicked: {}", panic_message(&*panic))),
        )),
    }
}

fn plain(h: &yrs::Doc, diff_error: Option<String>) -> Rendered {
    Rendered {
        doc: pm_json(h),
        changes: false,
        diff_error,
    }
}

/// A panic payload's message, as `std`'s default hook prints it.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a non-string payload")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::{TimeZone, Utc};
    use knot_storage::{DocUpdate, UpdateSource};
    use uuid::Uuid;
    use yrs::updates::decoder::Decode;
    use yrs::{
        Doc, IdSet, ReadTxn, Text, Transact, Update, XmlElementPrelim, XmlFragment, XmlOut,
        XmlTextPrelim,
    };

    use super::*;
    use crate::pass::{Progress, ReplayHooks};

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn the_view_cap_keeps_the_build_memory_rule() {
        let cap = 4 * MIB;
        let diff = view_max_input_bytes(cap, true);
        let plain = view_max_input_bytes(cap, false);
        assert_eq!(
            diff,
            i64::try_from(cap * 40 / VIEW_MEMORY_FACTOR.max(40)).unwrap()
        );
        assert_eq!(
            plain,
            i64::try_from(cap * 40 / PLAIN_MEMORY_FACTOR.max(40)).unwrap()
        );
        assert!(
            plain >= diff,
            "the plain cap {plain} under the diff cap {diff}"
        );
        assert!(plain <= i64::try_from(cap).unwrap());
        assert!(diff <= i64::try_from(cap).unwrap());
        // `workers × F × cap` stays what the builder's F allows.
        for (f, c) in [(VIEW_MEMORY_FACTOR, diff), (PLAIN_MEMORY_FACTOR, plain)] {
            assert!(u64::try_from(c).unwrap() * f.max(BUILD_MEMORY_FACTOR) <= cap * 40);
        }
        // A cap too large for the arithmetic saturates instead of wrapping.
        assert_eq!(view_max_input_bytes(u64::MAX, false), i64::MAX);
        assert!(view_max_input_bytes(u64::MAX, true) > 0);
    }

    /// A doc of two paragraphs, as a state.
    fn two_paragraphs() -> (Doc, Vec<u8>) {
        let doc = Doc::with_client_id(7);
        let frag = doc.get_or_insert_xml_fragment("default");
        {
            let mut txn = doc.transact_mut();
            for s in ["one", "two"] {
                let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(&mut txn, XmlTextPrelim::new(s));
            }
        }
        let state = doc
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        (doc, state)
    }

    #[test]
    fn a_state_input_renders_pm_json() {
        let (doc, state) = two_paragraphs();
        let r = render(ViewInput::State(state), None, RenderHooks::default()).expect("the render");
        assert_eq!(r.doc, pm_json(&doc));
        assert!(!r.changes);
        assert_eq!(r.diff_error, None);
        assert_eq!(r.doc["content"].as_array().map(Vec::len), Some(2));

        let r = render(
            ViewInput::State(vec![0xff, 0x01]),
            None,
            RenderHooks::default(),
        );
        assert!(matches!(r, Err(RenderError::State(_))), "{r:?}");
    }

    fn row(rev: i64, by: Uuid, update_bytes: Vec<u8>) -> DocUpdate {
        DocUpdate {
            seq: rev,
            doc_id: Uuid::nil(),
            update_bytes,
            by_user_id: Some(by),
            created_at: Utc.with_ymd_and_hms(2026, 11, 8, 9, 0, 0).unwrap()
                + chrono::Duration::seconds(rev),
            rev: Some(rev),
            source: Some(UpdateSource::Edit),
            meta: None,
            epoch: Some(0),
        }
    }

    /// Two authors on one page: A writes a paragraph, B appends to it, A
    /// adds a second one. The window input from an empty checkpoint, and
    /// A's merged insert and delete sets.
    fn two_authors() -> (WindowInput, (IdSet, IdSet)) {
        let a = Uuid::from_u128(0xa);
        let b = Uuid::from_u128(0xb);
        let docs = [Doc::with_client_id(1), Doc::with_client_id(2)];
        let mut rows = Vec::new();
        let mut write = |who: usize, f: &dyn Fn(&Doc)| {
            let before = docs[who].transact().state_vector();
            f(&docs[who]);
            let update = docs[who].transact().encode_state_as_update_v1(&before);
            let other = &docs[1 - who];
            other
                .transact_mut()
                .apply_update(Update::decode_v1(&update).unwrap())
                .unwrap();
            let by = if who == 0 { a } else { b };
            rows.push(row(rows.len() as i64 + 1, by, update));
        };
        write(0, &|d| {
            let frag = d.get_or_insert_xml_fragment("default");
            let mut txn = d.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("alpha"));
        });
        write(1, &|d| {
            let frag = d.get_or_insert_xml_fragment("default");
            let mut txn = d.transact_mut();
            let Some(XmlOut::Element(p)) = frag.get(&txn, 0) else {
                panic!("a paragraph");
            };
            let Some(XmlOut::Text(t)) = p.get(&txn, 0) else {
                panic!("a text");
            };
            t.insert(&mut txn, 5, " beta");
        });
        write(0, &|d| {
            let frag = d.get_or_insert_xml_fragment("default");
            let mut txn = d.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("gamma"));
        });

        let mut replay = Replayer::new(ReplayMode::KeepDeleted);
        let (mut ins, mut del) = (IdSet::new(), IdSet::new());
        for r in &rows {
            let e = replay.apply(&r.update_bytes);
            if r.by_user_id == Some(a) {
                ins.merge_with(e.inserted);
                del.merge_with(e.deleted);
            }
        }
        let end_rev = rows.len() as i64;
        let input = WindowInput {
            mode: ReplayMode::KeepDeleted,
            checkpoint_state: Replayer::new(ReplayMode::KeepDeleted).encode_state(),
            reapply: Vec::new(),
            rows,
            gap_states: Vec::new(),
            skip_revs: BTreeSet::new(),
            end_rev,
            merge_gap_at_end: false,
            progress: Progress::default(),
            hooks: ReplayHooks::default(),
        };
        (input, (ins, del))
    }

    /// Runs `f` with the panic hook silenced, so a deliberate panic prints
    /// nothing.
    fn quietly<T>(f: impl FnOnce() -> T) -> T {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out = f();
        std::panic::set_hook(hook);
        out
    }

    #[test]
    fn a_window_renders_with_and_without_changes() {
        let (input, sets) = two_authors();
        let plain = render(ViewInput::Window(input), None, RenderHooks::default()).unwrap();
        assert!(!plain.changes);
        assert_eq!(plain.diff_error, None);

        let (input, _) = two_authors();
        let diff = render(ViewInput::Window(input), Some(sets), RenderHooks::default()).unwrap();
        assert!(diff.changes, "{:?}", diff.diff_error);
        assert_eq!(diff.diff_error, None);
        assert_ne!(diff.doc, plain.doc, "A's changes are marked");
        assert!(
            diff.doc
                .to_string()
                .contains(crate::pm_json::HISTORY_INSERT)
        );
    }

    #[test]
    fn a_diff_that_panics_falls_back_to_the_plain_render() {
        let (input, sets) = two_authors();
        let hooks = RenderHooks {
            panic_in_diff: true,
            ..RenderHooks::default()
        };
        let r = quietly(|| render(ViewInput::Window(input), Some(sets), hooks))
            .expect("the plain render");
        assert!(!r.changes);
        let reason = r.diff_error.expect("the diff's failure");
        assert!(reason.contains("panicked"), "{reason}");

        let (input, _) = two_authors();
        let plain = render(ViewInput::Window(input), None, RenderHooks::default()).unwrap();
        assert_eq!(r.doc, plain.doc);
    }

    #[test]
    fn a_cancelled_diff_is_an_error_not_a_fallback() {
        let (input, sets) = two_authors();
        let progress = input.progress.clone();
        let hooks = RenderHooks {
            hold_in_diff: Duration::from_secs(2),
            ..RenderHooks::default()
        };
        let render =
            std::thread::spawn(move || render(ViewInput::Window(input), Some(sets), hooks));
        // The three-row replay is long done; the diff step is holding.
        std::thread::sleep(Duration::from_millis(500));
        progress.cancel();
        let r = render.join().expect("no panic");
        assert!(
            matches!(r, Err(RenderError::Diff(DiffError::Cancelled))),
            "{r:?}"
        );
    }
}
