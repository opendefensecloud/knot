//! CRDT engine abstraction. The `Engine` trait is what every other crate
//! holds; `YrsEngine` is the v0.1 implementation backed by `yrs`.

use thiserror::Error;
use yrs::{
    Doc, ReadTxn, StateVector, Transact, Update,
    updates::{decoder::Decode, encoder::Encode},
};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("yrs apply: {0}")]
    Apply(String),
    #[error("yrs encode: {0}")]
    Encode(String),
}

pub struct DocHandle(pub(crate) Doc);

impl DocHandle {
    /// Returns a reference to the underlying yrs Doc.
    ///
    /// Intended for test helpers that construct documents directly via
    /// yrs APIs. Production code MUST go through the `Engine` trait.
    pub fn inner(&self) -> &yrs::Doc {
        &self.0
    }
}

pub trait Engine: Send + Sync + 'static {
    fn new_doc(&self) -> DocHandle;
    fn apply_update(&self, d: &DocHandle, update: &[u8]) -> Result<(), EngineError>;
    /// Apply `update` and report whether the document changed: new content
    /// integrated, something deleted, or content parked as pending because a
    /// dependency is missing. `Ok(false)` means the update carried nothing the
    /// doc did not already have — e.g. the SyncStep2 a reconnecting client
    /// sends back, which always includes its full delete set. Callers use it
    /// to skip persisting / fanning out such no-ops; anything that could not
    /// be integrated yet reports `true`, so it is never dropped.
    fn apply_update_changed(&self, d: &DocHandle, update: &[u8]) -> Result<bool, EngineError>;
    fn encode_state_as_update(
        &self,
        d: &DocHandle,
        peer_sv: Option<&[u8]>,
    ) -> Result<Vec<u8>, EngineError>;
    fn encode_state_vector(&self, d: &DocHandle) -> Result<Vec<u8>, EngineError>;
}

#[derive(Default, Clone)]
pub struct YrsEngine;

impl Engine for YrsEngine {
    fn new_doc(&self) -> DocHandle {
        DocHandle(Doc::new())
    }

    fn apply_update(&self, d: &DocHandle, update: &[u8]) -> Result<(), EngineError> {
        let u = Update::decode_v1(update).map_err(|e| EngineError::Apply(e.to_string()))?;
        let mut txn = d.0.transact_mut();
        txn.apply_update(u)
            .map_err(|e| EngineError::Apply(e.to_string()))?;
        Ok(())
    }

    fn apply_update_changed(&self, d: &DocHandle, update: &[u8]) -> Result<bool, EngineError> {
        let u = Update::decode_v1(update).map_err(|e| EngineError::Apply(e.to_string()))?;
        let mut txn = d.0.transact_mut();
        let pending_before = pending_fingerprint(&txn);
        txn.apply_update(u)
            .map_err(|e| EngineError::Apply(e.to_string()))?;
        // - insert_set: structs THIS transaction integrated (yrs records every
        //   integrated item and GC range there), so already-known structs in
        //   a re-sent state do not count;
        // - delete_set: likewise only items newly deleted here, so the full
        //   delete set every SyncStep2 carries does not count by itself;
        // - pending: structs/deletes yrs had to park because a dependency is
        //   missing. Compared before/after, not as a doc-wide flag: a doc can
        //   hold pending content indefinitely (and snapshots and every
        //   client's state re-send it), and re-delivering it changes nothing.
        //   Content that is newly parked does change it, and is never dropped.
        Ok(!txn.insert_set().is_empty()
            || !txn.delete_set().is_empty()
            || pending_fingerprint(&txn) != pending_before)
    }

    fn encode_state_as_update(
        &self,
        d: &DocHandle,
        peer_sv: Option<&[u8]>,
    ) -> Result<Vec<u8>, EngineError> {
        let sv = match peer_sv {
            Some(bytes) => {
                StateVector::decode_v1(bytes).map_err(|e| EngineError::Encode(e.to_string()))?
            }
            None => StateVector::default(),
        };
        let txn = d.0.transact();
        Ok(txn.encode_state_as_update_v1(&sv))
    }

    fn encode_state_vector(&self, d: &DocHandle) -> Result<Vec<u8>, EngineError> {
        let txn = d.0.transact();
        Ok(txn.state_vector().encode_v1())
    }
}

/// The doc's pending (not yet integrable) structs and deletes, encoded, so a
/// caller can tell whether a transaction parked anything new. `None` in the
/// common case of nothing pending, which costs nothing to compute.
fn pending_fingerprint<T: ReadTxn>(txn: &T) -> Option<(Vec<u8>, Vec<u8>)> {
    let store = txn.store();
    let structs = store.pending_update().map(|p| p.update.encode_v1());
    let deletes = store.pending_ds().map(|ds| ds.encode_v1());
    if structs.is_none() && deletes.is_none() {
        return None;
    }
    Some((structs.unwrap_or_default(), deletes.unwrap_or_default()))
}

#[derive(Debug, Clone)]
pub struct TextMark {
    pub kind: String,
    pub attrs: Vec<TextMarkAttr>,
}

#[derive(Debug, Clone)]
pub struct TextMarkAttr {
    pub name: String,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{GetString, Text};

    /// Encode everything `doc` holds as a v1 update.
    fn full_update(doc: &Doc) -> Vec<u8> {
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    #[test]
    fn apply_update_changed_reports_new_content() {
        let src = Doc::new();
        src.get_or_insert_text("t")
            .push(&mut src.transact_mut(), "hello");

        let e = YrsEngine;
        let d = e.new_doc();
        assert!(e.apply_update_changed(&d, &full_update(&src)).unwrap());
    }

    /// The reconnect handshake makes every writer reply with a SyncStep2
    /// that usually re-sends what the server already has (plus the whole
    /// delete set). That must read as "unchanged".
    #[test]
    fn reapplying_known_content_including_deletes_is_unchanged() {
        let src = Doc::new();
        let t = src.get_or_insert_text("t");
        t.push(&mut src.transact_mut(), "hellox");
        t.remove_range(&mut src.transact_mut(), 5, 1);
        let update = full_update(&src);

        let e = YrsEngine;
        let d = e.new_doc();
        assert!(e.apply_update_changed(&d, &update).unwrap());
        assert!(
            !e.apply_update_changed(&d, &update).unwrap(),
            "re-applying an identical update (with a delete set) must be a no-op"
        );
        // And a diff against our own state vector — exactly what a client
        // answers to the server's SyncStep1 — is a no-op too.
        let sv = e.encode_state_vector(&d).unwrap();
        let reply = {
            let sv = StateVector::decode_v1(&sv).unwrap();
            src.transact().encode_state_as_update_v1(&sv)
        };
        assert!(!e.apply_update_changed(&d, &reply).unwrap());
    }

    /// A deletion adds no new structs (the state vector does not move) but
    /// it does change the document.
    #[test]
    fn delete_only_update_is_changed() {
        let src = Doc::new();
        let t = src.get_or_insert_text("t");
        t.push(&mut src.transact_mut(), "hello");

        let e = YrsEngine;
        let d = e.new_doc();
        e.apply_update(&d, &full_update(&src)).unwrap();

        let before = src.transact().state_vector();
        t.remove_range(&mut src.transact_mut(), 0, 1);
        let delete_only = src.transact().encode_state_as_update_v1(&before);
        assert_eq!(
            src.transact().state_vector(),
            before,
            "fixture: a delete must not advance the state vector"
        );

        assert!(e.apply_update_changed(&d, &delete_only).unwrap());
        let txt = d.0.get_or_insert_text("t");
        assert_eq!(txt.get_string(&d.0.transact()), "ello");
    }

    /// An update whose dependency the server lacks cannot be integrated yet;
    /// yrs parks it as pending. It must still count as a change so the room
    /// persists it — dropping it would lose the edit for good once the
    /// missing piece arrives later.
    #[test]
    fn update_with_missing_dependency_is_changed() {
        let src = Doc::new();
        let t = src.get_or_insert_text("t");
        t.push(&mut src.transact_mut(), "a");
        let sv_after_a = src.transact().state_vector();
        t.push(&mut src.transact_mut(), "b");
        let only_b = src.transact().encode_state_as_update_v1(&sv_after_a);

        let e = YrsEngine;
        let d = e.new_doc();
        assert!(
            e.apply_update_changed(&d, &only_b).unwrap(),
            "an update parked as pending must be reported as changed"
        );
        assert!(d.0.transact().has_missing_updates());
        assert_eq!(
            e.encode_state_vector(&d).unwrap(),
            StateVector::default().encode_v1(),
            "fixture: the dependent update must not have integrated"
        );
    }

    /// A doc can hold pending content for good: the offline-edit bug left
    /// docs whose later edits depend on clocks that were never uploaded, and
    /// snapshots keep the pending part (yrs and Yjs both re-encode it into
    /// every state update). Then every client's SyncStep2 reply carries that
    /// pending content again. Re-delivering what is already pending changes
    /// nothing and must not read as a change — or every page open on such a
    /// doc would be persisted and credited as an edit.
    #[test]
    fn redelivering_already_pending_content_is_unchanged() {
        let src = Doc::new();
        let t = src.get_or_insert_text("t");
        t.push(&mut src.transact_mut(), "a");
        let sv_after_a = src.transact().state_vector();
        t.push(&mut src.transact_mut(), "b");
        let only_b = src.transact().encode_state_as_update_v1(&sv_after_a);

        let e = YrsEngine;
        let d = e.new_doc();
        assert!(e.apply_update_changed(&d, &only_b).unwrap());
        assert!(
            d.0.transact().has_missing_updates(),
            "fixture: b must be pending"
        );

        assert!(
            !e.apply_update_changed(&d, &only_b).unwrap(),
            "re-delivering the pending update must be a no-op"
        );
        // What a client that loaded this doc answers to SyncStep1: the doc's
        // state relative to the server's (empty) state vector, which includes
        // the pending content.
        let reply = full_update(&d.0);
        assert!(
            !e.apply_update_changed(&d, &reply).unwrap(),
            "a SyncStep2 reply that only re-sends pending content must be a no-op"
        );
    }

    /// The counterpart: filling the gap integrates the pending content.
    #[test]
    fn filling_the_missing_dependency_is_changed() {
        let src = Doc::new();
        let t = src.get_or_insert_text("t");
        t.push(&mut src.transact_mut(), "a");
        let only_a = full_update(&src);
        let sv_after_a = src.transact().state_vector();
        t.push(&mut src.transact_mut(), "b");
        let only_b = src.transact().encode_state_as_update_v1(&sv_after_a);

        let e = YrsEngine;
        let d = e.new_doc();
        e.apply_update(&d, &only_b).unwrap();
        assert!(e.apply_update_changed(&d, &only_a).unwrap());
        assert!(!d.0.transact().has_missing_updates());
        let txt = d.0.get_or_insert_text("t");
        assert_eq!(txt.get_string(&d.0.transact()), "ab");
    }

    #[test]
    fn malformed_update_is_an_error() {
        let e = YrsEngine;
        let d = e.new_doc();
        assert!(e.apply_update_changed(&d, &[0xff, 0xff, 0xff]).is_err());
    }
}
