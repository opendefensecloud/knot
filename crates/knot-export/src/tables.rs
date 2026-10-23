//! Table helpers: column shares and header detection.

use crate::model::Table;

/// Committed column weights add up to this (mirrors the editor).
pub const WEIGHT_TOTAL: f64 = 1000.0;

fn usable(w: Option<f64>) -> Option<f64> {
    w.filter(|v| v.is_finite() && *v > 0.0)
}

/// Fractions of the table width (summing to 1) for stored column weights.
/// Port of `resolveShares` in `web/src/features/editor/columnWidths.ts`.
pub fn resolve_shares(weights: &[Option<f64>]) -> Vec<f64> {
    let known: Vec<f64> = weights.iter().filter_map(|w| usable(*w)).collect();
    let known_total: f64 = known.iter().sum();
    let unset = weights.len() - known.len();
    let leftover = WEIGHT_TOTAL - known_total;
    let fallback = if known.is_empty() {
        1.0
    } else if leftover > 0.0 && unset > 0 {
        leftover / unset as f64
    } else {
        known_total / known.len() as f64
    };
    let filled: Vec<f64> = weights
        .iter()
        .map(|w| usable(*w).unwrap_or(fallback))
        .collect();
    let total: f64 = filled.iter().sum();
    filled.into_iter().map(|w| w / total).collect()
}

/// Number of leading rows in which every cell is a header cell.
pub fn header_rows(table: &Table) -> usize {
    table
        .rows
        .iter()
        .take_while(|r| !r.is_empty() && r.iter().all(|c| c.header))
        .count()
}
