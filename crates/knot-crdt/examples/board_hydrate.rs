//! cargo run --bin board_hydrate
use sqlx::{PgPool, Row};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{Doc, Map, ReadTxn, StateVector, Transact, Update};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://knot:knot@localhost:5432/knot".into());
    let pool = PgPool::connect(&url).await?;
    let board_id: Uuid = "9b7453b7-bf85-4d97-913c-b56c111cba59".parse()?;

    let rows = sqlx::query("SELECT seq, bytes FROM board_updates WHERE board_id=$1 ORDER BY seq")
        .bind(board_id)
        .fetch_all(&pool)
        .await?;
    println!("loaded {} updates", rows.len());

    let doc = Doc::new();
    let mut applied = 0;
    let mut failed = 0;
    for (i, row) in rows.iter().enumerate() {
        let bytes: Vec<u8> = row.get("bytes");
        let seq: i64 = row.get("seq");
        match Update::decode_v1(&bytes) {
            Ok(u) => {
                let mut txn = doc.transact_mut();
                if let Err(e) = txn.apply_update(u) {
                    failed += 1;
                    eprintln!("seq {}: apply failed: {:?}", seq, e);
                } else {
                    applied += 1;
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("seq {}: decode failed: {:?}", seq, e);
            }
        }
        if i < 3 {
            println!("update {} size={}", seq, bytes.len());
        }
    }
    println!("applied={} failed={}", applied, failed);

    let txn = doc.transact();
    let elements = txn.get_map("elements");
    println!("get_map(elements) = {:?}", elements.is_some());
    if let Some(m) = &elements {
        println!("elements.len() = {}", m.len(&txn));
        for (k, v) in m.iter(&txn).take(3) {
            println!("  {} => {:?}", k, v);
        }
    }
    // Also list top-level keys
    println!("root keys:");
    for (k, _v) in txn.root_refs() {
        println!("  {}", k);
    }
    // Re-encode the state as if for a fresh client (None SV).
    let txn = doc.transact();
    let state = txn.encode_state_as_update_v1(&StateVector::default());
    println!(
        "encode_state_as_update_v1(empty SV) -> {} bytes",
        state.len()
    );

    // Apply the encoded state to a FRESH doc and check element count.
    let doc2 = Doc::new();
    {
        let u = Update::decode_v1(&state).expect("decode roundtrip");
        let mut t2 = doc2.transact_mut();
        t2.apply_update(u).expect("apply roundtrip");
    }
    let t2 = doc2.transact();
    let elements2 = t2.get_map("elements");
    println!(
        "roundtrip elements.len() = {}",
        elements2.map(|m| m.len(&t2)).unwrap_or(0)
    );
    Ok(())
}
