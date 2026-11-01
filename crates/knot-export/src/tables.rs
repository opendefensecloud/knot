//! Table helpers shared by the writers: column shares, header detection, and
//! the layout of a table's cells on its grid.

use std::collections::BTreeMap;

use crate::model::{Cell, Table};

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

/// Column boundaries from the table's left edge, `shares.len() + 1` of them
/// from 0 to `total` (only `[0]` without columns), in whatever unit `total`
/// is in. Each column gets its share of `total`, rounded, and the last one
/// what is left, so they add up exactly.
pub fn grid_edges(shares: &[f64], total: u32) -> Vec<u32> {
    let mut edges = Vec::with_capacity(shares.len() + 1);
    edges.push(0);
    let Some((_, init)) = shares.split_last() else {
        return edges;
    };
    let mut at = 0;
    for share in init {
        let width = (share * f64::from(total)).round() as u32;
        at += width.min(total - at);
        edges.push(at);
    }
    edges.push(total);
    edges
}

/// What fills a stretch of grid columns in one row of a laid-out table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Slot<'a> {
    /// `cell` at grid column `pos`, spanning `span` columns and `rowspan`
    /// rows, both clamped by [`layout`] and at least 1.
    Cell {
        cell: &'a Cell,
        pos: usize,
        span: usize,
        rowspan: usize,
    },
    /// Columns `pos..pos + span`, covered by the row span of a cell in a row
    /// above. `header` is whether that cell is a header cell.
    Covered {
        pos: usize,
        span: usize,
        header: bool,
    },
    /// Columns `pos..end`, which no cell fills.
    Gap { pos: usize, end: usize },
}

/// A row span that still covers rows below the current one.
#[derive(Clone, Copy)]
struct RowSpan {
    /// Grid columns it spans.
    span: usize,
    /// Rows it still covers, counting from the next row laid out.
    rows_left: usize,
    /// Whether it started in a header cell.
    header: bool,
}

/// Lay `t` out on its grid of `t.shares.len()` columns: for each row, the
/// slots that fill it left to right, so that every row has exactly the grid
/// width.
///
/// - Each cell goes at its column, or at the next free one when a cell or
///   span before it already reaches past that column. A cell that starts
///   past the grid is dropped.
/// - A column span is clamped to the grid and stops short of a row span
///   from above.
/// - A row span is clamped to the rows from its row up to `group_end(row)`,
///   the end (exclusive row index) of the row group it starts in: the end
///   of the table, or of the header rows when they form a group of their
///   own. No row span is less than 1, whatever `group_end` returns.
/// - Each gap is one slot, however wide, so a crafted wide grid can't
///   multiply the output.
pub fn layout<'a>(
    t: &'a Table,
    group_end: impl Fn(usize) -> usize + 'a,
) -> impl Iterator<Item = Vec<Slot<'a>>> + 'a {
    let width = t.shares.len();
    // Open row spans by their first grid column. They never overlap.
    let mut spans = BTreeMap::new();
    t.rows.iter().enumerate().map(move |(r, cells)| {
        let rows_to_end = group_end(r).saturating_sub(r).max(1);
        layout_row(cells, width, rows_to_end, &mut spans)
    })
}

/// One row of [`layout`], `rows_to_end` (at least 1) rows from the end of
/// its group, given the row spans reaching into it from above.
fn layout_row<'a>(
    cells: &'a [Cell],
    width: usize,
    rows_to_end: usize,
    spans: &mut BTreeMap<usize, RowSpan>,
) -> Vec<Slot<'a>> {
    let mut slots = Vec::new();
    let mut opened = Vec::new();
    let mut cells = cells.iter().peekable();
    let mut pos = 0;
    while pos < width {
        let next_span = spans.range(pos..).next().map(|(col, s)| (*col, *s));
        if let Some((_, s)) = next_span.filter(|(col, _)| *col == pos) {
            slots.push(Slot::Covered {
                pos,
                span: s.span,
                header: s.header,
            });
            pos += s.span;
            continue;
        }
        // The free columns run from `pos` up to the next row span, which
        // starts after `pos`.
        let limit = next_span.map_or(width, |(col, _)| col);
        if let Some(cell) = cells.next_if(|c| c.col <= pos) {
            let span = cell.colspan.clamp(1, limit - pos);
            let rowspan = cell.rowspan.clamp(1, rows_to_end);
            if rowspan > 1 {
                opened.push((
                    pos,
                    RowSpan {
                        span,
                        rows_left: rowspan - 1,
                        header: cell.header,
                    },
                ));
            }
            slots.push(Slot::Cell {
                cell,
                pos,
                span,
                rowspan,
            });
            pos += span;
        } else {
            // The next cell, if there is one, starts after `pos`.
            let end = cells.peek().map_or(limit, |c| c.col.min(limit));
            slots.push(Slot::Gap { pos, end });
            pos = end;
        }
    }
    spans.retain(|_, s| {
        s.rows_left -= 1;
        s.rows_left > 0
    });
    spans.extend(opened);
    slots
}
