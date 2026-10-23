//! Tables: a fixed grid of `w:gridCol`s from the column shares, every row
//! filled out to that grid, and spans as `w:gridSpan` and `w:vMerge`.

use std::collections::BTreeMap;

use crate::model::{Align, Cell, Table};
use crate::tables::header_rows;

use super::{Ctx, body};

/// Left plus right cell margin of the TableNormal style, in twips: what
/// the content of a cell loses of the cell's width.
const CELL_MARGINS_TWIPS: u32 = 216;

const V_MERGE_RESTART: &str = r#"<w:vMerge w:val="restart"/>"#;
const V_MERGE_CONTINUE: &str = "<w:vMerge/>";

/// A vertical merge that still covers rows below the current one.
#[derive(Clone, Copy)]
struct Merge {
    /// Grid columns it spans.
    span: usize,
    /// Rows it still covers, counting from the next row written.
    rows_left: usize,
    /// Whether it started in a header cell; its continuations are shaded
    /// like it.
    header: bool,
}

/// Write `t`, `avail_twips` wide. A table without columns or rows writes
/// nothing. Word joins adjacent tables into one, so a table right after
/// another gets an empty paragraph in between.
pub(super) fn table(cx: &mut Ctx, t: &Table, avail_twips: u32) {
    if t.shares.is_empty() || t.rows.is_empty() {
        return;
    }
    if cx.xml.ends_with("</w:tbl>") {
        cx.xml.push_str("<w:p/>");
    }
    let edges = grid_edges(&t.shares, avail_twips);
    cx.xml.push_str(&format!(
        concat!(
            r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/>"#,
            r#"<w:tblW w:w="{avail}" w:type="dxa"/><w:tblLayout w:type="fixed"/></w:tblPr>"#,
            "<w:tblGrid>",
        ),
        avail = avail_twips,
    ));
    for col in edges.windows(2) {
        cx.xml
            .push_str(&format!(r#"<w:gridCol w:w="{}"/>"#, col[1] - col[0]));
    }
    cx.xml.push_str("</w:tblGrid>");

    let heads = header_rows(t);
    // Open vertical merges by their first grid column. They never overlap.
    let mut merges = BTreeMap::new();
    for (r, cells) in t.rows.iter().enumerate() {
        cx.xml.push_str("<w:tr>");
        if r < heads {
            cx.xml.push_str("<w:trPr><w:tblHeader/></w:trPr>");
        }
        row(cx, cells, t.rows.len() - r, &edges, &mut merges);
        cx.xml.push_str("</w:tr>");
    }
    cx.xml.push_str("</w:tbl>");
}

/// Column boundaries in twips from the table's left edge, `shares.len() + 1`
/// of them from 0 to `avail_twips`. Each column gets its share of the
/// width, rounded, and the last one what is left, so they add up exactly.
fn grid_edges(shares: &[f64], avail_twips: u32) -> Vec<u32> {
    let mut edges = vec![0];
    let mut at = 0;
    for share in &shares[..shares.len().saturating_sub(1)] {
        let width = (share * f64::from(avail_twips)).round() as u32;
        at += width.min(avail_twips - at);
        edges.push(at);
    }
    edges.push(avail_twips);
    edges
}

/// One row across the whole grid, left to right: each cell at its column, a
/// continuation wherever a merge from a row above reaches into this one, and
/// an empty cell for each gap that is left. A column span is clamped to the
/// grid and stops short of a merge; a row span is clamped to the
/// `rows_to_end` rows from this one to the end of the table.
fn row(
    cx: &mut Ctx,
    cells: &[Cell],
    rows_to_end: usize,
    edges: &[u32],
    merges: &mut BTreeMap<usize, Merge>,
) {
    let width = edges.len() - 1;
    let mut opened = Vec::new();
    let mut cells = cells.iter().peekable();
    let mut pos = 0;
    while pos < width {
        let next_merge = merges.range(pos..).next().map(|(col, m)| (*col, *m));
        if let Some((col, m)) = next_merge.filter(|(col, _)| *col == pos) {
            let twips = edges[col + m.span] - edges[col];
            empty_cell(cx, twips, m.span, Some(V_MERGE_CONTINUE), m.header);
            pos += m.span;
            continue;
        }
        let limit = next_merge.map_or(width, |(col, _)| col);
        if let Some(c) = cells.next_if(|c| c.col <= pos) {
            let span = c.colspan.clamp(1, limit - pos);
            let rowspan = c.rowspan.clamp(1, rows_to_end);
            if rowspan > 1 {
                opened.push((
                    pos,
                    Merge {
                        span,
                        rows_left: rowspan - 1,
                        header: c.header,
                    },
                ));
            }
            cell(cx, c, edges[pos + span] - edges[pos], span, rowspan > 1);
            pos += span;
        } else {
            // One cell per gap, however wide, so a crafted wide grid can't
            // multiply the output.
            let end = cells.peek().map_or(limit, |c| c.col.min(limit));
            empty_cell(cx, edges[end] - edges[pos], end - pos, None, false);
            pos = end;
        }
    }
    merges.retain(|_, m| {
        m.rows_left -= 1;
        m.rows_left > 0
    });
    merges.extend(opened);
}

/// A cell with `c`'s blocks, `twips` wide over `span` grid columns.
fn cell(cx: &mut Ctx, c: &Cell, twips: u32, span: usize, restart: bool) {
    cx.xml.push_str("<w:tc>");
    tc_pr(
        &mut cx.xml,
        twips,
        span,
        restart.then_some(V_MERGE_RESTART),
        c.header,
    );
    // The content starts at the cell's edge: no quote, list or list-item
    // indent from around the table carries into it.
    let outer = (
        cx.quote_depth,
        cx.list_depth,
        cx.block_indent,
        cx.header_cell,
        cx.cell_jc,
    );
    (cx.quote_depth, cx.list_depth, cx.block_indent) = (0, 0, 0);
    cx.header_cell = c.header;
    cx.cell_jc = match c.align {
        Align::Left => None,
        Align::Center => Some("center"),
        Align::Right => Some("right"),
    };
    body::blocks(cx, &c.blocks, twips.saturating_sub(CELL_MARGINS_TWIPS));
    (
        cx.quote_depth,
        cx.list_depth,
        cx.block_indent,
        cx.header_cell,
        cx.cell_jc,
    ) = outer;
    // A cell must end in a paragraph, also when it is empty or ends in a
    // table.
    if !(cx.xml.ends_with("</w:p>") || cx.xml.ends_with("<w:p/>")) {
        cx.xml.push_str("<w:p/>");
    }
    cx.xml.push_str("</w:tc>");
}

/// A cell holding only an empty paragraph: a merge continuation or padding.
fn empty_cell(cx: &mut Ctx, twips: u32, span: usize, v_merge: Option<&str>, header: bool) {
    cx.xml.push_str("<w:tc>");
    tc_pr(&mut cx.xml, twips, span, v_merge, header);
    cx.xml.push_str("<w:p/></w:tc>");
}

/// `w:tcPr` in schema order: `tcW, gridSpan, vMerge, shd`. Header cells are
/// shaded.
fn tc_pr(out: &mut String, twips: u32, span: usize, v_merge: Option<&str>, header: bool) {
    out.push_str(&format!(r#"<w:tcPr><w:tcW w:w="{twips}" w:type="dxa"/>"#));
    if span > 1 {
        out.push_str(&format!(r#"<w:gridSpan w:val="{span}"/>"#));
    }
    if let Some(v_merge) = v_merge {
        out.push_str(v_merge);
    }
    if header {
        out.push_str(r#"<w:shd w:val="clear" w:color="auto" w:fill="F2F2F2"/>"#);
    }
    out.push_str("</w:tcPr>");
}
