//! A room reads every inbound update with knot's reader
//! (`knot_crdt::update_items`) before yrs reads it, and refuses the sender
//! of an update the reader can't read (4422): a reader that refused an
//! update yrs accepts would refuse an innocent writer, and again on every
//! reconnect. So the reader must read every update as yrs's decoder
//! (`Update::decode_v1`) does: whether it decodes, and, when it does, the
//! items it holds (their ids and clock lengths).
//!
//! The updates are those of the compat harness's corpus (`common::corpus`):
//! - the generated documents, each in the editor's and in the importer's
//!   representation (`Shape`);
//! - the random documents of the harness's seeds;
//! - every fixture and probe file, imported by today's importer and by the
//!   legacy one.
//!
//! For each document: its state (for a file, the update its import wrote),
//! what it holds past half its clocks (a diff that starts inside a block),
//! and, once its first block is deleted, that deletion (a delete set alone)
//! and its state then (collected ranges, deleted items and a delete set).
//! For the files, every update that ends early in one of those is compared
//! too: none decodes. knot-crdt's `tests/update_items.rs` and
//! `tests/nesting.rs` compare knot-crdt's own updates the same way.

mod common;

use common::corpus::{self, Case};
use knot_crdt::DocHandle;
use knot_crdt::update_items::decode;
use knot_markdown::{from_markdown, legacy};
use yrs::updates::decoder::Decode;
use yrs::{IdSet, ReadTxn, StateVector, Transact, Update, XmlFragment};

/// The compat harness's random corpus (`tests/compat.rs`): its seeds, and
/// documents per seed.
const RANDOM_SEEDS: std::ops::RangeInclusive<u64> = 1..=50;
const RANDOM_DOCS_PER_SEED: usize = 200;

/// A lower bound on the corpus documents' updates, so that a broken corpus
/// can't make the comparison pass by comparing nothing: far below today's
/// 94,544 (`KNOT_COMPAT_REPORT=1` prints the count).
const MIN_UPDATES: usize = 40_000;

/// How many disagreements the failure prints.
const SHOWN: usize = 10;

/// How the reader and yrs disagree on `update`, if they do.
fn disagreement(update: &[u8]) -> Option<String> {
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

/// The updates of `doc`, named after `what`, as the module doc lists them;
/// `stored` is the update its import wrote, if it was imported. Deletes the
/// document's first block.
fn updates_of(what: &str, doc: &DocHandle, stored: Option<Vec<u8>>) -> Vec<(String, Vec<u8>)> {
    let ydoc = doc.inner();
    let frag = ydoc.get_or_insert_xml_fragment("default");
    let state = |sv: &StateVector| ydoc.transact().encode_state_as_update_v1(sv);

    let mut out = vec![(
        format!("{what}: its state"),
        stored.unwrap_or_else(|| state(&StateVector::default())),
    )];
    let mut half = StateVector::default();
    for (client, clock) in ydoc.transact().state_vector().iter() {
        half.set_max(*client, clock / 2);
    }
    out.push((format!("{what}: past half its clocks"), state(&half)));
    if frag.len(&ydoc.transact()) > 0 {
        let before = ydoc.transact().state_vector();
        frag.remove_range(&mut ydoc.transact_mut(), 0, 1);
        out.push((format!("{what}: its first block deleted"), state(&before)));
        out.push((
            format!("{what}: its state then"),
            state(&StateVector::default()),
        ));
    }
    out
}

/// Panics with the disagreements `test` found (see the module doc). With
/// `KNOT_COMPAT_REPORT=1`, first prints how many updates it compared.
fn assert_none(test: &str, found: &[String], compared: usize) {
    common::report(test, || {
        format!("{compared} updates compared, {} disagreements", found.len())
    });
    let count = |how: &str| found.iter().filter(|d| d.contains(how)).count();
    assert!(
        found.is_empty(),
        "{} disagreement(s) over {compared} update(s) ({} where only the reader reads it, \
         {} where only yrs does, {} on the items), the first {}:\n{}",
        found.len(),
        count("the reader reads it, yrs doesn't"),
        count("yrs reads it, the reader doesn't"),
        count("the reader reads the items"),
        found.len().min(SHOWN),
        found[..found.len().min(SHOWN)].join("\n")
    );
}

#[test]
fn the_reader_reads_the_corpus_documents_as_yrs_does() {
    let mut cases: Vec<Case> = corpus::generated();
    for seed in RANDOM_SEEDS {
        cases.extend(corpus::random(seed, RANDOM_DOCS_PER_SEED));
    }
    let mut found = Vec::new();
    let mut compared = 0;
    for case in &cases {
        for (what, update) in updates_of(&case.name, &case.doc, None) {
            compared += 1;
            if let Some(how) = disagreement(&update) {
                found.push(format!("{what}: {how}"));
            }
        }
    }
    assert!(
        compared >= MIN_UPDATES,
        "the corpus shrank: {compared} updates"
    );
    assert_none("update_reader corpus documents", &found, compared);
}

#[test]
fn the_reader_reads_the_imported_files_as_yrs_does_and_every_update_that_ends_early() {
    let mut found = Vec::new();
    let mut compared = 0;
    for (name, md) in corpus::markdown_inputs() {
        let imports = [
            ("import", from_markdown::parse(&md).expect("import")),
            ("legacy import", legacy::parse(&md).expect("legacy import")),
        ];
        for (how, (doc, stored)) in imports {
            for (what, update) in updates_of(&format!("{how} of {name}"), &doc, Some(stored)) {
                for end in 0..=update.len() {
                    compared += 1;
                    if let Some(how) = disagreement(&update[..end]) {
                        found.push(format!(
                            "{what}, its first {end} of {}: {how}",
                            update.len()
                        ));
                    }
                }
            }
        }
    }
    assert_none("update_reader imported files", &found, compared);
}
