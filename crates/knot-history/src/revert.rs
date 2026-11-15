//! Restore here (M2): the update that brings a live page back to a version,
//! computed off the room and verified before anyone applies it.
//!
//! A revert is planned from the version's skip-GC state ([`RevertTarget`],
//! built by [`target`] on the replay thread) and the live room's
//! pending-free export:
//!
//! 1. The plan doc holds the version and applies the live state, under a
//!    fresh client id. What changed since the version is computed as sets
//!    of ids (the blocks the plan doc holds minus those the version holds,
//!    and its delete set minus the version's, Rulings), and yrs's own
//!    `UndoManager` undoes exactly that as one stack item: it deletes what
//!    was inserted since and redoes what was deleted since, so text the
//!    version and the live page share keeps its ids, and with them every
//!    comment and suggestion anchor and its authorship.
//! 2. When the undo panics, captures nothing or several updates, or its
//!    update does not reproduce the version, a whole-page replace runs
//!    instead: on a GC scratch of the live state (fresh client) it clears
//!    `default` and copies every live child of the version's `default` into
//!    it with typed attributes (yrs's `as_prelim` would stringify them,
//!    Rulings).
//!
//! Every update [`plan`] returns has passed [`verify`]: a GC replay of the
//! live state applies it, holds no pending content, and renders exactly
//! as the version ([`rendered`]: its [`pm_json`] as the editor shows it).
//! Other roots of the doc (the applied-
//! suggestions ledger, anything else) are never reverted; those that differ
//! are reported in [`RevertOutcome::Revert::other_roots_changed`].
//!
//! Pure: no database, no tokio, no knot-crdt. States are encoded with
//! [`replay::own_state`](crate::replay) and updates captured with
//! `observe_update_v1`; nothing here calls `Update::merge` or
//! `merge_updates`.

use std::collections::{BTreeMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use yrs::block::ClientID;
use yrs::types::ToJson;
use yrs::types::text::YChange;
use yrs::undo::{Options as UndoOptions, StackItem, UndoManager};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Any, ArrayRef, Doc, IdSet, MapRef, Out, ReadTxn, Text, Transact, TransactionMut, Update, Xml,
    XmlElementPrelim, XmlFragment, XmlOut, XmlTextPrelim,
};

use serde_json::Value;

use crate::pm_json::pm_json;
use crate::replay::{self, ReplayMode, Replayer, RowOutcome};

/// How deep the shared types of a page may nest for a revert: mirrors
/// `knot_crdt::nesting::MAX_NESTING` (knot-history cannot depend on
/// knot-crdt; `crates/knot-crdt/tests/nesting.rs` pins the two equal). A
/// root type is at depth 0, a type in it at depth 1.
pub const MAX_NESTING: usize = 512;

/// The roots a revert never compares or reports: the page itself, which it
/// reverts, and the applied-suggestions ledger, which it leaves alone.
const OWN_ROOTS: [&str; 2] = ["default", "knot_applied_suggestions"];

/// A version to revert to.
#[derive(Debug, Clone)]
pub struct RevertTarget {
    /// The version's skip-GC, pending-free state (deleted content kept).
    pub state: Vec<u8>,
    /// [`rendered`] of the version (its `pm_json`, text nodes joined as the
    /// editor joins them): what a verified revert must render.
    pub json: Vec<u8>,
    /// Whether a type of the version nests deeper than [`MAX_NESTING`].
    pub nests_too_deep: bool,
}

/// The target of a Restore here: `state` (a skip-GC state of the version,
/// as `Views::restore_input` hands it to its `finish`) with its
/// [`rendered`] page and nesting depth.
pub fn target(state: &[u8]) -> Result<RevertTarget, String> {
    let version =
        Replayer::from_state(ReplayMode::KeepDeleted, state).map_err(|e| e.to_string())?;
    Ok(RevertTarget {
        state: state.to_vec(),
        json: rendered(version.doc())?,
        nests_too_deep: nests_deeper_than(version.doc(), MAX_NESTING),
    })
}

/// The content of a Restore as new page: `state` as a GC'd state, so the
/// new page holds none of the version's deleted content.
pub fn copy_state(state: &[u8]) -> Result<Vec<u8>, String> {
    Ok(Replayer::from_state(ReplayMode::Gc, state)
        .map_err(|e| e.to_string())?
        .encode_state())
}

/// How a revert was computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertKind {
    /// yrs's undo of every change since the version: unchanged text keeps
    /// its ids.
    Undo,
    /// The whole-page replace: every block is new, so comments on the page
    /// may lose their place.
    Fallback,
}

impl RevertKind {
    /// The `source` of the row that persists the revert.
    pub fn source(self) -> knot_storage::UpdateSource {
        match self {
            RevertKind::Undo => knot_storage::UpdateSource::Restore,
            RevertKind::Fallback => knot_storage::UpdateSource::RestoreFallback,
        }
    }

    /// The restore route's `outcome`.
    pub fn outcome(self) -> &'static str {
        match self {
            RevertKind::Undo => "restored",
            RevertKind::Fallback => "fallback",
        }
    }
}

/// What a plan found.
#[derive(Debug)]
pub enum RevertOutcome {
    /// The live page already renders as the version: nothing to write.
    Unchanged,
    /// A verified update that brings the live page back to the version.
    Revert {
        update: Vec<u8>,
        kind: RevertKind,
        /// The roots other than `default` and `knot_applied_suggestions`
        /// that differ between the version and the live page, sorted; left
        /// alone, logged by the caller.
        other_roots_changed: Vec<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RevertError {
    #[error("the live state does not apply: {0}")]
    Live(String),
    #[error("the version does not apply: {0}")]
    Target(String),
    #[error("no revert verified: {0}")]
    Unverified(String),
    #[error("cancelled")]
    Cancelled,
}

/// The points at which a plan asks whether to go on (the cancellation
/// Ruling): before each step that can take long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// Before the undo.
    Undo,
    /// Before the undo's verification.
    UndoVerify,
    /// Before the whole-page replace.
    Fallback,
    /// Before the whole-page replace's verification.
    FallbackVerify,
}

/// Faults a test injects into a plan.
#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RevertHooks {
    /// Go straight to the whole-page replace.
    pub skip_undo: bool,
    /// The undo's capture callback panics while the undo's transaction
    /// commits, as a yrs panic inside the undo would.
    pub panic_in_undo: bool,
    /// The undo runs and its captures are discarded.
    pub empty_undo: bool,
    /// The undo's one update loses its delete set ([`corrupt`]).
    pub corrupt_undo: bool,
    /// The whole-page replace's update is replaced by an empty one.
    pub fail_fallback: bool,
}

/// What a plan did, by cause, for the properties.
#[doc(hidden)]
#[derive(Debug, Default, Clone)]
pub struct PlanTrace {
    /// How many updates the undo captured; `None` when the undo did not run
    /// or panicked.
    pub undo_captures: Option<usize>,
    pub undo_panicked: bool,
    /// Why the undo gave no verified update, when the live state did not
    /// apply to the version or the undo's update did not verify.
    pub undo_unverified: Option<String>,
    /// Why the whole-page replace gave no verified update.
    pub fallback_unverified: Option<String>,
}

/// The verified update that brings the page `live` (a pending-free GC
/// state, the room's export) back to `target`.
pub fn plan(
    target: &RevertTarget,
    live: &[u8],
    hooks: RevertHooks,
) -> Result<RevertOutcome, RevertError> {
    plan_with(target, live, hooks, &|_| true)
}

/// [`plan`], asking `go_on` at each [`Checkpoint`]: the first `false`
/// returns [`RevertError::Cancelled`] at once.
pub fn plan_with(
    target: &RevertTarget,
    live: &[u8],
    hooks: RevertHooks,
    go_on: &dyn Fn(Checkpoint) -> bool,
) -> Result<RevertOutcome, RevertError> {
    plan_core(target, live, hooks, go_on, &mut PlanTrace::default())
}

/// [`plan`] with what it did ([`PlanTrace`]).
#[doc(hidden)]
pub fn plan_traced(
    target: &RevertTarget,
    live: &[u8],
    hooks: RevertHooks,
) -> (Result<RevertOutcome, RevertError>, PlanTrace) {
    let mut trace = PlanTrace::default();
    let result = plan_core(target, live, hooks, &|_| true, &mut trace);
    (result, trace)
}

/// Whether `update` brings `live` back to the version rendered as
/// `target_json`: a GC replay of `live` applies it (outcome `Applied`),
/// holds no pending content after it, and its [`rendered`] page is exactly
/// `target_json`.
pub fn verify(live: &[u8], update: &[u8], target_json: &[u8]) -> Result<(), String> {
    let mut replay = Replayer::from_state(ReplayMode::Gc, live).map_err(|e| e.to_string())?;
    let outcome = replay.apply(update).outcome;
    if outcome != RowOutcome::Applied {
        return Err(format!("the update does not apply: {outcome:?}"));
    }
    if replay.has_pending() {
        return Err("the update leaves pending content".to_string());
    }
    if rendered(replay.doc())? != target_json {
        return Err("the page differs from the version".to_string());
    }
    Ok(())
}

fn plan_core(
    target: &RevertTarget,
    live: &[u8],
    hooks: RevertHooks,
    go_on: &dyn Fn(Checkpoint) -> bool,
    trace: &mut PlanTrace,
) -> Result<RevertOutcome, RevertError> {
    let live_doc =
        Replayer::from_state(ReplayMode::Gc, live).map_err(|e| RevertError::Live(e.to_string()))?;
    if rendered(live_doc.doc()).map_err(RevertError::Live)? == target.json {
        return Ok(RevertOutcome::Unchanged);
    }
    if target.nests_too_deep {
        return Err(RevertError::Unverified(format!(
            "nested past the limit: the version nests deeper than {MAX_NESTING}"
        )));
    }
    if nests_deeper_than(live_doc.doc(), MAX_NESTING) {
        return Err(RevertError::Unverified(format!(
            "nested past the limit: the live page nests deeper than {MAX_NESTING}"
        )));
    }

    let undo_failed = if hooks.skip_undo {
        "skipped".to_string()
    } else {
        if !go_on(Checkpoint::Undo) {
            return Err(RevertError::Cancelled);
        }
        // The undo's docs and captures live in `undo_step`; only its update
        // and `other` come back, and the update is dropped here when it
        // does not verify, so the fallback's peak never adds to the undo's.
        match undo_step(target, live, hooks, trace)? {
            Ok(Step {
                update,
                other_roots_changed,
            }) => {
                if !go_on(Checkpoint::UndoVerify) {
                    return Err(RevertError::Cancelled);
                }
                match verify(live, &update, &target.json) {
                    Ok(()) => {
                        return Ok(RevertOutcome::Revert {
                            update,
                            kind: RevertKind::Undo,
                            other_roots_changed,
                        });
                    }
                    Err(why) => {
                        trace.undo_unverified = Some(why.clone());
                        format!("its update did not verify: {why}")
                    }
                }
            }
            Err(why) => why,
        }
    };

    if !go_on(Checkpoint::Fallback) {
        return Err(RevertError::Cancelled);
    }
    let fallback_failed = match fallback(target, live, &live_doc, hooks) {
        Ok(Step {
            update,
            other_roots_changed,
        }) => {
            if !go_on(Checkpoint::FallbackVerify) {
                return Err(RevertError::Cancelled);
            }
            match verify(live, &update, &target.json) {
                Ok(()) => {
                    return Ok(RevertOutcome::Revert {
                        update,
                        kind: RevertKind::Fallback,
                        other_roots_changed,
                    });
                }
                Err(why) => format!("its update did not verify: {why}"),
            }
        }
        Err(why) => why,
    };
    trace.fallback_unverified = Some(fallback_failed.clone());
    Err(RevertError::Unverified(format!(
        "undo: {undo_failed}; whole-page replace: {fallback_failed}"
    )))
}

/// A step's update, not verified yet.
struct Step {
    update: Vec<u8>,
    other_roots_changed: Vec<String>,
}

/// What the undo did on its plan doc.
struct Undone {
    /// The updates it emitted, or why it did not run.
    captures: Result<Vec<Vec<u8>>, String>,
    other_roots_changed: Vec<String>,
}

/// The undo of every change since the version: `Ok(Err(why))` when it gave
/// no update to verify.
fn undo_step(
    target: &RevertTarget,
    live: &[u8],
    hooks: RevertHooks,
    trace: &mut PlanTrace,
) -> Result<Result<Step, String>, RevertError> {
    let version =
        Update::decode_v1(&target.state).map_err(|e| RevertError::Target(e.to_string()))?;
    let client = fresh_client(&[&target.state, live], draw);
    // The plan doc, the undo manager and the captures live in this closure
    // and are dropped when it returns, also by an unwind.
    let run = catch_unwind(AssertUnwindSafe(|| -> Result<Undone, RevertError> {
        let mut p = Replayer::from_state_as(ReplayMode::KeepDeleted, &target.state, client)
            .map_err(|e| RevertError::Target(e.to_string()))?;
        let before = other_roots(p.doc());
        let outcome = p.apply(live).outcome;
        if outcome != RowOutcome::Applied {
            return Ok(Undone {
                captures: Err(format!(
                    "the live state does not apply to the version: {outcome:?}"
                )),
                other_roots_changed: Vec::new(),
            });
        }
        let other_roots_changed = changed_roots(&before, &other_roots(p.doc()));
        let Some((deletions, insertions)) = since(&p, &version) else {
            return Ok(Undone {
                captures: Err("the plan doc does not encode again".to_string()),
                other_roots_changed,
            });
        };
        let mut captures = undo_captures(p.doc(), deletions, insertions, hooks.panic_in_undo);
        if hooks.empty_undo {
            captures.clear();
        }
        Ok(Undone {
            captures: Ok(captures),
            other_roots_changed,
        })
    }));
    let undone = match run {
        Err(panic) => {
            trace.undo_panicked = true;
            return Ok(Err(format!(
                "the undo panicked: {}",
                panic_message(&*panic)
            )));
        }
        Ok(undone) => undone?,
    };
    let mut captures = match undone.captures {
        Ok(captures) => captures,
        Err(why) => {
            trace.undo_unverified = Some(why.clone());
            return Ok(Err(why));
        }
    };
    trace.undo_captures = Some(captures.len());
    if captures.len() != 1 {
        // Never merged (Rulings): yrs 0.27.3's encoding defect sits in
        // `Update::merge`.
        return Ok(Err(format!("the undo captured {} updates", captures.len())));
    }
    let mut update = captures.pop().unwrap_or_default();
    if hooks.corrupt_undo {
        update = corrupt(&update);
    }
    Ok(Ok(Step {
        update,
        other_roots_changed: undone.other_roots_changed,
    }))
}

/// What changed in the plan doc `p` since `version`, as `(deletions,
/// insertions)` (the Rulings' sets of ids): the blocks `p` holds minus those
/// the version holds, and `p`'s delete set minus the version's. `None` when
/// `p`'s own state does not decode again.
fn since(p: &Replayer, version: &Update) -> Option<(IdSet, IdSet)> {
    let txn = p.doc().transact();
    let insertions = replay::held(&txn)?.diff(&version.insertions(true));
    let deletions = txn.snapshot().delete_set.diff(version.delete_set());
    Some((deletions, insertions))
}

/// Undoes one stack item `(deletions, insertions)` on `doc`'s `default`
/// and returns every update the undo emitted (`observe_update_v1`). With
/// `panic`, the capture callback panics instead, inside the undo's commit.
fn undo_captures(doc: &Doc, deletions: IdSet, insertions: IdSet, panic: bool) -> Vec<Vec<u8>> {
    let mut mgr = UndoManager::<()>::with_options(UndoOptions {
        init_undo_stack: vec![StackItem::new(doc.guid(), deletions, insertions)],
        ..Default::default()
    });
    mgr.expand_scope(doc, &doc.get_or_insert_xml_fragment("default"));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sub = {
        let captured = captured.clone();
        doc.observe_update_v1(move |_, e| {
            if panic {
                panic!("revert hook: a panic inside the undo");
            }
            if let Ok(mut c) = captured.lock() {
                c.push(e.update.clone());
            }
        })
    };
    let Ok(sub) = sub else {
        return Vec::new();
    };
    mgr.undo_blocking();
    drop(sub);
    drop(mgr);
    let mut captured = captured.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *captured)
}

/// `u` without its delete set (`RevertHooks::corrupt_undo`), rewritten
/// through the public API only: a v1 update is its blocks followed by its
/// delete set, whose encoded length does not depend on its client order,
/// so the blocks are `u` minus that length, followed by an empty delete set
/// (`0`). An update whose delete set is already empty becomes
/// `Update::EMPTY_V1`.
fn corrupt(u: &[u8]) -> Vec<u8> {
    let Ok(decoded) = Update::decode_v1(u) else {
        return Update::EMPTY_V1.to_vec();
    };
    if decoded.delete_set().is_empty() {
        return Update::EMPTY_V1.to_vec();
    }
    let ds_len = decoded.delete_set().encode_v1().len();
    let mut out = u[..u.len() - ds_len].to_vec();
    out.push(0);
    let corrupted = Update::decode_v1(&out).expect("the corrupted update decodes");
    assert!(corrupted.delete_set().is_empty());
    assert_eq!(corrupted.insertions(true), decoded.insertions(true));
    out
}

/// The whole-page replace (the typed-copy Ruling): on a GC scratch of
/// `live` under a fresh client, one transaction clears `default` and copies
/// each live child of the version's `default` into it, attributes typed.
fn fallback(
    target: &RevertTarget,
    live: &[u8],
    live_doc: &Replayer,
    hooks: RevertHooks,
) -> Result<Step, String> {
    let h = Replayer::from_state(ReplayMode::KeepDeleted, &target.state)
        .map_err(|e| format!("the version does not apply: {e}"))?;
    let other_roots_changed = changed_roots(&other_roots(h.doc()), &other_roots(live_doc.doc()));
    let client = fresh_client(&[&target.state, live], draw);
    let s = Replayer::from_state_as(ReplayMode::Gc, live, client)
        .map_err(|e| format!("the live state does not apply: {e}"))?;
    let dst = s.doc().get_or_insert_xml_fragment("default");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sub = {
        let captured = captured.clone();
        s.doc()
            .observe_update_v1(move |_, e| {
                if let Ok(mut c) = captured.lock() {
                    c.push(e.update.clone());
                }
            })
            .map_err(|e| e.to_string())?
    };
    let copied = {
        let src_txn = h.doc().transact();
        let mut txn = s.doc().transact_mut();
        let len = dst.len(&txn);
        dst.remove_range(&mut txn, 0, len);
        match src_txn.get_xml_fragment("default") {
            Some(src) => src
                .children(&src_txn)
                .enumerate()
                .try_for_each(|(i, child)| copy_node(&src_txn, child, &mut txn, &dst, i as u32)),
            None => Ok(()),
        }
    };
    drop(sub);
    copied?;
    let mut captured = std::mem::take(&mut *captured.lock().unwrap_or_else(|e| e.into_inner()));
    if captured.len() != 1 {
        return Err(format!(
            "the whole-page replace captured {} updates",
            captured.len()
        ));
    }
    let mut update = captured.pop().unwrap_or_default();
    if hooks.fail_fallback {
        update = Update::EMPTY_V1.to_vec();
    }
    Ok(Step {
        update,
        other_roots_changed,
    })
}

/// Copies `node`, read through `src_txn`, into `dst` at `at`, recursively,
/// with typed attributes and text formatting; a value the copy cannot carry
/// (a shared type in an attribute or a text chunk, a nested fragment) is an
/// error. The depth is bounded: `plan` refuses a version nested past
/// [`MAX_NESTING`] before it gets here.
fn copy_node<T: ReadTxn>(
    src_txn: &T,
    node: XmlOut,
    txn: &mut TransactionMut,
    dst: &impl XmlFragment,
    at: u32,
) -> Result<(), String> {
    match node {
        XmlOut::Element(el) => {
            let copy = dst.insert(txn, at, XmlElementPrelim::empty(el.tag().clone()));
            for (key, value) in el.attributes(src_txn) {
                let Out::Any(any) = value else {
                    return Err(format!(
                        "the attribute {key:?} of a {:?} holds a shared type",
                        el.tag()
                    ));
                };
                copy.insert_attribute(txn, key, any);
            }
            for (i, child) in el.children(src_txn).enumerate() {
                copy_node(src_txn, child, txn, &copy, i as u32)?;
            }
        }
        XmlOut::Text(text) => {
            let copy = dst.insert(txn, at, XmlTextPrelim::new(""));
            for chunk in text.diff(src_txn, YChange::identity) {
                let attrs = chunk.attributes.map(|a| *a).unwrap_or_default();
                let end = copy.len(txn);
                match chunk.insert {
                    Out::Any(Any::String(s)) => copy.insert_with_attributes(txn, end, &s, attrs),
                    Out::Any(any) => {
                        copy.insert_embed_with_attributes(txn, end, any, attrs);
                    }
                    _ => return Err("a text holds a shared type".to_string()),
                }
            }
            for (key, value) in text.attributes(src_txn) {
                let Out::Any(any) = value else {
                    return Err(format!("the text attribute {key:?} holds a shared type"));
                };
                copy.insert_attribute(txn, key, any);
            }
        }
        XmlOut::Fragment(_) => return Err("a nested fragment".to_string()),
    }
    Ok(())
}

/// A client id that neither `states`' state vectors nor their blocks
/// (`insertions(true)`, which covers a client whose blocks all sit past a
/// Skip, where the state vector stops) name, drawn by `draw` until one is
/// free.
fn fresh_client(states: &[&[u8]], mut draw: impl FnMut() -> u32) -> ClientID {
    let mut named: HashSet<u64> = HashSet::new();
    for state in states {
        if let Ok(u) = Update::decode_v1(state) {
            named.extend(u.state_vector().iter().map(|(c, _)| c.get()));
            named.extend(u.insertions(true).client_ids().map(|c| c.get()));
        }
    }
    loop {
        let client = u64::from(draw());
        if !named.contains(&client) {
            return ClientID::new(client);
        }
    }
}

/// A random 32-bit client id (yjs ids stay within 2^53).
fn draw() -> u32 {
    uuid::Uuid::new_v4().as_u128() as u32
}

/// `doc`'s page as the editor shows it, as compact JSON bytes: its
/// [`pm_json`] with adjacent text nodes of the same marks joined, as
/// ProseMirror's `Fragment.fromArray` joins them when y-prosemirror builds
/// the editor's document. `pm_json` writes one text node per chunk of yrs's
/// `Text::diff`, and the diff starts a chunk at every live formatting item,
/// also one that changes nothing (a mark set and unset around text deleted
/// since), so two pages the editor shows alike can differ there: the
/// whole-page replace writes no such item. What [`RevertTarget::json`]
/// holds and [`verify`] compares.
pub fn rendered(doc: &Doc) -> Result<Vec<u8>, String> {
    let mut page = pm_json(doc);
    join_text(&mut page);
    serde_json::to_vec(&page).map_err(|e| e.to_string())
}

/// Joins adjacent text nodes with equal marks in `node`'s content, at every
/// depth.
fn join_text(node: &mut Value) {
    let Some(content) = node.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    let is_text = |v: &Value| v.get("type").and_then(Value::as_str) == Some("text");
    let mut joined: Vec<Value> = Vec::with_capacity(content.len());
    for mut child in content.drain(..) {
        join_text(&mut child);
        if let Some(prev) = joined.last_mut()
            && is_text(prev)
            && is_text(&child)
            && prev.get("marks") == child.get("marks")
            && let (Some(Value::String(a)), Some(Value::String(b))) =
                (prev.get_mut("text"), child.get("text"))
        {
            a.push_str(b);
            continue;
        }
        joined.push(child);
    }
    *content = joined;
}

/// Each root of `doc` but [`OWN_ROOTS`] as JSON, by name.
fn other_roots(doc: &Doc) -> BTreeMap<String, Any> {
    let txn = doc.transact();
    txn.root_refs()
        .filter(|(name, _)| !OWN_ROOTS.contains(name))
        .map(|(name, root)| (name.to_string(), root_json(&txn, &root)))
        .collect()
}

/// A root's JSON. A root the doc only decoded has no type yet: it reads as
/// both its list and its map entries.
fn root_json<T: ReadTxn>(txn: &T, root: &Out) -> Any {
    match root {
        Out::UndefinedRef(branch) => Any::Array(Arc::from(vec![
            ArrayRef::from(*branch).to_json(txn),
            MapRef::from(*branch).to_json(txn),
        ])),
        other => other.to_json(txn),
    }
}

/// The names whose root differs between `a` and `b` (a missing root reads
/// as an empty one), sorted.
fn changed_roots(a: &BTreeMap<String, Any>, b: &BTreeMap<String, Any>) -> Vec<String> {
    let empty = Any::Array(Arc::from(vec![
        Any::Array(Arc::from(Vec::<Any>::new())),
        Any::Map(Arc::new(Default::default())),
    ]));
    let names: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    names
        .into_iter()
        .filter(|name| a.get(*name).unwrap_or(&empty) != b.get(*name).unwrap_or(&empty))
        .cloned()
        .collect()
}

/// Whether a type of `doc`, under any of its roots, nests deeper than
/// `limit` (a root type is at depth 0), read with an explicit stack and
/// only `limit + 1` levels deep, as `knot_crdt::nesting::nests_deeper_than`
/// reads it.
fn nests_deeper_than(doc: &Doc, limit: usize) -> bool {
    let txn = doc.transact();
    let mut stack: Vec<(Out, usize)> = txn.root_refs().map(|(_, root)| (root, 0)).collect();
    while let Some((node, depth)) = stack.pop() {
        if depth > limit {
            return true;
        }
        stack.extend(nested(&txn, &node).into_iter().map(|c| (c, depth + 1)));
    }
    false
}

/// The shared types directly inside `node`.
fn nested<T: ReadTxn>(txn: &T, node: &Out) -> Vec<Out> {
    let mut found: Vec<Out> = match node {
        Out::YXmlFragment(f) => f.children(txn).map(Out::from).collect(),
        Out::YXmlElement(e) => e
            .children(txn)
            .map(Out::from)
            .chain(e.attributes(txn).map(|(_, v)| v))
            .collect(),
        Out::YXmlText(t) => t
            .diff(txn, YChange::identity)
            .into_iter()
            .map(|d| d.insert)
            .chain(t.attributes(txn).map(|(_, v)| v))
            .collect(),
        Out::YText(t) => t
            .diff(txn, YChange::identity)
            .into_iter()
            .map(|d| d.insert)
            .collect(),
        Out::YArray(a) => yrs::Array::iter(a, txn).collect(),
        Out::YMap(m) => yrs::Map::iter(m, txn).map(|(_, v)| v).collect(),
        Out::UndefinedRef(b) => {
            let array = ArrayRef::from(*b);
            let map = MapRef::from(*b);
            yrs::Array::iter(&array, txn)
                .chain(yrs::Map::iter(&map, txn).map(|(_, v)| v))
                .collect()
        }
        _ => Vec::new(),
    };
    found.retain(|out| !matches!(out, Out::Any(_) | Out::YDoc(_)));
    found
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic without a message".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{GetString, TextRef, XmlFragmentRef};

    /// The one update of the transaction `f` runs on `doc`.
    fn txn_update(doc: &Doc, f: impl FnOnce(&mut TransactionMut)) -> Vec<u8> {
        let out = Arc::new(Mutex::new(Vec::new()));
        let sub = {
            let out = out.clone();
            doc.observe_update_v1(move |_, e| out.lock().unwrap().push(e.update.clone()))
                .unwrap()
        };
        f(&mut doc.transact_mut());
        drop(sub);
        let mut all = std::mem::take(&mut *out.lock().unwrap());
        assert_eq!(all.len(), 1);
        all.pop().unwrap()
    }

    fn state(doc: &Doc) -> Vec<u8> {
        replay::own_state(&doc.transact())
    }

    /// A page client 1 writes in one transaction.
    fn page(f: impl FnOnce(&mut TransactionMut, &XmlFragmentRef)) -> Doc {
        let doc = Doc::with_client_id(1);
        let frag = doc.get_or_insert_xml_fragment("default");
        f(&mut doc.transact_mut(), &frag);
        doc
    }

    /// `base` as client `client` sees it, after `f`.
    fn edited(
        base: &Doc,
        client: u64,
        f: impl FnOnce(&mut TransactionMut, &XmlFragmentRef),
    ) -> Doc {
        let doc = Doc::with_client_id(client);
        doc.transact_mut()
            .apply_update(Update::decode_v1(&state(base)).unwrap())
            .unwrap();
        let frag = doc.get_or_insert_xml_fragment("default");
        f(&mut doc.transact_mut(), &frag);
        doc
    }

    fn el(txn: &impl ReadTxn, parent: &impl XmlFragment, i: u32) -> yrs::XmlElementRef {
        match parent.get(txn, i) {
            Some(XmlOut::Element(e)) => e,
            other => panic!("no element at {i}: {other:?}"),
        }
    }

    fn text(txn: &impl ReadTxn, parent: &impl XmlFragment, i: u32) -> yrs::XmlTextRef {
        match el(txn, parent, i).get(txn, 0) {
            Some(XmlOut::Text(t)) => t,
            other => panic!("no text in {i}: {other:?}"),
        }
    }

    fn para(txn: &mut TransactionMut, parent: &impl XmlFragment, s: &str) {
        let n = parent.len(txn);
        let p = parent.insert(txn, n, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new(s));
    }

    /// What the undo of `live` back to `version` captures.
    fn captures(version: &Doc, live: &Doc) -> Vec<Vec<u8>> {
        let (v, l) = (state(version), state(live));
        let t = Update::decode_v1(&v).unwrap();
        let mut p =
            Replayer::from_state_as(ReplayMode::KeepDeleted, &v, ClientID::new(99)).unwrap();
        assert_eq!(p.apply(&l).outcome, RowOutcome::Applied);
        let (deletions, insertions) = since(&p, &t).unwrap();
        undo_captures(p.doc(), deletions, insertions, false)
    }

    /// The page pairs: version, live.
    fn pairs() -> Vec<(&'static str, Doc, Doc)> {
        let alpha_beta = page(|txn, frag| {
            para(txn, frag, "alpha");
            para(txn, frag, "beta");
        });
        let alpha_beta_live = edited(&alpha_beta, 2, |txn, frag| {
            text(txn, frag, 0).remove_range(txn, 0, 5);
            text(txn, frag, 1).insert(txn, 4, " gamma");
            para(txn, frag, "delta");
        });
        let deleted = edited(&alpha_beta, 2, |txn, frag| frag.remove_range(txn, 0, 1));
        let heading = page(|txn, frag| {
            let h = frag.push_back(txn, XmlElementPrelim::empty("heading"));
            h.insert_attribute(txn, "level", Any::Number(1.0));
            h.push_back(txn, XmlTextPrelim::new("Title"));
        });
        let heading_live = edited(&heading, 2, |txn, frag| {
            el(txn, frag, 0).insert_attribute(txn, "level", Any::Number(2.0));
        });
        let list = page(|txn, frag| {
            let l = frag.push_back(txn, XmlElementPrelim::empty("bullet_list"));
            for s in ["one", "two"] {
                let li = l.push_back(txn, XmlElementPrelim::empty("list_item"));
                para(txn, &li, s);
            }
        });
        let nested = edited(&list, 2, |txn, frag| {
            let l = el(txn, frag, 0);
            l.remove_range(txn, 1, 1);
            let first = el(txn, &l, 0);
            let sub = first.push_back(txn, XmlElementPrelim::empty("bullet_list"));
            let li = sub.push_back(txn, XmlElementPrelim::empty("list_item"));
            para(txn, &li, "two");
        });
        let table = page(|txn, frag| {
            let t = frag.push_back(txn, XmlElementPrelim::empty("table"));
            let r = t.push_back(txn, XmlElementPrelim::empty("table_row"));
            let c = r.push_back(txn, XmlElementPrelim::empty("table_cell"));
            para(txn, &c, "a1");
        });
        let row_added = edited(&table, 2, |txn, frag| {
            let t = el(txn, frag, 0);
            let r = t.push_back(txn, XmlElementPrelim::empty("table_row"));
            let c = r.push_back(txn, XmlElementPrelim::empty("table_cell"));
            para(txn, &c, "a2");
        });
        vec![
            ("alpha beta", alpha_beta.clone(), alpha_beta_live),
            ("a deleted paragraph", alpha_beta, deleted),
            ("a heading level by another client", heading, heading_live),
            ("a list item nested", list, nested),
            ("a table row added", table, row_added),
        ]
    }

    #[test]
    fn the_undo_captures_exactly_one_update() {
        for (name, version, live) in pairs() {
            assert_ne!(state(&version), state(&live), "{name}");
            assert_eq!(captures(&version, &live).len(), 1, "{name}");
        }
    }

    #[test]
    fn corrupt_drops_only_the_delete_set() {
        let (_, version, live) = pairs().into_iter().next().unwrap();
        let u = captures(&version, &live).pop().unwrap();
        let decoded = Update::decode_v1(&u).unwrap();
        assert!(!decoded.delete_set().is_empty());
        let c = Update::decode_v1(&corrupt(&u)).unwrap();
        assert!(c.delete_set().is_empty());
        assert_eq!(c.insertions(true), decoded.insertions(true));

        let doc = Doc::with_client_id(3);
        let frag = doc.get_or_insert_xml_fragment("default");
        let insert_only = txn_update(&doc, |txn| para(txn, &frag, "new"));
        assert!(
            Update::decode_v1(&insert_only)
                .unwrap()
                .delete_set()
                .is_empty()
        );
        assert_eq!(corrupt(&insert_only), Update::EMPTY_V1);
    }

    #[test]
    fn the_client_draw_avoids_every_named_client() {
        // Client 7's second block, alone: it names a root and no origin, so
        // it integrates past a Skip over 7#0.
        let a = Doc::with_client_id(7);
        let (t, u): (TextRef, TextRef) = (a.get_or_insert_text("t"), a.get_or_insert_text("u"));
        txn_update(&a, |txn| t.insert(txn, 0, "x"));
        let second = txn_update(&a, |txn| u.insert(txn, 0, "y"));
        let past_skip = Doc::new();
        past_skip
            .transact_mut()
            .apply_update(Update::decode_v1(&second).unwrap())
            .unwrap();
        assert_eq!(
            past_skip
                .get_or_insert_text("u")
                .get_string(&past_skip.transact()),
            "y"
        );
        let s7 = state(&past_skip);
        assert_eq!(
            past_skip.transact().state_vector().get(&ClientID::new(7)),
            0
        );
        assert!(
            Update::decode_v1(&s7)
                .unwrap()
                .insertions(true)
                .client_ids()
                .any(|c| c == ClientID::new(7))
        );

        let nine = Doc::with_client_id(9);
        let frag = nine.get_or_insert_xml_fragment("default");
        txn_update(&nine, |txn| para(txn, &frag, "nine"));
        let s9 = state(&nine);
        assert!(
            Update::decode_v1(&s9)
                .unwrap()
                .state_vector()
                .get(&ClientID::new(9))
                > 0
        );

        let mut script = [7u32, 9, 11].into_iter();
        let drawn = fresh_client(&[&s7, &s9], || script.next().unwrap());
        assert_eq!(drawn, ClientID::new(11));
    }
}
