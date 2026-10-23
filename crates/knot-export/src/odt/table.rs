//! Tables: a `table:table-column` per grid column, sized from the column
//! shares, and every row filled out to the grid. A span is
//! `number-columns-spanned` / `number-rows-spanned` on its cell plus a
//! `table:covered-table-cell` for each other grid position it covers.

use std::collections::BTreeMap;

use crate::model::{Align, Cell, Table};
use crate::tables::header_rows;

use super::{CellCx, Ctx, body};

/// Left and right padding of a table cell, in cm: what the content of a cell
/// loses of the cell's width on each side. The cell styles below use it.
const CELL_PADDING_CM: f64 = 0.19;
/// What the `style:rel-column-width`s of a table add up to.
const REL_WIDTH_TOTAL: f64 = 65535.0;

const CELL: &str = "Cell";
const HEADER_CELL: &str = "HeaderCell";

/// Thin grey borders on every cell, and shading on header cells.
const CELL_STYLES: &str = concat!(
    r#"<style:style style:name="Cell" style:family="table-cell">"#,
    r#"<style:table-cell-properties fo:padding-top="0cm" fo:padding-bottom="0cm" "#,
    r#"fo:padding-left="0.19cm" fo:padding-right="0.19cm" fo:border="0.5pt solid #bfbfbf"/>"#,
    r#"</style:style>"#,
    r#"<style:style style:name="HeaderCell" style:family="table-cell">"#,
    r##"<style:table-cell-properties fo:background-color="#f2f2f2" fo:padding-top="0cm" fo:padding-bottom="0cm" "##,
    r#"fo:padding-left="0.19cm" fo:padding-right="0.19cm" fo:border="0.5pt solid #bfbfbf"/>"#,
    r#"</style:style>"#,
);

/// A row span that still covers rows below the current one.
#[derive(Clone, Copy)]
struct Merge {
    /// Grid columns it spans.
    span: usize,
    /// Rows it still covers, counting from the next row written.
    rows_left: usize,
}

/// Write `t`, `avail_cm` wide. A table without columns or rows writes
/// nothing.
pub(super) fn table(cx: &mut Ctx, t: &Table, avail_cm: f64) {
    let width = t.shares.len();
    if width == 0 || t.rows.is_empty() {
        return;
    }
    // A list item can't hold a table: close the lists around it, and
    // continue them after it. The table lines up with the text it
    // interrupts.
    let lists = body::close_lists(cx);
    let indent_cm = body::indent_cm(cx, &lists);
    // Widths in thousandths of a cm; every column gets at least one.
    let total = ((avail_cm * 1000.0).round() as u32).max(width as u32);
    let edges = grid_edges(&t.shares, total);
    cx.last_table += 1;
    let name = format!("Table{}", cx.last_table);
    styles(cx, &name, &t.shares, &edges, indent_cm);

    cx.body.push_str(&format!(
        r#"<table:table table:name="{name}" table:style-name="{name}">"#
    ));
    for i in 1..=width {
        cx.body.push_str(&format!(
            r#"<table:table-column table:style-name="{name}.C{i}"/>"#
        ));
    }
    // The header rows form a group of their own, which no row span leaves.
    let heads = header_rows(t);
    let mut merges = BTreeMap::new();
    for (r, cells) in t.rows.iter().enumerate() {
        if r == 0 && heads > 0 {
            cx.body.push_str("<table:table-header-rows>");
        }
        let group_end = if r < heads { heads } else { t.rows.len() };
        row(cx, cells, group_end - r, &edges, &mut merges);
        if r + 1 == heads {
            cx.body.push_str("</table:table-header-rows>");
        }
    }
    cx.body.push_str("</table:table>");
    body::reopen_lists(cx, lists);
}

/// Column boundaries in thousandths of a cm from the table's left edge,
/// `shares.len() + 1` of them from 0 to `total`. Each column gets its share
/// of the width, rounded, and the last one what is left, so they add up
/// exactly.
fn grid_edges(shares: &[f64], total: u32) -> Vec<u32> {
    let mut edges = vec![0];
    let mut at = 0;
    for share in &shares[..shares.len() - 1] {
        let width = (share * f64::from(total)).round() as u32;
        at += width.min(total - at);
        edges.push(at);
    }
    edges.push(total);
    edges
}

/// A length in thousandths of a cm, never zero (ODF lengths of tables and
/// columns must be positive).
fn cm(thousandths: u32) -> String {
    let n = thousandths.max(1);
    format!("{}.{:03}cm", n / 1000, n % 1000)
}

/// The automatic styles of table `name`: its width and indent, each
/// column's absolute and relative width, and, once per document, the cell
/// styles.
fn styles(cx: &mut Ctx, name: &str, shares: &[f64], edges: &[u32], indent_cm: f64) {
    let margin = if indent_cm > 0.0 {
        format!(r#" fo:margin-left="{indent_cm:.3}cm""#)
    } else {
        String::new()
    };
    cx.auto_styles.push_str(&format!(
        concat!(
            r#"<style:style style:name="{name}" style:family="table">"#,
            r#"<style:table-properties style:width="{width}" table:align="margins"{margin}/></style:style>"#,
        ),
        name = name,
        width = cm(edges[edges.len() - 1]),
        margin = margin,
    ));
    for (i, (col, share)) in edges.windows(2).zip(shares).enumerate() {
        let rel = ((share * REL_WIDTH_TOTAL).round() as u32).max(1);
        cx.auto_styles.push_str(&format!(
            concat!(
                r#"<style:style style:name="{name}.C{n}" style:family="table-column">"#,
                r#"<style:table-column-properties style:column-width="{width}" style:rel-column-width="{rel}*"/>"#,
                r#"</style:style>"#,
            ),
            name = name,
            n = i + 1,
            width = cm(col[1] - col[0]),
            rel = rel,
        ));
    }
    if !cx.cell_styles {
        cx.auto_styles.push_str(CELL_STYLES);
        cx.cell_styles = true;
    }
}

/// One row across the whole grid, left to right: each cell at its column,
/// covered cells wherever a row span from above reaches into this one, and
/// one empty cell spanning each gap that is left. A column span is clamped
/// to the grid and stops short of a row span; a row span is clamped to the
/// `rows_to_end` rows from this one to the end of its row group.
fn row(
    cx: &mut Ctx,
    cells: &[Cell],
    rows_to_end: usize,
    edges: &[u32],
    merges: &mut BTreeMap<usize, Merge>,
) {
    let width = edges.len() - 1;
    cx.body.push_str("<table:table-row>");
    let mut opened = Vec::new();
    let mut cells = cells.iter().peekable();
    let mut pos = 0;
    while pos < width {
        let next_merge = merges.range(pos..).next().map(|(col, m)| (*col, *m));
        if let Some((_, m)) = next_merge.filter(|(col, _)| *col == pos) {
            covered(cx, m.span);
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
                    },
                ));
            }
            cell(cx, c, edges[pos + span] - edges[pos], span, rowspan);
            pos += span;
        } else {
            let end = cells.peek().map_or(limit, |c| c.col.min(limit));
            open_cell(cx, CELL, end - pos, 1);
            cx.body.push_str("</table:table-cell>");
            covered(cx, end - pos - 1);
            pos = end;
        }
    }
    merges.retain(|_, m| {
        m.rows_left -= 1;
        m.rows_left > 0
    });
    merges.extend(opened);
    cx.body.push_str("</table:table-row>");
}

/// The opening tag of a cell spanning `span` columns and `rows` rows.
fn open_cell(cx: &mut Ctx, style: &str, span: usize, rows: usize) {
    cx.body
        .push_str(&format!(r#"<table:table-cell table:style-name="{style}""#));
    if span > 1 {
        cx.body
            .push_str(&format!(r#" table:number-columns-spanned="{span}""#));
    }
    if rows > 1 {
        cx.body
            .push_str(&format!(r#" table:number-rows-spanned="{rows}""#));
    }
    cx.body.push('>');
}

/// A cell with `c`'s blocks, `thousandths` of a cm wide, then the covered
/// cells of the rest of its column span.
fn cell(cx: &mut Ctx, c: &Cell, thousandths: u32, span: usize, rows: usize) {
    open_cell(cx, if c.header { HEADER_CELL } else { CELL }, span, rows);
    // The content starts at the cell's edge: no quote around the table
    // carries into it (the lists around it are closed already).
    let outer = (cx.quote_depth, cx.cell);
    cx.quote_depth = 0;
    cx.cell = Some(CellCx {
        header: c.header,
        align: match c.align {
            Align::Left => "start",
            Align::Center => "center",
            Align::Right => "end",
        },
    });
    let avail_cm = (f64::from(thousandths) / 1000.0 - 2.0 * CELL_PADDING_CM).max(0.0);
    body::blocks(cx, &c.blocks, avail_cm);
    (cx.quote_depth, cx.cell) = outer;
    cx.body.push_str("</table:table-cell>");
    covered(cx, span - 1);
}

fn covered(cx: &mut Ctx, n: usize) {
    for _ in 0..n {
        cx.body.push_str("<table:covered-table-cell/>");
    }
}
