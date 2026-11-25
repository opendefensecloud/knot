//! Tables: a fixed grid of `w:gridCol`s from the column shares, every row
//! filled out to that grid, and spans as `w:gridSpan` and `w:vMerge`.

use crate::model::{Align, Block, CalloutKind, Cell, Table};
use crate::tables::{Slot, grid_edges, header_rows, layout};
use crate::xml::esc_text;

use super::body::QUOTE_INDENT_TWIPS;
use super::{Ctx, body};

/// Left plus right cell margin of the TableNormal style, in twips: what
/// the content of a cell loses of the cell's width.
const CELL_MARGINS_TWIPS: u32 = 216;

const V_MERGE_RESTART: &str = r#"<w:vMerge w:val="restart"/>"#;
const V_MERGE_CONTINUE: &str = "<w:vMerge/>";

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
    // A table inside a list item or a quote lines up with the text around
    // it, which `avail_twips` has already lost that indent for.
    let indent = cx.block_indent + cx.quote_depth * QUOTE_INDENT_TWIPS;
    let tbl_ind = if indent > 0 {
        format!(r#"<w:tblInd w:w="{indent}" w:type="dxa"/>"#)
    } else {
        String::new()
    };
    cx.xml.push_str(&format!(
        concat!(
            r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/>"#,
            r#"<w:tblW w:w="{avail}" w:type="dxa"/>{tbl_ind}"#,
            r#"<w:tblLayout w:type="fixed"/></w:tblPr><w:tblGrid>"#,
        ),
        avail = avail_twips,
        tbl_ind = tbl_ind,
    ));
    for col in edges.windows(2) {
        cx.xml
            .push_str(&format!(r#"<w:gridCol w:w="{}"/>"#, col[1] - col[0]));
    }
    cx.xml.push_str("</w:tblGrid>");

    let heads = header_rows(t);
    // The whole table is one row group: a row span is clamped to the end of
    // the table, also when it starts in a header row.
    let rows = t.rows.len();
    for (r, slots) in layout(t, move |_| rows).enumerate() {
        if cx.full() {
            break;
        }
        cx.xml.push_str("<w:tr>");
        if r < heads {
            cx.xml.push_str("<w:trPr><w:tblHeader/></w:trPr>");
        }
        for slot in slots {
            row_slot(cx, slot, &edges);
        }
        cx.xml.push_str("</w:tr>");
    }
    cx.xml.push_str("</w:tbl>");
}

/// One slot of a row: a cell, a continuation of a merge from a row above
/// (shaded like the cell it continues), or an empty cell filling a gap.
fn row_slot(cx: &mut Ctx, slot: Slot, edges: &[u32]) {
    match slot {
        Slot::Cell {
            cell: c,
            pos,
            span,
            rowspan,
        } => cell(cx, c, edges[pos + span] - edges[pos], span, rowspan > 1),
        Slot::Covered { pos, span, header } => {
            let twips = edges[pos + span] - edges[pos];
            empty_cell(cx, twips, span, Some(V_MERGE_CONTINUE), header);
        }
        Slot::Gap { pos, end } => {
            empty_cell(cx, edges[end] - edges[pos], end - pos, None, false);
        }
    }
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

/// Write a callout box `avail_twips` wide: a one-cell table bordered in the
/// kind's colour, its first paragraph the title (bold, in that colour, on
/// a light shade of it), then the box's blocks. A table keeps the box
/// together and readable in grey print, in Word and LibreOffice alike.
pub(super) fn callout(
    cx: &mut Ctx,
    kind: CalloutKind,
    title: Option<&str>,
    blocks: &[Block],
    avail_twips: u32,
) {
    if cx.xml.ends_with("</w:tbl>") {
        cx.xml.push_str("<w:p/>");
    }
    let indent = cx.block_indent + cx.quote_depth * QUOTE_INDENT_TWIPS;
    let tbl_ind = if indent > 0 {
        format!(r#"<w:tblInd w:w="{indent}" w:type="dxa"/>"#)
    } else {
        String::new()
    };
    let hue = kind.hue();
    let border =
        |side: &str| format!(r#"<w:{side} w:val="single" w:sz="8" w:space="0" w:color="{hue}"/>"#);
    cx.xml.push_str(&format!(
        concat!(
            r#"<w:tbl><w:tblPr><w:tblW w:w="{avail}" w:type="dxa"/>{tbl_ind}"#,
            r#"<w:tblBorders>{top}{left}{bottom}{right}</w:tblBorders>"#,
            r#"<w:tblLayout w:type="fixed"/></w:tblPr>"#,
            r#"<w:tblGrid><w:gridCol w:w="{avail}"/></w:tblGrid><w:tr><w:tc>"#,
            r#"<w:tcPr><w:tcW w:w="{avail}" w:type="dxa"/></w:tcPr>"#,
            r#"<w:p><w:pPr><w:shd w:val="clear" w:color="auto" w:fill="{shade}"/></w:pPr>"#,
            r#"<w:r><w:rPr><w:b/><w:color w:val="{hue}"/></w:rPr>"#,
            r#"<w:t xml:space="preserve">{title}</w:t></w:r></w:p>"#,
        ),
        avail = avail_twips,
        tbl_ind = tbl_ind,
        top = border("top"),
        left = border("left"),
        bottom = border("bottom"),
        right = border("right"),
        shade = kind.shade(),
        hue = hue,
        title = esc_text(title.unwrap_or(kind.label())),
    ));
    // The content starts at the cell's edge, as in a table cell.
    let outer = (cx.quote_depth, cx.list_depth, cx.block_indent);
    (cx.quote_depth, cx.list_depth, cx.block_indent) = (0, 0, 0);
    body::blocks(cx, blocks, avail_twips.saturating_sub(CELL_MARGINS_TWIPS));
    (cx.quote_depth, cx.list_depth, cx.block_indent) = outer;
    if !(cx.xml.ends_with("</w:p>") || cx.xml.ends_with("<w:p/>")) {
        cx.xml.push_str("<w:p/>");
    }
    cx.xml.push_str("</w:tc></w:tr></w:tbl>");
}
