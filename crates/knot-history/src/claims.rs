//! Parked claims: content a row (or a gap merge) delivered that yrs could
//! not integrate yet, kept until a later row or merge integrates it.
//!
//! A claim is what its row *newly* parked (the pending structs' insertions
//! and pending deletes after the apply, minus before), never the row's whole
//! update. When a later apply integrates parked ids, [`Claims::credit`]
//! moves them out of that apply's effect and credits them to the claim's
//! author (Rulings: late content is credited to the row that parked it, at
//! the draining rev). A *gap claim* is what a gap merge newly parked; its
//! credits are gap content, never the draining row's author's.
//!
//! The claims open as of `built_rev` live in `doc_history_state.parked`
//! ([`Claims::to_json`] / [`Claims::from_json`]); the pending content itself
//! rides in each checkpoint.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;
use yrs::IdSet;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;

/// What one row (or one gap merge) newly parked.
#[derive(Debug, Clone, PartialEq)]
pub struct ParkedClaim {
    /// The rev of the row that parked it (the gap's rev for a gap claim).
    pub rev: i64,
    /// The parking row's author; `None` for a gap claim and author-less rows.
    pub author: Option<Uuid>,
    /// Parked by a gap merge, not by a row.
    pub gap: bool,
    pub inserts: IdSet,
    pub deletes: IdSet,
}

/// The part of one claim that an apply integrated.
#[derive(Debug, Clone, PartialEq)]
pub struct Credit {
    pub parked_rev: i64,
    pub author: Option<Uuid>,
    pub gap: bool,
    pub inserted: IdSet,
    pub deleted: IdSet,
}

/// `doc_history_state.parked` does not hold valid claims.
#[derive(Debug, Error)]
pub enum ClaimsError {
    #[error("parked claims are not a JSON array")]
    NotAnArray,
    #[error("parked claim {index}: {reason}")]
    Claim { index: usize, reason: String },
}

/// The open claims of a page, oldest first.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Claims {
    claims: Vec<ParkedClaim>,
}

impl Claims {
    /// Reads `doc_history_state.parked`: an array of
    /// `{"rev", "author", "gap", "inserts", "deletes"}`.
    pub fn from_json(v: &Value) -> Result<Claims, ClaimsError> {
        let items = v.as_array().ok_or(ClaimsError::NotAnArray)?;
        let mut claims = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let claim = parse_claim(item).map_err(|reason| ClaimsError::Claim { index, reason })?;
            claims.push(claim);
        }
        // Oldest first, whatever order was stored (the sort is stable, so a
        // row claim stays before the gap claim of its rev).
        claims.sort_by_key(|c| c.rev);
        Ok(Claims { claims })
    }

    /// The value stored in `doc_history_state.parked`; the sets are standard
    /// base64 of `IdSet::encode_v1`.
    pub fn to_json(&self) -> Value {
        Value::Array(
            self.claims
                .iter()
                .map(|c| {
                    json!({
                        "rev": c.rev,
                        "author": c.author,
                        "gap": c.gap,
                        "inserts": STANDARD.encode(c.inserts.encode_v1()),
                        "deletes": STANDARD.encode(c.deletes.encode_v1()),
                    })
                })
                .collect(),
        )
    }

    /// Records what row `rev` newly parked; nothing when both sets are empty.
    pub fn record(&mut self, rev: i64, author: Option<Uuid>, inserts: IdSet, deletes: IdSet) {
        self.push(ParkedClaim {
            rev,
            author,
            gap: false,
            inserts,
            deletes,
        });
    }

    /// Records what the gap merge at `rev` newly parked (a gap claim, author
    /// `None`); nothing when both sets are empty.
    pub fn record_gap(&mut self, rev: i64, inserts: IdSet, deletes: IdSet) {
        self.push(ParkedClaim {
            rev,
            author: None,
            gap: true,
            inserts,
            deletes,
        });
    }

    fn push(&mut self, claim: ParkedClaim) {
        if claim.inserts.is_empty() && claim.deletes.is_empty() {
            return;
        }
        self.claims.push(claim);
    }

    /// The revs of the open row claims (gap claims have no row), ascending.
    pub fn revs(&self) -> Vec<i64> {
        let mut revs: Vec<i64> = self
            .claims
            .iter()
            .filter(|c| !c.gap)
            .map(|c| c.rev)
            .collect();
        revs.sort_unstable();
        revs.dedup();
        revs
    }

    /// Credits what an apply integrated (`inserted`, `deleted`: its effect)
    /// to the claims that parked it, oldest claim first: the intersection
    /// moves from the effect into the credit and out of the claim; a claim
    /// left empty is dropped. What stays in the effect is the apply's own.
    pub fn credit(&mut self, inserted: &mut IdSet, deleted: &mut IdSet) -> Vec<Credit> {
        let mut credits = Vec::new();
        for claim in &mut self.claims {
            let ins = claim.inserts.intersect(inserted);
            let del = claim.deletes.intersect(deleted);
            if ins.is_empty() && del.is_empty() {
                continue;
            }
            inserted.diff_with(&ins);
            deleted.diff_with(&del);
            claim.inserts.diff_with(&ins);
            claim.deletes.diff_with(&del);
            credits.push(Credit {
                parked_rev: claim.rev,
                author: claim.author,
                gap: claim.gap,
                inserted: ins,
                deleted: del,
            });
        }
        self.claims
            .retain(|c| !(c.inserts.is_empty() && c.deletes.is_empty()));
        credits
    }

    /// The open claims, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &ParkedClaim> {
        self.claims.iter()
    }

    /// The number of open claims.
    pub fn len(&self) -> usize {
        self.claims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }
}

fn parse_claim(v: &Value) -> Result<ParkedClaim, String> {
    let rev = v
        .get("rev")
        .and_then(Value::as_i64)
        .ok_or("rev is not an integer")?;
    let author = match v.get("author") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(Uuid::parse_str(s).map_err(|e| format!("author: {e}"))?),
        Some(_) => return Err("author is neither a uuid nor null".into()),
    };
    let gap = v
        .get("gap")
        .and_then(Value::as_bool)
        .ok_or("gap is not a boolean")?;
    Ok(ParkedClaim {
        rev,
        author,
        gap,
        inserts: parse_set(v, "inserts")?,
        deletes: parse_set(v, "deletes")?,
    })
}

fn parse_set(v: &Value, field: &str) -> Result<IdSet, String> {
    let text = v
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} is not a string"))?;
    let bytes = STANDARD.decode(text).map_err(|e| format!("{field}: {e}"))?;
    IdSet::decode_v1(&bytes).map_err(|e| format!("{field}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use yrs::ID;
    use yrs::block::ClientID;

    fn ids(client: u64, clock: u32, len: u32) -> IdSet {
        let mut s = IdSet::new();
        s.insert(ID::new(ClientID::new(client), clock), len);
        s
    }

    #[test]
    fn claims_credit_the_parking_rows_author_first() {
        let a = Uuid::new_v4();
        let b_own = ids(3, 0, 4);
        let parked_ins = ids(1, 0, 3);
        let parked_del = ids(2, 0, 2);
        let mut claims = Claims::default();
        claims.record(5, Some(a), parked_ins.clone(), parked_del.clone());
        assert_eq!(claims.len(), 1);

        // b's row integrates a's parked content and its own.
        let mut ins = parked_ins.merge(&b_own);
        let mut del = parked_del.merge(&ids(2, 10, 1));
        let credits = claims.credit(&mut ins, &mut del);

        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0].author, Some(a));
        assert_eq!(credits[0].parked_rev, 5);
        assert!(!credits[0].gap);
        assert_eq!(credits[0].inserted, parked_ins);
        assert_eq!(credits[0].deleted, parked_del);
        assert_eq!(ins, b_own);
        assert_eq!(del, ids(2, 10, 1));
        assert_eq!(claims.len(), 0);
    }

    #[test]
    fn claims_credit_oldest_first_and_shrink_until_drained() {
        let a = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut claims = Claims::default();
        claims.record(3, Some(a), ids(1, 0, 3), IdSet::new());
        claims.record(5, Some(c), ids(4, 0, 2), IdSet::new());

        // A row drains c's claim and the first id of a's.
        let mut ins = ids(4, 0, 2).merge(&ids(1, 0, 1));
        let mut del = IdSet::new();
        let credits = claims.credit(&mut ins, &mut del);
        assert_eq!(credits.len(), 2);
        assert_eq!((credits[0].parked_rev, credits[0].author), (3, Some(a)));
        assert_eq!(credits[0].inserted, ids(1, 0, 1));
        assert_eq!((credits[1].parked_rev, credits[1].author), (5, Some(c)));
        assert_eq!(credits[1].inserted, ids(4, 0, 2));
        assert!(ins.is_empty());
        assert_eq!(claims.len(), 1, "a's claim keeps what is still parked");

        // A later row drains the rest of a's claim.
        let mut ins = ids(1, 1, 2).merge(&ids(9, 0, 1));
        let credits = claims.credit(&mut ins, &mut del);
        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0].inserted, ids(1, 1, 2));
        assert_eq!(ins, ids(9, 0, 1));
        assert_eq!(claims.len(), 0);

        // Nothing parked: nothing credited, the row keeps its effect.
        let mut ins = ids(9, 1, 1);
        assert!(claims.credit(&mut ins, &mut del).is_empty());
        assert_eq!(ins, ids(9, 1, 1));
    }

    #[test]
    fn an_empty_claim_is_not_recorded() {
        let mut claims = Claims::default();
        claims.record(1, Some(Uuid::new_v4()), IdSet::new(), IdSet::new());
        claims.record_gap(2, IdSet::new(), IdSet::new());
        assert_eq!(claims.len(), 0);
        assert_eq!(claims.to_json(), json!([]));
    }

    #[test]
    fn gap_claims_credit_as_gap_content() {
        let parked = ids(1, 0, 3);
        let mut claims = Claims::default();
        claims.record_gap(5, parked.clone(), IdSet::new());
        assert!(claims.revs().is_empty(), "a gap claim has no row");

        let mut ins = parked.clone();
        let mut del = IdSet::new();
        let credits = claims.credit(&mut ins, &mut del);
        assert_eq!(credits.len(), 1);
        assert!(credits[0].gap);
        assert_eq!(credits[0].author, None);
        assert_eq!(credits[0].parked_rev, 5);
        assert_eq!(credits[0].inserted, parked);
        assert!(ins.is_empty());
        assert_eq!(claims.len(), 0);
    }

    #[test]
    fn revs_are_the_open_row_claims_ascending() {
        let a = Uuid::new_v4();
        let mut claims = Claims::default();
        claims.record(3, Some(a), ids(1, 0, 1), IdSet::new());
        claims.record_gap(5, ids(2, 0, 1), IdSet::new());
        claims.record(7, None, IdSet::new(), ids(3, 0, 1));
        assert_eq!(claims.revs(), vec![3, 7]);
    }

    #[test]
    fn claims_round_trip_through_json() {
        let a = Uuid::new_v4();
        let mut claims = Claims::default();
        claims.record(3, Some(a), ids(1, 0, 3), ids(2, 4, 2));
        claims.record(4, None, ids(1, 7, 1), IdSet::new());
        claims.record_gap(5, ids(5, 0, 2), ids(6, 1, 1));

        let v = claims.to_json();
        assert_eq!(v.as_array().map(Vec::len), Some(3));
        assert_eq!(v[0]["rev"], json!(3));
        assert_eq!(v[0]["author"], json!(a.to_string()));
        assert_eq!(v[0]["gap"], json!(false));
        assert!(v[0]["inserts"].is_string());
        assert!(v[0]["deletes"].is_string());
        assert_eq!(v[1]["author"], json!(null));
        assert_eq!(v[2]["gap"], json!(true));
        assert_eq!(v[2]["author"], json!(null));

        let back = Claims::from_json(&v).expect("round trip");
        assert_eq!(back, claims);
        assert_eq!(back.to_json(), v);
        assert_eq!(back.revs(), vec![3, 4]);
    }

    #[test]
    fn the_columns_default_reads_as_no_claims() {
        let claims = Claims::from_json(&json!([])).expect("empty");
        assert_eq!(claims.len(), 0);
    }

    #[test]
    fn malformed_claims_are_an_error() {
        let ok = Claims::default().to_json();
        assert!(Claims::from_json(&ok).is_ok());
        for bad in [
            json!({}),
            json!([1]),
            json!([{"rev": "x", "author": null, "gap": false, "inserts": "", "deletes": ""}]),
            json!([{"rev": 1, "author": "not-a-uuid", "gap": false, "inserts": "AA==", "deletes": "AA=="}]),
            json!([{"rev": 1, "author": null, "gap": false, "inserts": "%%%", "deletes": "AA=="}]),
            json!([{"rev": 1, "author": null, "gap": false, "inserts": "/w==", "deletes": "AA=="}]),
            json!([{"rev": 1, "author": null, "inserts": "AA==", "deletes": "AA=="}]),
        ] {
            assert!(Claims::from_json(&bad).is_err(), "{bad} must not parse");
        }
    }
}
