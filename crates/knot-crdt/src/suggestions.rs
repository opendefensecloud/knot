//! Applying an accepted change suggestion to a page's Y.Doc.
//!
//! A suggestion is anchored by two Yjs RelativePositions that the web client
//! encodes against the y-tiptap binding (spec §"Anchors"): the start is tied
//! to the first selected character (assoc ≥ 0), the end to the last selected
//! character (assoc < 0), and an insertion point to the character before it.
//! Text typed right at either edge therefore stays outside the range.
//!
//! Positions differ between client and server. Yjs clocks and the browser
//! count UTF-16 units, while room docs use `OffsetKind::Bytes`, and
//! `StickyIndex::get_offset` adds a UTF-16 offset inside the anchored item to
//! byte lengths of the items left of it. So [`resolve_range`] resolves the
//! anchors in a UTF-16 replica of the doc and converts the indices to byte
//! offsets of the live doc by walking the text. Whenever the text holds
//! anything other than plain strings (an embed, a hidden item), string offsets
//! would not match item positions, and the range is refused as
//! [`ApplySuggestionError::AnchorLost`] rather than guessed.
//!
//! An insertion whose anchored character is deleted is lost
//! ([`ApplySuggestionError::AnchorLost`]): a deleted item still resolves to
//! an index, the place it was deleted from, which is not where its text went
//! when a block split moved it (y-tiptap keeps the first XmlText and
//! recreates the tail in a new one).
//!
//! [`apply_suggestion`] runs every check before its first mutation and records
//! the suggestion id in the [`APPLIED_MAP`] root map in the same transaction,
//! which makes the apply idempotent and tells a recovery sweep whether a
//! suggestion stuck in `applying` was in fact applied. Any Editor can write
//! that root map over the socket, so the marker's value is the accept claim's
//! `apply_token`, which only the server knows until it writes it: a marker
//! holding anything else is not the room's and is ignored (and overwritten
//! by the apply).

use uuid::Uuid;
use yrs::types::text::YChange;
use yrs::types::{Attrs, TypeRef};
use yrs::updates::decoder::Decode;
use yrs::{
    Any, BranchID, Doc, Map, MapRef, OffsetKind, Options, Out, ReadTxn, StateVector, StickyIndex,
    Text, Transact, TransactionMut, Update, Xml, XmlOut, XmlTextRef,
};

/// What a suggestion does to the text between its anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    /// Replace the range with `proposed_text`.
    Replace,
    /// Insert `proposed_text` at a point (both anchors are the same point).
    Insert,
    /// Remove the range.
    Delete,
}

/// Everything the room needs to apply one accepted suggestion.
#[derive(Debug, Clone)]
pub struct ApplySuggestionRequest {
    /// The suggestion's id, recorded in [`APPLIED_MAP`] once applied.
    pub suggestion_id: Uuid,
    /// The accept claim's token (`suggestions.apply_token`), recorded as the
    /// marker's value; only a marker holding it counts as applied.
    pub apply_token: Uuid,
    /// What to do with the range.
    pub kind: SuggestionKind,
    /// Raw `Y.RelativePosition` v1 bytes of the start anchor (not base64).
    pub start: Vec<u8>,
    /// Raw `Y.RelativePosition` v1 bytes of the end anchor (not base64).
    pub end: Vec<u8>,
    /// The text the accepting owner saw between the anchors; `""` for
    /// [`SuggestionKind::Insert`].
    pub seen_text: String,
    /// The text to put in place of the range; `""` for
    /// [`SuggestionKind::Delete`], which ignores it.
    pub proposed_text: String,
}

/// Why a suggestion was not applied. Every variant leaves the doc untouched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplySuggestionError {
    /// The anchors no longer resolve to a range inside one live text block,
    /// the range collapsed for a replace or delete, it is not a point for an
    /// insertion, or an insertion's anchored character is deleted.
    #[error("anchor lost")]
    AnchorLost,
    /// The text between the anchors is not what the owner saw.
    #[error("text changed")]
    Changed,
    /// The suggestion id is already recorded in [`APPLIED_MAP`] under the
    /// request's `apply_token`.
    #[error("already applied")]
    AlreadyApplied,
    /// An anchor's bytes don't decode as a RelativePosition.
    #[error("decode: {0}")]
    Decode(String),
    /// The applied change could not be persisted (raised by the room).
    #[error("persist: {0}")]
    Persist(String),
}

/// Root Y.Map recording applied suggestion ids. The value is the accept
/// claim's `apply_token` as a string; see [`is_applied`].
pub const APPLIED_MAP: &str = "knot_applied_suggestions";

/// The root XmlFragment holding a page's content. Suggestions apply only
/// to text inside it.
const FRAGMENT: &str = "default";

/// The mark a mention, a date or a page link is made of. An insertion never
/// inherits it, so it doesn't become part of the chip next to it.
const LINK_MARK: &str = "link";

/// The live XmlText a range lives in, plus its bounds in BYTE offsets of the
/// live (`OffsetKind::Bytes`) doc, and the plain text between them.
#[derive(Debug)]
pub struct ResolvedRange {
    /// The text block of the live doc that holds the range.
    pub text: XmlTextRef,
    /// Byte offset of the range's start in `text`.
    pub start_byte: u32,
    /// Byte offset of the range's end in `text`; equal to `start_byte` for a
    /// point.
    pub end_byte: u32,
    /// The plain text between the bounds.
    pub current_text: String,
}

/// Resolves the anchors `start` and `end` (raw RelativePosition v1 bytes) to
/// a range of one live text block of `txn`'s doc.
///
/// [`ApplySuggestionError::AnchorLost`] when an anchor doesn't resolve, the
/// two land in different blocks, in something other than a live XmlText of
/// the page's `default` fragment, or in reverse order, when an index splits a
/// surrogate pair, or when the block holds anything but plain text (so string
/// offsets might not match item positions).
pub fn resolve_range<T: ReadTxn>(
    txn: &T,
    start: &[u8],
    end: &[u8],
) -> Result<ResolvedRange, ApplySuggestionError> {
    let decode = |bytes: &[u8]| {
        StickyIndex::decode_v1(bytes).map_err(|e| ApplySuggestionError::Decode(e.to_string()))
    };
    let start = decode(start)?;
    let end = decode(end)?;

    // Resolve in a UTF-16 replica: there, the in-item offset and the lengths
    // of the items to its left count the same unit. Defining the fragment
    // first gives the root its type, which a root created only by an update
    // lacks, so the walk up to it in `in_live_page` succeeds.
    let replica = Doc::with_options(Options {
        offset_kind: OffsetKind::Utf16,
        ..Default::default()
    });
    replica.get_or_insert_xml_fragment(FRAGMENT);
    let state = txn.encode_state_as_update_v1(&StateVector::default());
    // The live doc's own state should always load; if it doesn't, refuse
    // rather than resolve against a partial replica.
    let update = Update::decode_v1(&state).map_err(|_| ApplySuggestionError::AnchorLost)?;
    replica
        .transact_mut()
        .apply_update(update)
        .map_err(|_| ApplySuggestionError::AnchorLost)?;
    let rtxn = replica.transact();
    let (Some(start), Some(end)) = (start.get_offset(&rtxn), end.get_offset(&rtxn)) else {
        return Err(ApplySuggestionError::AnchorLost);
    };
    if *start.branch.type_ref() != TypeRef::XmlText
        || start.branch.is_deleted()
        || start.branch.id() != end.branch.id()
        || start.index > end.index
    {
        return Err(ApplySuggestionError::AnchorLost);
    }
    let replica_text = XmlTextRef::from(start.branch);
    if !in_live_page(&replica_text) {
        return Err(ApplySuggestionError::AnchorLost);
    }
    let replica_len = replica_text.len(&rtxn);

    let Some(branch) = start.branch.id().get_branch(txn) else {
        return Err(ApplySuggestionError::AnchorLost);
    };
    if *branch.type_ref() != TypeRef::XmlText {
        return Err(ApplySuggestionError::AnchorLost);
    }
    let text = XmlTextRef::from(branch);
    let plain = plain_text(&text, txn).ok_or(ApplySuggestionError::AnchorLost)?;
    if plain.encode_utf16().count() != replica_len as usize {
        return Err(ApplySuggestionError::AnchorLost);
    }
    let (Some(start_byte), Some(end_byte)) = (
        utf16_to_byte(&plain, start.index),
        utf16_to_byte(&plain, end.index),
    ) else {
        return Err(ApplySuggestionError::AnchorLost);
    };
    let current_text = plain[start_byte as usize..end_byte as usize].to_string();
    Ok(ResolvedRange {
        text,
        start_byte,
        end_byte,
        current_text,
    })
}

/// Whether `suggestion_id` is recorded as applied in `applied`, the doc's
/// [`APPLIED_MAP`], under `apply_token`. Clients can write that map, so a
/// marker with any other value (a forged `true`, another token) is not one
/// the room wrote for this claim and reads as not applied.
pub fn is_applied<T: ReadTxn>(
    applied: &MapRef,
    txn: &T,
    suggestion_id: Uuid,
    apply_token: Uuid,
) -> bool {
    match applied.get(txn, &suggestion_id.to_string()) {
        Some(Out::Any(Any::String(value))) => *value == *apply_token.to_string(),
        _ => false,
    }
}

/// Whether `anchor` (raw RelativePosition v1 bytes) is tied to a character
/// that is deleted in `txn`'s doc. An anchor scoped to a whole type (the
/// start of a block) has no character and is never deleted here.
fn anchored_char_deleted<T: ReadTxn>(txn: &T, anchor: &[u8]) -> Result<bool, ApplySuggestionError> {
    let anchor =
        StickyIndex::decode_v1(anchor).map_err(|e| ApplySuggestionError::Decode(e.to_string()))?;
    Ok(anchor
        .id()
        .is_some_and(|id| txn.snapshot().delete_set.contains(id)))
}

/// Applies `req` in `txn` and records its id in `applied`, the doc's
/// [`APPLIED_MAP`], with `req.apply_token` as the value. A marker already
/// there with that token makes it [`ApplySuggestionError::AlreadyApplied`];
/// one with any other value is ignored and overwritten.
///
/// A replace or delete needs a non-empty range and an insertion a point;
/// otherwise the result is [`ApplySuggestionError::AnchorLost`]. An insertion
/// whose anchored character is deleted is lost too (`AnchorLost`): it would
/// resolve to where the character was deleted from, which after a block split
/// is the end of the head, not next to the text that moved. (A replace or
/// delete whose text moved has both anchors on deleted characters and
/// collapses, so it is refused as above.) A delete
/// ignores `proposed_text`. A replace keeps the marks of the first replaced
/// character. An insertion
/// takes the marks of the character before the point (at the start of a
/// block, the one after it), without the `link` mark.
///
/// All checks run before the first mutation, so an Err leaves `txn` empty.
pub fn apply_suggestion(
    applied: &MapRef,
    txn: &mut TransactionMut,
    req: &ApplySuggestionRequest,
) -> Result<(), ApplySuggestionError> {
    if is_applied(applied, txn, req.suggestion_id, req.apply_token) {
        return Err(ApplySuggestionError::AlreadyApplied);
    }
    if req.kind == SuggestionKind::Insert && anchored_char_deleted(txn, &req.start)? {
        return Err(ApplySuggestionError::AnchorLost);
    }
    let range = resolve_range(txn, &req.start, &req.end)?;
    // A replace or delete needs a range, an insertion a point. Anything else
    // would change text the suggestion didn't describe.
    let collapsed = range.start_byte == range.end_byte;
    if collapsed != (req.kind == SuggestionKind::Insert) {
        return Err(ApplySuggestionError::AnchorLost);
    }
    if range.current_text != req.seen_text {
        return Err(ApplySuggestionError::Changed);
    }
    let attrs = match req.kind {
        SuggestionKind::Replace => attrs_at(&range.text, txn, range.start_byte, false),
        SuggestionKind::Insert => {
            let mut attrs = attrs_at(&range.text, txn, range.start_byte, true);
            attrs.remove(LINK_MARK);
            attrs
        }
        SuggestionKind::Delete => Attrs::new(),
    };

    if range.end_byte > range.start_byte {
        range
            .text
            .remove_range(txn, range.start_byte, range.end_byte - range.start_byte);
    }
    if req.kind != SuggestionKind::Delete && !req.proposed_text.is_empty() {
        range
            .text
            .insert_with_attributes(txn, range.start_byte, &req.proposed_text, attrs);
    }
    applied.insert(
        txn,
        req.suggestion_id.to_string(),
        req.apply_token.to_string(),
    );
    Ok(())
}

/// Whether `text` and every block around it is live, up to the page's
/// `default` fragment. Needs that root to have been defined in `text`'s doc.
fn in_live_page(text: &XmlTextRef) -> bool {
    let mut node = XmlOut::Text(text.clone());
    loop {
        let ptr = node.as_ptr();
        if ptr.is_deleted() {
            return false;
        }
        let parent = match &node {
            XmlOut::Text(t) => t.parent(),
            XmlOut::Element(e) => e.parent(),
            XmlOut::Fragment(_) => return ptr.id() == BranchID::Root(FRAGMENT.into()),
        };
        match parent {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

/// The plain text of `text`, or `None` when it holds anything but strings.
fn plain_text<T: ReadTxn>(text: &XmlTextRef, txn: &T) -> Option<String> {
    let mut plain = String::new();
    for chunk in text.diff(txn, YChange::identity) {
        let Out::Any(Any::String(s)) = chunk.insert else {
            return None;
        };
        plain.push_str(&s);
    }
    Some(plain)
}

/// The byte offset of UTF-16 index `index` in `s`; `None` when the index
/// lies past the end or between the two halves of a surrogate pair.
fn utf16_to_byte(s: &str, index: u32) -> Option<u32> {
    let (mut utf16, mut byte) = (0u32, 0u32);
    for c in s.chars() {
        if utf16 >= index {
            break;
        }
        utf16 += c.len_utf16() as u32;
        byte += c.len_utf8() as u32;
    }
    (utf16 == index).then_some(byte)
}

/// The marks of the character at byte `byte` of `text`, or, with `before`,
/// of the character ending at `byte` (falling back to the one at `byte` when
/// `byte` is 0). Empty when there is no such character or it is unmarked.
fn attrs_at<T: ReadTxn>(text: &XmlTextRef, txn: &T, byte: u32, before: bool) -> Attrs {
    let byte = byte as usize;
    let mut chunk_start = 0usize;
    for chunk in text.diff(txn, YChange::identity) {
        let Out::Any(Any::String(s)) = &chunk.insert else {
            continue;
        };
        let chunk_end = chunk_start + s.len();
        let holds = if before && byte > 0 {
            chunk_start < byte && byte <= chunk_end
        } else {
            chunk_start <= byte && byte < chunk_end
        };
        if holds {
            return chunk.attributes.map(|a| *a).unwrap_or_default();
        }
        chunk_start = chunk_end;
    }
    Attrs::new()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use yrs::block::ClientID;
    use yrs::updates::encoder::Encode;
    use yrs::{Assoc, ID, IndexScope, XmlElementPrelim, XmlElementRef, XmlFragment, XmlTextPrelim};

    use super::*;

    const CLIENT: u64 = 7;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn id(clock: u32) -> ID {
        ID::new(ClientID::new(CLIENT), clock)
    }

    /// An anchor tied to the character at `clock`, at the space before it
    /// (assoc ≥ 0): a range's start.
    fn at(clock: u32) -> Vec<u8> {
        StickyIndex::from_id(id(clock), Assoc::After).encode_v1()
    }

    /// An anchor tied to the character at `clock`, at the space after it
    /// (assoc < 0): a range's end, or an insertion point.
    fn after(clock: u32) -> Vec<u8> {
        StickyIndex::from_id(id(clock), Assoc::Before).encode_v1()
    }

    /// Appends an element `tag` to `parent`.
    fn push<P: XmlFragment>(parent: &P, txn: &mut TransactionMut, tag: &str) -> XmlElementRef {
        parent.push_back(txn, XmlElementPrelim::empty(tag))
    }

    /// A doc of client [`CLIENT`] holding `paragraph > text`: the paragraph
    /// is clock 0, the XmlText clock 1, and its characters start at clock 2.
    fn paragraph_doc(text: &str) -> (Doc, XmlTextRef) {
        let doc = Doc::with_client_id(CLIENT);
        let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
        let xml = {
            let mut txn = doc.transact_mut();
            let p = push(&frag, &mut txn, "paragraph");
            p.push_back(&mut txn, XmlTextPrelim::new(text))
        };
        (doc, xml)
    }

    fn request(
        kind: SuggestionKind,
        start: Vec<u8>,
        end: Vec<u8>,
        seen: &str,
        proposed: &str,
    ) -> ApplySuggestionRequest {
        ApplySuggestionRequest {
            suggestion_id: Uuid::new_v4(),
            apply_token: Uuid::new_v4(),
            kind,
            start,
            end,
            seen_text: seen.into(),
            proposed_text: proposed.into(),
        }
    }

    fn apply(doc: &Doc, req: &ApplySuggestionRequest) -> Result<(), ApplySuggestionError> {
        let applied = doc.get_or_insert_map(APPLIED_MAP);
        let mut txn = doc.transact_mut();
        apply_suggestion(&applied, &mut txn, req)
    }

    fn resolve(doc: &Doc, start: &[u8], end: &[u8]) -> Result<ResolvedRange, ApplySuggestionError> {
        resolve_range(&doc.transact(), start, end)
    }

    fn plain(doc: &Doc, text: &XmlTextRef) -> String {
        plain_text(text, &doc.transact()).unwrap()
    }

    /// `text` as runs of equally marked text, adjacent equal runs merged.
    fn runs(doc: &Doc, text: &XmlTextRef) -> Vec<(String, Attrs)> {
        let txn = doc.transact();
        let mut runs: Vec<(String, Attrs)> = Vec::new();
        for chunk in text.diff(&txn, YChange::identity) {
            let Out::Any(Any::String(s)) = chunk.insert else {
                panic!("non-string chunk");
            };
            let attrs = chunk.attributes.map(|a| *a).unwrap_or_default();
            match runs.last_mut() {
                Some((last, last_attrs)) if *last_attrs == attrs => last.push_str(&s),
                _ => runs.push((s.to_string(), attrs)),
            }
        }
        runs
    }

    fn bold() -> Attrs {
        Attrs::from([("bold".into(), Any::Map(Arc::new(HashMap::new())))])
    }

    #[test]
    fn decodes_yjs_made_fixtures() {
        // Made by yjs 13.6 (clientID 7): paragraph > XmlText "äb€cd", bold on
        // "b€". Anchors made by the client's suggestion encoder.
        let doc = Doc::new();
        {
            let update = Update::decode_v1(&hex(
                "0107070007010764656661756c74030970617261677261706807000700060400070102c3a48407020462e282ac840704026364c60702070304626f6c64027b7dc60704070504626f6c64046e756c6c00",
            ))
            .unwrap();
            doc.transact_mut().apply_update(update).unwrap();
        }

        let range = resolve(&doc, &hex("00070400"), &hex("00070541")).unwrap();
        assert_eq!(range.current_text, "€c");
        assert_eq!((range.start_byte, range.end_byte), (3, 7));

        let point = resolve(&doc, &hex("02070141"), &hex("02070141")).unwrap();
        assert_eq!(point.current_text, "");
        assert_eq!((point.start_byte, point.end_byte), (0, 0));

        let to_end = resolve(&doc, &hex("00070400"), &hex("02070100")).unwrap();
        assert_eq!(to_end.current_text, "€cd");
        // ä is 2 bytes, b 1, € 3, c and d 1 each.
        assert_eq!((to_end.start_byte, to_end.end_byte), (3, 8));

        let req = request(
            SuggestionKind::Replace,
            hex("00070400"),
            hex("00070541"),
            "€c",
            "X",
        );
        apply(&doc, &req).unwrap();
        assert_eq!(
            runs(&doc, &range.text),
            vec![
                ("ä".to_string(), Attrs::new()),
                ("bX".to_string(), bold()),
                ("d".to_string(), Attrs::new()),
            ]
        );
    }

    #[test]
    fn non_ascii_before_anchor_in_same_item() {
        // One item "äbc": ä at clock 2, b at 3, c at 4.
        let (doc, text) = paragraph_doc("äbc");
        let range = resolve(&doc, &at(4), &after(4)).unwrap();
        assert_eq!(range.current_text, "c");
        assert_eq!((range.start_byte, range.end_byte), (3, 4));

        apply(
            &doc,
            &request(SuggestionKind::Replace, at(4), after(4), "c", "ü"),
        )
        .unwrap();
        assert_eq!(plain(&doc, &text), "äbü");
    }

    #[test]
    fn surrogate_split_index_is_anchor_lost() {
        // "a😀b": a at clock 2, 😀 at 3..4, b at 5.
        let (doc, _) = paragraph_doc("a😀b");
        assert_eq!(
            resolve(&doc, &at(4), &after(5)).unwrap_err(),
            ApplySuggestionError::AnchorLost
        );
        assert_eq!(
            resolve(&doc, &at(2), &after(3)).unwrap_err(),
            ApplySuggestionError::AnchorLost
        );
    }

    #[test]
    fn emoji_surrogate_pair_range() {
        let (doc, text) = paragraph_doc("a😀b");
        let range = resolve(&doc, &at(3), &after(4)).unwrap();
        assert_eq!(range.current_text, "😀");
        assert_eq!((range.start_byte, range.end_byte), (1, 5));

        apply(
            &doc,
            &request(SuggestionKind::Replace, at(3), after(4), "😀", "X"),
        )
        .unwrap();
        assert_eq!(plain(&doc, &text), "aXb");
    }

    #[test]
    fn insert_next_to_link_drops_link_mark() {
        // "hi @bob end", all bold, "@bob" a mention: chars at clocks 2..12,
        // the formatting after them. "b" of "@bob" is clock 8.
        let (doc, text) = paragraph_doc("hi @bob end");
        let link = Attrs::from([(
            LINK_MARK.into(),
            Any::Map(Arc::new(HashMap::from([(
                "href".to_string(),
                Any::from("knot://user/0b6f0c64-9f0e-4c48-9b7c-7f0b1d6a3a10"),
            )]))),
        )]);
        {
            let mut txn = doc.transact_mut();
            text.format(&mut txn, 0, 11, bold());
            text.format(&mut txn, 3, 4, link.clone());
        }
        apply(
            &doc,
            &request(SuggestionKind::Insert, after(8), after(8), "", " and alice"),
        )
        .unwrap();

        let mut mention = bold();
        mention.extend(link);
        assert_eq!(
            runs(&doc, &text),
            vec![
                ("hi ".to_string(), bold()),
                ("@bob".to_string(), mention),
                (" and alice end".to_string(), bold()),
            ]
        );
    }

    #[test]
    fn block_split_is_anchor_lost() {
        // "hello world": h at clock 2, "world" at 8..12.
        let (doc, text) = paragraph_doc("hello world");
        {
            // Split after "hello" the way y-prosemirror does: the tail moves
            // into a new paragraph with a new XmlText.
            let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
            let mut txn = doc.transact_mut();
            text.remove_range(&mut txn, 5, 6);
            let p = push(&frag, &mut txn, "paragraph");
            p.push_back(&mut txn, XmlTextPrelim::new(" world"));
        }
        let lost = request(SuggestionKind::Replace, at(8), after(12), "world", "there");
        assert_eq!(apply(&doc, &lost), Err(ApplySuggestionError::AnchorLost));

        let kept = resolve(&doc, &at(2), &after(4)).unwrap();
        assert_eq!(kept.current_text, "hel");
        apply(
            &doc,
            &request(SuggestionKind::Replace, at(2), after(4), "hel", "J"),
        )
        .unwrap();
        assert_eq!(plain(&doc, &text), "Jlo");
    }

    #[test]
    fn embedded_item_in_text_is_anchor_lost() {
        let (doc, text) = paragraph_doc("ab");
        {
            let mut txn = doc.transact_mut();
            text.insert_embed(&mut txn, 1, vec![0u8, 1, 2]);
        }
        assert_eq!(
            resolve(&doc, &at(2), &after(3)).unwrap_err(),
            ApplySuggestionError::AnchorLost
        );
    }

    #[test]
    fn replace_keeps_marks_of_first_char() {
        // "abcdef" at clocks 2..7, bold on "cd"; replace "cde".
        let (doc, text) = paragraph_doc("abcdef");
        {
            let mut txn = doc.transact_mut();
            text.format(&mut txn, 2, 2, bold());
        }
        apply(
            &doc,
            &request(SuggestionKind::Replace, at(4), after(6), "cde", "XYZ"),
        )
        .unwrap();
        assert_eq!(
            runs(&doc, &text),
            vec![
                ("ab".to_string(), Attrs::new()),
                ("XYZ".to_string(), bold()),
                ("f".to_string(), Attrs::new()),
            ]
        );
    }

    #[test]
    fn insert_at_block_start_takes_next_char_marks() {
        // "abc", all bold; the point at the start of the block is encoded
        // against the XmlText itself (clock 1), as Yjs does at index 0.
        let (doc, text) = paragraph_doc("abc");
        {
            let mut txn = doc.transact_mut();
            text.format(&mut txn, 0, 3, bold());
        }
        let point = StickyIndex::new(IndexScope::Nested(id(1)), Assoc::Before).encode_v1();
        apply(
            &doc,
            &request(SuggestionKind::Insert, point.clone(), point, "", "X"),
        )
        .unwrap();
        assert_eq!(runs(&doc, &text), vec![("Xabc".to_string(), bold())]);
    }

    #[test]
    fn insert_with_a_range_is_anchor_lost() {
        // "Grüße": G at clock 2, r at 3. Start before G, end after r.
        let (doc, text) = paragraph_doc("Grüße");
        let req = request(SuggestionKind::Insert, at(2), after(3), "Gr", "X");
        assert_eq!(apply(&doc, &req), Err(ApplySuggestionError::AnchorLost));
        assert_eq!(plain(&doc, &text), "Grüße");
    }

    #[test]
    fn delete_ignores_proposed_text() {
        // "hello world": " world" at clocks 7..12.
        let (doc, text) = paragraph_doc("hello world");
        apply(
            &doc,
            &request(SuggestionKind::Delete, at(7), after(12), " world", "X"),
        )
        .unwrap();
        assert_eq!(plain(&doc, &text), "hello");
    }

    #[test]
    fn delete_removes_range() {
        // "hello world": " world" at clocks 7..12.
        let (doc, text) = paragraph_doc("hello world");
        apply(
            &doc,
            &request(SuggestionKind::Delete, at(7), after(12), " world", ""),
        )
        .unwrap();
        assert_eq!(plain(&doc, &text), "hello");
    }

    #[test]
    fn changed_text_is_refused_and_writes_nothing() {
        let (doc, text) = paragraph_doc("hello world");
        let applied = doc.get_or_insert_map(APPLIED_MAP);
        let updates = Arc::new(Mutex::new(0usize));
        let counter = updates.clone();
        let sub = doc
            .observe_update_v1(move |_, _| *counter.lock().unwrap() += 1)
            .unwrap();
        let req = request(SuggestionKind::Replace, at(8), after(12), "earth", "there");
        let result = {
            let mut txn = doc.transact_mut();
            apply_suggestion(&applied, &mut txn, &req)
        };
        drop(sub);
        assert_eq!(result, Err(ApplySuggestionError::Changed));
        assert_eq!(*updates.lock().unwrap(), 0);
        assert_eq!(plain(&doc, &text), "hello world");
        assert!(!is_applied(
            &applied,
            &doc.transact(),
            req.suggestion_id,
            req.apply_token
        ));
    }

    #[test]
    fn collapsed_range_is_anchor_lost() {
        let (doc, text) = paragraph_doc("hello world");
        {
            let mut txn = doc.transact_mut();
            text.remove_range(&mut txn, 6, 5);
        }
        let req = request(SuggestionKind::Replace, at(8), after(12), "", "there");
        assert_eq!(apply(&doc, &req), Err(ApplySuggestionError::AnchorLost));
    }

    #[test]
    fn deleted_paragraph_is_anchor_lost() {
        let (doc, _) = paragraph_doc("hello world");
        {
            let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
            let mut txn = doc.transact_mut();
            push(&frag, &mut txn, "paragraph");
            frag.remove_range(&mut txn, 0, 1);
        }
        let req = request(SuggestionKind::Replace, at(8), after(12), "world", "there");
        assert_eq!(apply(&doc, &req), Err(ApplySuggestionError::AnchorLost));
    }

    #[test]
    fn cross_text_range_is_anchor_lost() {
        // Paragraph 1 "one" (p 0, text 1, chars 2..4), paragraph 2 "two"
        // (p 5, text 6, chars 7..9).
        let (doc, _) = paragraph_doc("one");
        {
            let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
            let mut txn = doc.transact_mut();
            let p = push(&frag, &mut txn, "paragraph");
            p.push_back(&mut txn, XmlTextPrelim::new("two"));
        }
        assert_eq!(
            resolve(&doc, &at(7), &after(9)).unwrap().current_text,
            "two"
        );
        assert_eq!(
            resolve(&doc, &at(3), &after(8)).unwrap_err(),
            ApplySuggestionError::AnchorLost
        );
    }

    #[test]
    fn edge_inserts_stay_outside_range() {
        // "hello world": "world" at clocks 8..12.
        let (doc, text) = paragraph_doc("hello world");
        {
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 6, "XX");
            text.insert(&mut txn, 13, "YY");
        }
        assert_eq!(plain(&doc, &text), "hello XXworldYY");
        assert_eq!(
            resolve(&doc, &at(8), &after(12)).unwrap().current_text,
            "world"
        );
        apply(
            &doc,
            &request(SuggestionKind::Replace, at(8), after(12), "world", "there"),
        )
        .unwrap();
        assert_eq!(plain(&doc, &text), "hello XXthereYY");
    }

    #[test]
    fn apply_inside_table_cell() {
        // table 0 > table_row 1 > table_cell 2 > paragraph 3 > text 4, chars
        // "cell text" at 5..13; "text" at 10..13.
        let doc = Doc::with_client_id(CLIENT);
        let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
        let text = {
            let mut txn = doc.transact_mut();
            let table = push(&frag, &mut txn, "table");
            let row = push(&table, &mut txn, "table_row");
            let cell = push(&row, &mut txn, "table_cell");
            let p = push(&cell, &mut txn, "paragraph");
            p.push_back(&mut txn, XmlTextPrelim::new("cell text"))
        };
        apply(
            &doc,
            &request(SuggestionKind::Replace, at(10), after(13), "text", "data"),
        )
        .unwrap();
        assert_eq!(plain(&doc, &text), "cell data");
    }

    #[test]
    fn undecodable_anchor_is_decode_error() {
        let (doc, _) = paragraph_doc("hello");
        assert!(matches!(
            resolve(&doc, &[0x09], &after(2)),
            Err(ApplySuggestionError::Decode(_))
        ));
    }

    #[test]
    fn second_apply_is_already_applied() {
        let (doc, text) = paragraph_doc("hello world");
        let req = request(SuggestionKind::Replace, at(8), after(12), "world", "there");
        apply(&doc, &req).unwrap();
        assert!(is_applied(
            &doc.get_or_insert_map(APPLIED_MAP),
            &doc.transact(),
            req.suggestion_id,
            req.apply_token
        ));
        assert_eq!(apply(&doc, &req), Err(ApplySuggestionError::AlreadyApplied));
        assert_eq!(plain(&doc, &text), "hello there");
    }

    /// Any Editor can write the applied map over the socket. A marker that
    /// doesn't carry the claim's token is not the room's, so the apply goes
    /// ahead and overwrites it with the real one.
    #[test]
    fn forged_marker_does_not_short_circuit_apply() {
        for forged in [Any::Bool(true), Any::from(Uuid::new_v4().to_string())] {
            let (doc, text) = paragraph_doc("hello world");
            let req = request(SuggestionKind::Replace, at(8), after(12), "world", "there");
            let applied = doc.get_or_insert_map(APPLIED_MAP);
            {
                let mut txn = doc.transact_mut();
                applied.insert(&mut txn, req.suggestion_id.to_string(), forged.clone());
            }
            assert!(
                !is_applied(
                    &applied,
                    &doc.transact(),
                    req.suggestion_id,
                    req.apply_token
                ),
                "{forged:?} passed for the claim's token"
            );

            apply(&doc, &req).unwrap_or_else(|e| panic!("{forged:?}: {e:?}"));
            assert_eq!(plain(&doc, &text), "hello there");
            assert!(is_applied(
                &applied,
                &doc.transact(),
                req.suggestion_id,
                req.apply_token
            ));
            assert_eq!(apply(&doc, &req), Err(ApplySuggestionError::AlreadyApplied));
        }
    }

    /// Splits `text`'s block at `index` the way y-tiptap does on Enter: the
    /// first XmlText keeps the head, its tail is deleted and recreated in a
    /// new XmlText of a new paragraph right after it.
    fn split_block(doc: &Doc, text: &XmlTextRef, index: u32) -> XmlTextRef {
        let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
        let mut txn = doc.transact_mut();
        let full = plain_text(text, &txn).unwrap();
        let tail = full[index as usize..].to_string();
        text.remove_range(&mut txn, index, tail.len() as u32);
        let p = frag.insert(&mut txn, 1, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(tail))
    }

    #[test]
    fn insert_anchored_to_moved_text_is_anchor_lost() {
        // "Please call me tomorrow.": "Please call me" at clocks 2..15,
        // " tomorrow." at 16..25, the "w" of "tomorrow" at 24.
        let (doc, text) = paragraph_doc("Please call me tomorrow.");
        let req = request(SuggestionKind::Insert, after(24), after(24), "", " early");
        // Enter between "me" and " tomorrow": the anchored "w" is now a
        // tombstone in the still-live first XmlText, where it would resolve
        // to the end of "Please call me".
        let moved = split_block(&doc, &text, 14);
        assert_eq!(plain(&doc, &text), "Please call me");
        assert_eq!(plain(&doc, &moved), " tomorrow.");

        assert_eq!(apply(&doc, &req), Err(ApplySuggestionError::AnchorLost));
        assert_eq!(plain(&doc, &text), "Please call me");
        assert_eq!(plain(&doc, &moved), " tomorrow.");
    }

    #[test]
    fn insert_anchored_to_deleted_char_is_anchor_lost() {
        // "hello world": the "o" of "hello" at clock 6.
        let (doc, text) = paragraph_doc("hello world");
        {
            let mut txn = doc.transact_mut();
            text.remove_range(&mut txn, 4, 1);
        }
        let req = request(SuggestionKind::Insert, after(6), after(6), "", "X");
        assert_eq!(apply(&doc, &req), Err(ApplySuggestionError::AnchorLost));
        assert_eq!(plain(&doc, &text), "hell world");
    }

    #[test]
    fn replace_of_moved_text_is_anchor_lost() {
        // "tomorrow" at clocks 17..24 of "Please call me tomorrow.".
        let (doc, text) = paragraph_doc("Please call me tomorrow.");
        let req = request(
            SuggestionKind::Replace,
            at(17),
            after(24),
            "tomorrow",
            "today",
        );
        let moved = split_block(&doc, &text, 14);

        assert_eq!(apply(&doc, &req), Err(ApplySuggestionError::AnchorLost));
        assert_eq!(plain(&doc, &text), "Please call me");
        assert_eq!(plain(&doc, &moved), " tomorrow.");
    }
}
