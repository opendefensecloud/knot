mod common;

use common::*;
use knot_export::tables::{Slot, grid_edges, header_rows, layout, resolve_shares};
use knot_export::{Align, Block, Cell, Table, walk};
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

fn texts(w: &knot_export::Walked) -> Vec<String> {
    w.doc
        .blocks
        .iter()
        .map(|b| match b {
            Block::Paragraph(inl) => match inl.as_slice() {
                [knot_export::Inline::Run(r)] => r.text.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("not a paragraph: {other:?}"),
        })
        .collect()
}

#[test]
fn spans_are_clamped_to_max_span() {
    let attrs = vec![
        ("colspan", Any::Number(1000.)),
        ("rowspan", Any::Number(1000.)),
    ];
    let t = table_of(vec![row(vec![cell("table_cell", attrs, "A")])]);
    assert_eq!(t.rows[0][0].colspan, knot_export::MAX_SPAN);
    assert_eq!(t.rows[0][0].rowspan, knot_export::MAX_SPAN);
    assert_eq!(t.shares.len(), 64);
}

#[test]
fn too_wide_table_degrades_to_paragraphs() {
    let names: Vec<String> = (0..=knot_export::MAX_TABLE_COLUMNS)
        .map(|i| format!("c{i}"))
        .collect();
    let w = walk(&build(vec![
        el(
            "table",
            vec![],
            vec![
                row(names.iter().map(|n| td(n)).collect()),
                row(vec![td("second")]),
            ],
        ),
        p("after"),
    ]));
    assert_eq!(w.unknown_elements, ["table-limit"]);
    assert_eq!(
        texts(&w),
        [names.join(" | "), "second".into(), "after".into()]
    );
}

#[test]
fn too_many_grid_cells_degrade() {
    let table = |n: usize| {
        let rows = (0..n)
            .map(|r| row((0..10).map(|c| td(&format!("{r}.{c}"))).collect()))
            .collect();
        walk(&build(vec![el("table", vec![], rows)]))
    };
    let ok = table(5_000);
    assert!(ok.unknown_elements.is_empty());
    assert!(matches!(ok.doc.blocks.as_slice(), [Block::Table(_)]));

    let over = table(5_001);
    assert_eq!(over.unknown_elements, ["table-limit"]);
    assert_eq!(over.doc.blocks.len(), 5_001);
    assert_eq!(
        texts(&over)[0],
        "0.0 | 0.1 | 0.2 | 0.3 | 0.4 | 0.5 | 0.6 | 0.7 | 0.8 | 0.9"
    );
}

#[test]
fn tables_past_the_page_budget_degrade() {
    use knot_export::{MAX_PAGE_GRID_CELLS, MAX_TABLE_GRID_CELLS, TABLE_COLUMN_ROWS};
    // 64 x 773 grid positions each, inside the per-table limits; with
    // their columns each counts 64 x 781 = 49,984, and four of them fill
    // the page budget.
    let rows = MAX_TABLE_GRID_CELLS / 64 - TABLE_COLUMN_ROWS;
    let per_table = 64 * (rows + TABLE_COLUMN_ROWS);
    assert!(4 * per_table <= MAX_PAGE_GRID_CELLS && 5 * per_table > MAX_PAGE_GRID_CELLS);
    let mut blocks: Vec<N> = (0..5).map(|_| wide_empty_table(64, rows)).collect();
    // The budget counts kept tables only: a small table after the
    // degraded one still fits.
    blocks.push(el("table", vec![], vec![row(vec![td("a"), td("b")])]));
    let w = walk(&build(blocks));

    assert_eq!(w.unknown_elements, ["table-limit"]);
    let kept: Vec<usize> = w
        .doc
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| matches!(b, Block::Table(_)).then_some(i))
        .collect();
    // Four tables, the fifth as one paragraph per row, then the small one.
    assert_eq!(kept, [0, 1, 2, 3, 4 + rows]);
    assert_eq!(w.doc.blocks.len(), 5 + rows);
    assert!(matches!(&w.doc.blocks[4], Block::Paragraph(_)));
}

fn grid_cell(col: usize, colspan: usize, rowspan: usize) -> Cell {
    Cell {
        col,
        colspan,
        rowspan,
        header: false,
        align: Align::Left,
        blocks: vec![],
    }
}

fn grid(width: usize, rows: Vec<Vec<Cell>>) -> Table {
    Table {
        shares: vec![1.0 / width as f64; width],
        rows,
    }
}

/// `t` laid out with `group_end`, each slot as a short string. Also checks
/// that every row's slots tile the grid exactly, left to right.
fn shape(t: &Table, group_end: impl Fn(usize) -> usize) -> Vec<Vec<String>> {
    layout(t, group_end)
        .map(|slots| {
            let mut at = 0;
            let row = slots
                .iter()
                .map(|s| {
                    let (pos, end, text) = match *s {
                        Slot::Cell {
                            pos, span, rowspan, ..
                        } => (pos, pos + span, format!("cell {pos}+{span} r{rowspan}")),
                        Slot::Covered { pos, span, header } => {
                            let h = if header { " h" } else { "" };
                            (pos, pos + span, format!("covered {pos}+{span}{h}"))
                        }
                        Slot::Gap { pos, end } => (pos, end, format!("gap {pos}..{end}")),
                    };
                    assert!(pos == at && end > pos, "{text} doesn't continue at {at}");
                    at = end;
                    text
                })
                .collect();
            assert_eq!(at, t.shares.len(), "the row fills the grid");
            row
        })
        .collect()
}

#[test]
fn layout_covers_row_spans_and_places_cells_after_them() {
    let t = table_of(vec![
        row(vec![
            cell("table_cell", vec![("rowspan", Any::Number(2.))], "A"),
            td("B"),
        ]),
        row(vec![td("C")]),
        row(vec![td("D"), td("E")]),
    ]);
    assert_eq!(
        shape(&t, |_| t.rows.len()),
        [
            vec!["cell 0+1 r2", "cell 1+1 r1"],
            vec!["covered 0+1", "cell 1+1 r1"],
            vec!["cell 0+1 r1", "cell 1+1 r1"],
        ]
    );
    // Each cell slot carries the cell it places.
    let rows: Vec<_> = layout(&t, |_| t.rows.len()).collect();
    match rows[1][1] {
        Slot::Cell { cell, .. } => assert!(std::ptr::eq(cell, &t.rows[1][0])),
        other => panic!("not a cell: {other:?}"),
    }
}

#[test]
fn layout_clamps_column_spans_to_the_grid_and_short_of_row_spans() {
    // A column span past the grid ends at its edge.
    let t = grid(2, vec![vec![grid_cell(0, 1, 1), grid_cell(1, 4, 1)]]);
    assert_eq!(shape(&t, |_| 1), [vec!["cell 0+1 r1", "cell 1+1 r1"]]);

    // A column span that runs into a row span from above stops short of it.
    let t = grid(
        3,
        vec![
            vec![grid_cell(0, 1, 1), grid_cell(1, 1, 2), grid_cell(2, 1, 1)],
            vec![grid_cell(0, 3, 1)],
        ],
    );
    assert_eq!(
        shape(&t, |_| 2),
        [
            vec!["cell 0+1 r1", "cell 1+1 r2", "cell 2+1 r1"],
            vec!["cell 0+1 r1", "covered 1+1", "gap 2..3"],
        ]
    );
}

#[test]
fn layout_clamps_row_spans_to_the_group_end() {
    let rows = || vec![vec![grid_cell(0, 1, 9)], vec![], vec![]];
    // Clamped to the end of the table.
    let t = grid(1, rows());
    assert_eq!(
        shape(&t, |_| 3),
        [
            vec!["cell 0+1 r3"],
            vec!["covered 0+1"],
            vec!["covered 0+1"]
        ]
    );
    // Clamped to a group of its first row only: nothing is covered below.
    assert_eq!(
        shape(&t, |r| if r < 1 { 1 } else { 3 }),
        [vec!["cell 0+1 r1"], vec!["gap 0..1"], vec!["gap 0..1"]]
    );
    // A group end at or before the row (a caller bug) still lays out, with
    // every row span at 1, instead of panicking.
    assert_eq!(
        shape(&t, |_| 0),
        [vec!["cell 0+1 r1"], vec!["gap 0..1"], vec!["gap 0..1"]]
    );
}

#[test]
fn layout_ends_a_row_span_after_its_rows() {
    let t = grid(2, vec![vec![grid_cell(0, 2, 2)], vec![], vec![]]);
    assert_eq!(
        shape(&t, |_| 3),
        [vec!["cell 0+2 r2"], vec!["covered 0+2"], vec!["gap 0..2"]]
    );
}

#[test]
fn layout_fills_each_gap_with_one_slot() {
    let t = grid(
        4,
        vec![
            vec![grid_cell(2, 1, 1)],
            vec![],
            // A cell past the grid has nowhere to go.
            vec![grid_cell(9, 1, 1)],
            vec![grid_cell(0, 1, 1), grid_cell(2, 1, 2)],
            vec![],
        ],
    );
    assert_eq!(
        shape(&t, |_| 5),
        [
            vec!["gap 0..2", "cell 2+1 r1", "gap 3..4"],
            vec!["gap 0..4"],
            vec!["gap 0..4"],
            vec!["cell 0+1 r1", "gap 1..2", "cell 2+1 r2", "gap 3..4"],
            vec!["gap 0..2", "covered 2+1", "gap 3..4"],
        ]
    );
}

#[test]
fn layout_moves_an_overlapped_cell_to_the_next_free_column() {
    let t = grid(3, vec![vec![grid_cell(0, 2, 1), grid_cell(1, 1, 1)]]);
    assert_eq!(shape(&t, |_| 1), [vec!["cell 0+2 r1", "cell 2+1 r1"]]);
}

#[test]
fn layout_marks_rows_covered_by_a_header_cell() {
    let mut head = grid_cell(0, 1, 2);
    head.header = true;
    let t = grid(2, vec![vec![head, grid_cell(1, 1, 1)], vec![]]);
    assert_eq!(
        shape(&t, |_| 2),
        [
            vec!["cell 0+1 r2", "cell 1+1 r1"],
            vec!["covered 0+1 h", "gap 1..2"],
        ]
    );
}

#[test]
fn layout_of_an_empty_grid_has_empty_rows() {
    let t = grid(0, vec![vec![grid_cell(0, 1, 1)], vec![]]);
    assert_eq!(shape(&t, |_| 2), [Vec::<String>::new(), vec![]]);
    assert_eq!(layout(&grid(3, vec![]), |_| 0).count(), 0);
}

#[test]
fn grid_edges_add_up_to_the_total() {
    assert_eq!(grid_edges(&[0.3, 0.7], 9072), [0, 2722, 9072]);
    // Each column's width is rounded; the last one takes what is left.
    assert_eq!(grid_edges(&[1.0 / 3.0; 3], 1000), [0, 333, 666, 1000]);
    assert_eq!(grid_edges(&[1.0], 500), [0, 500]);
    assert_eq!(grid_edges(&[], 500), [0]);
    // Shares that round up past the total never push an edge beyond it.
    assert_eq!(grid_edges(&[0.6, 0.6, 0.6], 10), [0, 6, 10, 10]);
}
