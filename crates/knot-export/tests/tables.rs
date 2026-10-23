mod common;

use common::*;
use knot_export::tables::{header_rows, resolve_shares};
use knot_export::{Align, Block, Table, walk};
use yrs::Any;

fn approx(a: &[f64], b: &[f64]) {
    assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
    for (x, y) in a.iter().zip(b) {
        assert!((x - y).abs() < 1e-9, "{a:?} vs {b:?}");
    }
}

fn widths(ws: &[Option<f64>]) -> Any {
    Any::Array(
        ws.iter()
            .map(|w| w.map_or(Any::Null, Any::Number))
            .collect::<Vec<_>>()
            .into(),
    )
}

fn cell(tag: &'static str, attrs: Vec<(&'static str, Any)>, text: &str) -> N {
    el(tag, attrs, vec![p(text)])
}

fn td(text: &str) -> N {
    cell("table_cell", vec![], text)
}

fn th(text: &str) -> N {
    cell("table_header", vec![], text)
}

fn row(cells: Vec<N>) -> N {
    el("table_row", vec![], cells)
}

fn table_of(rows: Vec<N>) -> Table {
    let w = walk(&build(vec![el("table", vec![], rows)]));
    assert!(w.unknown_elements.is_empty(), "{:?}", w.unknown_elements);
    match w.doc.blocks.into_iter().next() {
        Some(Block::Table(t)) => t,
        other => panic!("not a table: {other:?}"),
    }
}

#[test]
fn shares_all_unset_are_equal() {
    assert_eq!(resolve_shares(&[None; 4]), vec![0.25; 4]);
}

#[test]
fn shares_normalise_known() {
    assert_eq!(resolve_shares(&[Some(200.), Some(600.)]), vec![0.25, 0.75]);
}

#[test]
fn shares_fill_unset_from_leftover() {
    approx(
        &resolve_shares(&[Some(400.), None, Some(600.)]),
        &[400. / 1500., 500. / 1500., 600. / 1500.],
    );
    approx(
        &resolve_shares(&[Some(300.), None, None, None]),
        &[0.3, 0.7 / 3., 0.7 / 3., 0.7 / 3.],
    );
    approx(
        &resolve_shares(&[Some(500.), Some(250.), None]),
        &[0.5, 0.25, 0.25],
    );
}

#[test]
fn shares_ignore_non_positive() {
    assert_eq!(
        resolve_shares(&[Some(0.), Some(300.), Some(-5.), Some(f64::NAN)]),
        resolve_shares(&[None, Some(300.), None, None])
    );
}

#[test]
fn shares_empty() {
    assert!(resolve_shares(&[]).is_empty());
}

#[test]
fn grid_places_cells_after_rowspan() {
    let t = table_of(vec![
        row(vec![
            cell("table_cell", vec![("rowspan", Any::Number(2.))], "A"),
            td("B"),
        ]),
        row(vec![td("C")]),
    ]);
    assert_eq!(t.shares.len(), 2);
    assert_eq!(t.rows[0][0].col, 0);
    assert_eq!(t.rows[0][0].rowspan, 2);
    assert_eq!(t.rows[0][1].col, 1);
    assert_eq!(t.rows[1][0].col, 1);
    assert_eq!(t.rows[1][0].colspan, 1);
}

#[test]
fn colspan_weights_come_from_first_row() {
    let t = table_of(vec![
        row(vec![cell(
            "table_cell",
            vec![
                ("colspan", Any::Number(2.)),
                ("colwidth", widths(&[Some(300.), Some(700.)])),
            ],
            "A",
        )]),
        row(vec![td("B"), td("C")]),
    ]);
    approx(&t.shares, &[0.3, 0.7]);
}

#[test]
fn string_spans_and_null_widths_are_read() {
    let t = table_of(vec![row(vec![
        cell(
            "table_cell",
            vec![
                ("colspan", "2".into()),
                ("colwidth", widths(&[None, Some(500.)])),
            ],
            "A",
        ),
        td("B"),
    ])]);
    assert_eq!(t.rows[0][0].colspan, 2);
    assert_eq!(t.rows[0][1].col, 2);
    // Column 0 and 2 are unset and share the 500 left of 1000.
    approx(&t.shares, &[0.25, 0.5, 0.25]);
}

#[test]
fn header_rows_counts_leading_all_header_rows() {
    let t = table_of(vec![
        row(vec![th("a"), th("b")]),
        row(vec![th("c"), th("d")]),
        row(vec![td("e"), td("f")]),
    ]);
    assert_eq!(header_rows(&t), 2);
    let t = table_of(vec![
        row(vec![th("a"), td("b")]),
        row(vec![th("c"), td("d")]),
    ]);
    assert_eq!(header_rows(&t), 0);
    assert!(t.rows[0][0].header && !t.rows[0][1].header);
}

#[test]
fn cell_blocks_are_kept() {
    let t = table_of(vec![row(vec![el(
        "table_cell",
        vec![],
        vec![
            el(
                "bullet_list",
                vec![],
                vec![el("list_item", vec![], vec![p("x")])],
            ),
            el(
                "code_block",
                vec![("language", "rs".into())],
                vec![t("fn main() {}")],
            ),
            p("one"),
            p("two"),
        ],
    )])]);
    let blocks = &t.rows[0][0].blocks;
    assert_eq!(blocks.len(), 4);
    assert!(matches!(blocks[0], Block::List(_)));
    assert!(matches!(blocks[1], Block::Code { .. }));
    assert!(matches!(blocks[2], Block::Paragraph(_)));
    assert!(matches!(blocks[3], Block::Paragraph(_)));
}

#[test]
fn empty_table_walks_without_panic() {
    let t = table_of(vec![]);
    assert!(t.shares.is_empty() && t.rows.is_empty());
    let t = table_of(vec![row(vec![])]);
    assert!(t.shares.is_empty());
    assert_eq!(t.rows.len(), 1);
    assert_eq!(header_rows(&t), 0);
}

#[test]
fn align_attribute_is_read() {
    let t = table_of(vec![row(vec![
        cell("table_cell", vec![("align", "center".into())], "a"),
        cell("table_cell", vec![("align", "right".into())], "b"),
        td("c"),
    ])]);
    assert_eq!(t.rows[0][0].align, Align::Center);
    assert_eq!(t.rows[0][1].align, Align::Right);
    assert_eq!(t.rows[0][2].align, Align::Left);
}
