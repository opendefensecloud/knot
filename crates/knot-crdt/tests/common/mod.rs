//! Shared by the test binaries that build updates: the differential between
//! knot's update reader (`knot_crdt::update_items`) and yrs's own decoder.
#![allow(dead_code)]

use knot_crdt::update_items::decode;
use yrs::updates::decoder::Decode;
use yrs::{IdSet, Update};

/// Where the reader and yrs (`Update::decode_v1`) disagree on `update`, and
/// on every update that ends early in it (each prefix, cut at every byte):
/// see [`disagreement`]. The prefixes end inside each part of the encoding,
/// the delete set included, and none of them decodes.
pub fn disagreements(what: &str, update: &[u8]) -> Vec<String> {
    (0..=update.len())
        .filter_map(|end| {
            disagreement(&update[..end])
                .map(|how| format!("{what}, its first {end} of {} bytes: {how}", update.len()))
        })
        .collect()
}

/// How the reader and yrs disagree on `update`, if they do: on whether it
/// decodes, or, when both decode it, on the items it holds (their ids and
/// clock lengths). A room reads every inbound update with the reader before
/// yrs reads it, and refuses its sender (4422) when the reader can't: a
/// reader that refused an update yrs accepts would refuse an innocent
/// writer, and again on every reconnect.
pub fn disagreement(update: &[u8]) -> Option<String> {
    let read = decode(update).try_fold(IdSet::new(), |mut items, item| {
        let item = item?;
        items.insert(item.id, item.len);
        Ok::<_, yrs::encoding::read::Error>(items)
    });
    match (read, Update::decode_v1(update)) {
        (Ok(read), Ok(decoded)) => {
            // Every item yrs decoded: none is marked deleted yet.
            let items = decoded.insertions(false);
            (read != items).then(|| format!("the reader reads the items {read:?}, yrs {items:?}"))
        }
        (Ok(_), Err(e)) => Some(format!("the reader reads it, yrs doesn't: {e}")),
        (Err(e), Ok(_)) => Some(format!("yrs reads it, the reader doesn't: {e}")),
        (Err(_), Err(_)) => None,
    }
}

/// Panics with every disagreement on `updates` (see [`disagreements`]).
pub fn assert_read_as_yrs_reads(updates: Vec<(String, Vec<u8>)>) {
    let found: Vec<String> = updates
        .iter()
        .flat_map(|(what, update)| disagreements(what, update))
        .collect();
    let count = |how: &str| found.iter().filter(|d| d.contains(how)).count();
    assert!(
        found.is_empty(),
        "{} disagreement(s) over {} update(s) ({} where only the reader reads it, \
         {} where only yrs does, {} on the items), the first ones:\n{}",
        found.len(),
        updates.len(),
        count("the reader reads it, yrs doesn't"),
        count("yrs reads it, the reader doesn't"),
        count("the reader reads the items"),
        found[..found.len().min(10)].join("\n")
    );
}
