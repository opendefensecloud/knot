//! The generator of the web editor's handshake fixture
//! (`web/src/features/editor/__fixtures__/pendingHandshake.json`), which
//! `KnotProvider.test.ts` feeds to a yjs provider as a fixed room's frames:
//! SyncStep2 of the pending-free state, then the rows that park the pending
//! content as sync Update frames, then what drains them (Ruling 5).
//!
//! It lives in its own test binary and needs only `fixtures.rs` and
//! `knot_crdt::pending`, so the fixture can be regenerated whatever the rest
//! of the crate's tests compile against.
//!
//! The test fails, printing the new JSON, when the file differs from what
//! the fixtures give; `KNOT_UPDATE_FIXTURES=1` writes the file instead.

#[path = "common/fixtures.rs"]
mod fixtures;

use fixtures::{cross_client_parked, overlapping_skips, skip_underflow_case};
use knot_crdt::pending::encode_without_pending;
use serde_json::{Value, json};
use yrs::updates::decoder::Decode;
use yrs::{Doc, StateVector, Transact, Update};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hexes(updates: &[&[u8]]) -> Value {
    Value::Array(updates.iter().map(|u| Value::String(hex(u))).collect())
}

/// The pending-free state of a fresh doc that applied `updates` in order:
/// what a fixed room's SyncStep2 carries.
fn state_of(updates: &[&[u8]]) -> String {
    let doc = Doc::new();
    for u in updates {
        doc.transact_mut()
            .apply_update(Update::decode_v1(u).expect("the update decodes"))
            .expect("the update applies");
    }
    hex(&encode_without_pending(
        &doc.transact(),
        &StateVector::default(),
    ))
}

fn fixture() -> Value {
    let (u1, u2) = cross_client_parked();
    let o = overlapping_skips();
    let (underflow_state, underflow_row, _) = skip_underflow_case();
    json!({
        "crossClient": {
            "state": state_of(&[&u2]),
            "frames": hexes(&[&u2]),
            "dependency": hex(&u1),
            "all": hexes(&[&u1, &u2]),
        },
        "overlapping": {
            "state": state_of(&[&o.t0, &o.t2, &o.t3]),
            "frames": hexes(&[&o.t2, &o.t3]),
            "dependencies": hexes(&[&o.t1, &o.t2b]),
            "all": hexes(&[&o.t0, &o.t1, &o.t2, &o.t2b, &o.t3]),
        },
        "underflow": {
            "state": state_of(&[&underflow_state]),
            "frames": hexes(&[&underflow_row]),
        },
    })
}

#[test]
fn the_web_handshake_fixture_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/src/features/editor/__fixtures__/pendingHandshake.json");
    let want = fixture();
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&want).expect("the fixture serializes")
    );
    if std::env::var_os("KNOT_UPDATE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the fixture's directory");
        std::fs::write(&path, &text).expect("the fixture is written");
        return;
    }
    let have: Option<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    assert!(
        have.as_ref() == Some(&want),
        "{} is not current; run with KNOT_UPDATE_FIXTURES=1, or write:\n{text}",
        path.display()
    );
}
