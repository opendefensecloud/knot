//! Walks the canonical "default" XmlFragment of a Y.Doc into an [`ExportDoc`].
//!
//! The walk never panics and never fails: anything it does not understand
//! becomes a plain paragraph and is reported in [`Walked::unknown_elements`].

use sha2::{Digest, Sha256};
use url::Url;
use yrs::types::text::YChange;
use yrs::{Any, Out, ReadTxn, Text, Transact, Xml, XmlFragment, XmlOut, XmlTextRef};

use crate::model::{
    Align, Block, Cell, ExportDoc, ImageSource, Inline, LinkTarget, List, ListItem, Marks, Run,
    Table,
};
use crate::tables::resolve_shares;

/// Maximum nesting of block containers (lists, list items, quotes, tables,
/// cells, unknown elements). Deeper subtrees are flattened to paragraphs so a
/// crafted page cannot overflow the stack.
pub const MAX_DEPTH: usize = 64;

/// Widest table (in grid columns) that stays a table.
pub const MAX_TABLE_COLUMNS: usize = 64;

/// Largest table (grid width times row count) that stays a table. Writers
/// emit output per grid position, so this bounds their output.
pub const MAX_TABLE_GRID_CELLS: usize = 50_000;

/// Grid positions all the tables of a page may have together. A table past
/// it degrades like one over [`MAX_TABLE_GRID_CELLS`]. Writers emit output
/// per grid position (a covered or merge-continuation cell even in an empty
/// row) and per column, so each table counts as
/// `width * (rows + TABLE_COLUMN_ROWS)`. That keeps what a page of many
/// tables can make a writer emit to about 16 MB of XML.
pub const MAX_PAGE_GRID_CELLS: usize = 200_000;

/// Rows a table's columns count for against [`MAX_PAGE_GRID_CELLS`], on top
/// of its own: an ODT column's style and element weigh about as much as
/// eight covered cells.
pub const TABLE_COLUMN_ROWS: usize = 8;

/// Largest `colspan` or `rowspan`; bigger values are clamped.
pub const MAX_SPAN: usize = 64;

/// Result of walking a document.
#[derive(Debug, Clone, PartialEq)]
pub struct Walked {
    pub doc: ExportDoc,
    /// Tags of block elements the walker did not recognise, in document order.
    pub unknown_elements: Vec<String>,
}

/// What a walk keeps track of across the whole page.
#[derive(Default)]
struct Tally {
    /// See [`Walked::unknown_elements`].
    unknown: Vec<String>,
    /// What the tables kept so far count for against
    /// [`MAX_PAGE_GRID_CELLS`].
    grid_cells: usize,
}

pub fn walk(doc: &yrs::Doc) -> Walked {
    let txn = doc.transact();
    let mut tally = Tally::default();
    let blocks = match txn.get_xml_fragment("default") {
        Some(frag) => walk_blocks(&frag, &txn, &mut tally, 0),
        None => Vec::new(),
    };
    Walked {
        doc: ExportDoc { blocks },
        unknown_elements: tally.unknown,
    }
}

/// Read an attribute as a string. Handles the editor's numbers and the
/// import's strings alike.
fn attr<X: Xml, T: ReadTxn>(el: &X, txn: &T, name: &str) -> Option<String> {
    el.get_attribute(txn, name).map(|v| v.to_string(txn))
}

/// Walk the children of a block container. Raw text or `hard_break` children
/// (the import's soft-break shape) are repaired into paragraphs; consecutive
/// ones share a paragraph.
fn walk_blocks<F: XmlFragment, T: ReadTxn>(
    parent: &F,
    txn: &T,
    tally: &mut Tally,
    depth: usize,
) -> Vec<Block> {
    if depth > MAX_DEPTH {
        tally.unknown.push("depth-limit".to_owned());
        return capped_paragraphs(parent, txn);
    }
    let mut out = Vec::new();
    let mut stray: Vec<Inline> = Vec::new();
    let mut last_href: Option<String> = None;
    for i in 0..parent.len(txn) {
        match parent.get(txn, i) {
            Some(XmlOut::Text(t)) => push_text(&mut stray, &mut last_href, &t, txn),
            Some(XmlOut::Element(el)) if el.tag().as_ref() == "hard_break" => {
                last_href = None;
                stray.push(Inline::Break);
            }
            Some(XmlOut::Element(el)) => {
                flush_stray(&mut stray, &mut out);
                last_href = None;
                out.extend(walk_element(&el, txn, tally, depth));
            }
            Some(XmlOut::Fragment(f)) => {
                flush_stray(&mut stray, &mut out);
                last_href = None;
                out.extend(walk_blocks(&f, txn, tally, depth + 1));
            }
            None => {}
        }
    }
    flush_stray(&mut stray, &mut out);
    out
}

fn flush_stray(stray: &mut Vec<Inline>, out: &mut Vec<Block>) {
    if !stray.is_empty() {
        out.push(Block::Paragraph(std::mem::take(stray)));
    }
}

fn walk_list<T: ReadTxn>(
    el: &yrs::XmlElementRef,
    ordered: bool,
    txn: &T,
    tally: &mut Tally,
    depth: usize,
) -> Block {
    let start = if ordered {
        attr(el, txn, "start")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|n| n.is_finite())
            .map(|n| n.round().clamp(1.0, f64::from(u32::MAX)) as u32)
            .unwrap_or(1)
    } else {
        1
    };
    let mut items = Vec::new();
    for i in 0..el.len(txn) {
        match el.get(txn, i) {
            Some(XmlOut::Element(item)) if item.tag().as_ref() == "list_item" => {
                items.push(ListItem {
                    checked: attr(&item, txn, "checked").map(|v| v == "true"),
                    blocks: walk_blocks(&item, txn, tally, depth + 2),
                });
            }
            Some(XmlOut::Element(other)) => {
                // Not a list_item: keep it as an item of its own.
                items.push(ListItem {
                    checked: None,
                    blocks: walk_element(&other, txn, tally, depth + 1),
                });
            }
            _ => {}
        }
    }
    Block::List(List {
        ordered,
        start,
        items,
    })
}

/// A span attribute (`colspan`/`rowspan`): editor number or import string.
/// Anything missing, non-numeric or below 1 counts as 1.
fn span<T: ReadTxn>(el: &yrs::XmlElementRef, txn: &T, name: &str) -> usize {
    attr(el, txn, name)
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite())
        .map(|n| n.round().clamp(1.0, MAX_SPAN as f64) as usize)
        .unwrap_or(1)
}

/// The `colwidth` attribute: an array of numbers and nulls.
fn colwidth<T: ReadTxn>(el: &yrs::XmlElementRef, txn: &T) -> Vec<Option<f64>> {
    match el.get_attribute(txn, "colwidth") {
        Some(Out::Any(Any::Array(items))) => items
            .iter()
            .map(|a| match a {
                Any::Number(n) => Some(*n),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn walk_table<T: ReadTxn>(
    el: &yrs::XmlElementRef,
    txn: &T,
    tally: &mut Tally,
    depth: usize,
) -> Vec<Block> {
    // Rows still occupied below the current one, per grid column.
    let mut carry: Vec<usize> = Vec::new();
    let mut width = 0;
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut weights_by_col: Vec<Option<f64>> = Vec::new();
    for i in 0..el.len(txn) {
        let Some(XmlOut::Element(row)) = el.get(txn, i) else {
            continue;
        };
        let blocked: Vec<bool> = carry.iter().map(|&n| n > 0).collect();
        for n in carry.iter_mut() {
            *n = n.saturating_sub(1);
        }
        let first_row = rows.is_empty();
        let mut col = 0;
        let mut cells = Vec::new();
        for j in 0..row.len(txn) {
            let Some(XmlOut::Element(c)) = row.get(txn, j) else {
                continue;
            };
            while blocked.get(col).copied().unwrap_or(false) {
                col += 1;
            }
            let colspan = span(&c, txn, "colspan");
            let rowspan = span(&c, txn, "rowspan");
            if carry.len() < col + colspan {
                carry.resize(col + colspan, 0);
            }
            for n in &mut carry[col..col + colspan] {
                *n = (*n).max(rowspan - 1);
            }
            if first_row {
                let cw = colwidth(&c, txn);
                weights_by_col.resize(col + colspan, None);
                for k in 0..colspan {
                    weights_by_col[col + k] = cw.get(k).copied().flatten();
                }
            }
            let align = match attr(&c, txn, "align").as_deref() {
                Some("center") => Align::Center,
                Some("right") => Align::Right,
                _ => Align::Left,
            };
            cells.push(Cell {
                col,
                colspan,
                rowspan,
                header: c.tag().as_ref() == "table_header",
                align,
                blocks: walk_blocks(&c, txn, tally, depth + 2),
            });
            col += colspan;
        }
        width = width.max(col).max(carry.len());
        rows.push(cells);
    }
    let grid_cells = width.saturating_mul(rows.len());
    let page_cells = tally
        .grid_cells
        .saturating_add(width.saturating_mul(rows.len().saturating_add(TABLE_COLUMN_ROWS)));
    if width > MAX_TABLE_COLUMNS
        || grid_cells > MAX_TABLE_GRID_CELLS
        || page_cells > MAX_PAGE_GRID_CELLS
    {
        // Too big to keep as a table: keep the text, one paragraph per row.
        tally.unknown.push("table-limit".to_owned());
        let paragraphs = rows
            .iter()
            .map(|cells| {
                let texts: Vec<String> = cells.iter().map(|c| plain_text(&c.blocks)).collect();
                Block::Paragraph(vec![Inline::Run(Run {
                    text: texts.join(" | "),
                    marks: Marks::default(),
                })])
            })
            .collect();
        return paragraphs;
    }
    tally.grid_cells = page_cells;
    weights_by_col.resize(width, None);
    vec![Block::Table(Table {
        shares: resolve_shares(&weights_by_col),
        rows,
    })]
}

/// The text of `blocks`, one space between blocks and breaks.
fn plain_text(blocks: &[Block]) -> String {
    fn runs(out: &mut Vec<String>, content: &[Inline]) {
        let mut s = String::new();
        for inline in content {
            match inline {
                Inline::Run(r) => s.push_str(&r.text),
                Inline::Link { runs, .. } => runs.iter().for_each(|r| s.push_str(&r.text)),
                Inline::Break => s.push(' '),
            }
        }
        out.push(s);
    }
    let mut parts = Vec::new();
    visit_blocks(blocks, &mut |b| match b {
        Block::Paragraph(content) | Block::Heading { content, .. } => runs(&mut parts, content),
        Block::Code { text, .. } => parts.push(text.clone()),
        Block::Mermaid { source, .. } => parts.push(source.clone()),
        _ => {}
    });
    parts.retain(|p| !p.is_empty());
    parts.join(" ")
}

/// One element as blocks: a table over the size limits becomes several
/// paragraphs, everything else exactly one block.
fn walk_element<T: ReadTxn>(
    el: &yrs::XmlElementRef,
    txn: &T,
    tally: &mut Tally,
    depth: usize,
) -> Vec<Block> {
    if depth <= MAX_DEPTH && el.tag().as_ref() == "table" {
        return walk_table(el, txn, tally, depth + 1);
    }
    vec![walk_single(el, txn, tally, depth)]
}

fn walk_single<T: ReadTxn>(
    el: &yrs::XmlElementRef,
    txn: &T,
    tally: &mut Tally,
    depth: usize,
) -> Block {
    if depth > MAX_DEPTH {
        // Reached through list/element recursion that never passes through
        // walk_blocks or inlines; cap here too.
        tally.unknown.push("depth-limit".to_owned());
        return Block::Paragraph(vec![Inline::Run(Run {
            text: flatten_text(el, txn),
            marks: Marks::default(),
        })]);
    }
    let tag = el.tag().to_string();
    match tag.as_str() {
        "paragraph" => Block::Paragraph(inlines(el, txn, depth)),
        "heading" => {
            let level = attr(el, txn, "level")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|n| n.is_finite())
                .map(|n| n.round().clamp(1.0, 6.0) as u8)
                .unwrap_or(1);
            Block::Heading {
                level,
                content: inlines(el, txn, depth),
            }
        }
        "blockquote" => Block::Quote(walk_blocks(el, txn, tally, depth + 1)),
        "code_block" => {
            let language = attr(el, txn, "language").unwrap_or_default();
            let text = flatten_text(el, txn);
            if language.trim().eq_ignore_ascii_case("mermaid") {
                let key = format!("mermaid-{:x}", Sha256::digest(text.as_bytes()));
                Block::Mermaid { key, source: text }
            } else {
                Block::Code { language, text }
            }
        }
        "horizontal_rule" => Block::Rule,
        "bullet_list" => walk_list(el, false, txn, tally, depth + 1),
        "ordered_list" => walk_list(el, true, txn, tally, depth + 1),
        "image" => {
            let src = attr(el, txn, "src").unwrap_or_default();
            Block::Image {
                source: classify_image(&src),
                alt: attr(el, txn, "alt").unwrap_or_default(),
            }
        }
        "excalidraw_board" => {
            let board_id = attr(el, txn, "board_id").unwrap_or_default();
            let label = attr(el, txn, "label")
                .filter(|l| !l.trim().is_empty())
                .unwrap_or_else(|| "Diagram".to_owned());
            Block::Board {
                key: format!("board-{board_id}"),
                board_id,
                label,
            }
        }
        "attachment" => Block::Attachment {
            url: attr(el, txn, "url").unwrap_or_default(),
            name: attr(el, txn, "name").unwrap_or_default(),
            size: attr(el, txn, "size")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|n| n.round() as u64),
        },
        _ => {
            tally.unknown.push(tag);
            Block::Paragraph(vec![Inline::Run(Run {
                text: flatten_text(el, txn),
                marks: Marks::default(),
            })])
        }
    }
}

/// Inline content of a textblock. Unknown inline elements keep their text.
fn inlines<F: XmlFragment, T: ReadTxn>(parent: &F, txn: &T, depth: usize) -> Vec<Inline> {
    if depth > MAX_DEPTH {
        return vec![Inline::Run(Run {
            text: flatten_text(parent, txn),
            marks: Marks::default(),
        })];
    }
    let mut out = Vec::new();
    let mut last_href: Option<String> = None;
    for i in 0..parent.len(txn) {
        match parent.get(txn, i) {
            Some(XmlOut::Text(t)) => push_text(&mut out, &mut last_href, &t, txn),
            Some(XmlOut::Element(el)) => {
                last_href = None;
                if el.tag().as_ref() == "hard_break" {
                    out.push(Inline::Break);
                } else {
                    let text = flatten_text(&el, txn);
                    if !text.is_empty() {
                        out.push(Inline::Run(Run {
                            text,
                            marks: Marks::default(),
                        }));
                    }
                }
            }
            Some(XmlOut::Fragment(f)) => {
                last_href = None;
                out.extend(inlines(&f, txn, depth + 1));
            }
            None => {}
        }
    }
    out
}

fn push_text<T: ReadTxn>(
    out: &mut Vec<Inline>,
    last_href: &mut Option<String>,
    t: &XmlTextRef,
    txn: &T,
) {
    for d in t.diff(txn, YChange::identity) {
        let text = match d.insert {
            Out::Any(Any::String(s)) => s.to_string(),
            _ => continue,
        };
        let attrs = d.attributes.as_deref();
        let has = |k: &str| attrs.is_some_and(|m| m.contains_key(k));
        let run = Run {
            text,
            marks: Marks {
                bold: has("bold"),
                italic: has("italic"),
                underline: has("underline"),
                strike: has("strike"),
                code: has("code"),
            },
        };
        let href = attrs.and_then(|m| match m.get("link") {
            Some(Any::Map(link)) => match link.get("href") {
                Some(Any::String(s)) => Some(s.to_string()),
                _ => None,
            },
            _ => None,
        });
        match href {
            None => {
                *last_href = None;
                out.push(Inline::Run(run));
            }
            Some(h) => {
                if last_href.as_deref() == Some(h.as_str())
                    && let Some(Inline::Link { runs, .. }) = out.last_mut()
                {
                    runs.push(run);
                } else {
                    out.push(Inline::Link {
                        target: classify(&h),
                        runs: vec![run],
                    });
                    *last_href = Some(h);
                }
            }
        }
    }
}

/// A UUID in any accepted spelling (hyphenated, simple, braced, urn) as
/// lower-case hyphenated.
fn canonical_id(s: &str) -> Option<String> {
    uuid::Uuid::parse_str(s)
        .ok()
        .map(|u| u.hyphenated().to_string())
}

fn classify_image(src: &str) -> ImageSource {
    if let Some(id) = src.strip_prefix("/api/blobs/")
        && let Some(id) = canonical_id(id)
    {
        return ImageSource::Blob(id);
    }
    match Url::parse(src) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => ImageSource::External(src.to_owned()),
        _ => ImageSource::Unsupported,
    }
}

fn classify(href: &str) -> LinkTarget {
    if let Some(rest) = href.strip_prefix("knot://doc/") {
        return canonical_id(rest).map_or(LinkTarget::Plain, LinkTarget::Doc);
    }
    if let Some(rest) = href.strip_prefix("knot://user/") {
        return canonical_id(rest).map_or(LinkTarget::Plain, LinkTarget::User);
    }
    match Url::parse(href) {
        Ok(u) if matches!(u.scheme(), "http" | "https" | "mailto") => {
            LinkTarget::Web(u.as_str().to_owned())
        }
        _ => LinkTarget::Plain,
    }
}

/// Text of one `XmlText` node, mark-free.
fn text_of<T: ReadTxn>(t: &XmlTextRef, txn: &T) -> String {
    let mut s = String::new();
    for d in t.diff(txn, YChange::identity) {
        if let Out::Any(Any::String(x)) = d.insert {
            s.push_str(&x);
        }
    }
    s
}

/// Text nodes below `node` in document order. Iterative (explicit stack), so
/// it is safe at any depth.
fn text_nodes<F: XmlFragment, T: ReadTxn>(node: &F, txn: &T) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<XmlOut> = (0..node.len(txn))
        .rev()
        .filter_map(|i| node.get(txn, i))
        .collect();
    while let Some(n) = stack.pop() {
        match n {
            XmlOut::Text(t) => out.push(text_of(&t, txn)),
            XmlOut::Element(e) => {
                stack.extend((0..e.len(txn)).rev().filter_map(|i| e.get(txn, i)));
            }
            XmlOut::Fragment(f) => {
                stack.extend((0..f.len(txn)).rev().filter_map(|i| f.get(txn, i)));
            }
        }
    }
    out
}

/// All plain text below `node`, concatenated (mark-free).
fn flatten_text<F: XmlFragment, T: ReadTxn>(node: &F, txn: &T) -> String {
    text_nodes(node, txn).concat()
}

/// A too-deep subtree as one paragraph per non-empty text node.
fn capped_paragraphs<F: XmlFragment, T: ReadTxn>(node: &F, txn: &T) -> Vec<Block> {
    text_nodes(node, txn)
        .into_iter()
        .filter(|t| !t.is_empty())
        .map(|text| {
            Block::Paragraph(vec![Inline::Run(Run {
                text,
                marks: Marks::default(),
            })])
        })
        .collect()
}

/// A diagram the server has to render before writing the document.
#[derive(Debug, Clone, PartialEq)]
pub enum DiagramRef {
    Mermaid {
        key: String,
        source: String,
    },
    Board {
        key: String,
        board_id: String,
        label: String,
    },
}

/// Calls `f` for every block, depth first in document order.
fn visit_blocks<'a>(blocks: &'a [Block], f: &mut impl FnMut(&'a Block)) {
    for b in blocks {
        f(b);
        match b {
            Block::Quote(inner) => visit_blocks(inner, f),
            Block::List(list) => {
                for item in &list.items {
                    visit_blocks(&item.blocks, f);
                }
            }
            Block::Table(table) => {
                for cell in table.rows.iter().flatten() {
                    visit_blocks(&cell.blocks, f);
                }
            }
            _ => {}
        }
    }
}

fn push_unique(out: &mut Vec<String>, id: &str) {
    if !out.iter().any(|x| x == id) {
        out.push(id.to_owned());
    }
}

/// Mermaid blocks and boards in document order, once per key.
pub fn diagrams(doc: &ExportDoc) -> Vec<DiagramRef> {
    let mut out: Vec<DiagramRef> = Vec::new();
    visit_blocks(&doc.blocks, &mut |b| {
        let (r, key) = match b {
            Block::Mermaid { key, source } => (
                DiagramRef::Mermaid {
                    key: key.clone(),
                    source: source.clone(),
                },
                key,
            ),
            Block::Board {
                key,
                board_id,
                label,
            } => (
                DiagramRef::Board {
                    key: key.clone(),
                    board_id: board_id.clone(),
                    label: label.clone(),
                },
                key,
            ),
            _ => return,
        };
        let seen = out.iter().any(|d| match d {
            DiagramRef::Mermaid { key: k, .. } | DiagramRef::Board { key: k, .. } => k == key,
        });
        if !seen {
            out.push(r);
        }
    });
    out
}

/// Blob ids of all images stored in knot, in document order, once each.
pub fn blob_ids(doc: &ExportDoc) -> Vec<String> {
    let mut out = Vec::new();
    visit_blocks(&doc.blocks, &mut |b| {
        if let Block::Image {
            source: ImageSource::Blob(id),
            ..
        } = b
        {
            push_unique(&mut out, id);
        }
    });
    out
}

/// Ids of all mentioned users, in document order, once each.
pub fn user_ids(doc: &ExportDoc) -> Vec<String> {
    let mut out = Vec::new();
    visit_blocks(&doc.blocks, &mut |b| {
        let inlines = match b {
            Block::Paragraph(i) | Block::Heading { content: i, .. } => i,
            _ => return,
        };
        for i in inlines {
            if let Inline::Link {
                target: LinkTarget::User(id),
                ..
            } = i
            {
                push_unique(&mut out, id);
            }
        }
    });
    out
}
