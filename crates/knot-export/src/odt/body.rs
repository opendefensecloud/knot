//! `office:text`: the title block, the blocks and the lists, and the text
//! encoder every piece of text goes through.
//!
//! Block emitters take `avail_cm`, the width their content may use. It
//! starts at the text width and shrinks inside quotes, list items and table
//! cells, so tables and pictures can size themselves to it.

use crate::model::{Block, ExportDoc, Inline, LinkTarget, List, ListItem, Marks, Run};
use crate::xml::{esc_attr, esc_text};

use super::{Ctx, LIST_INDENT_CM, LIST_LEVELS, QUOTE_INDENT_CM, TEXT_WIDTH_CM, picture, table};

const HEADINGS: [&str; 6] = [
    "Heading_20_1",
    "Heading_20_2",
    "Heading_20_3",
    "Heading_20_4",
    "Heading_20_5",
    "Heading_20_6",
];

/// A `text:list` or list item that is open: its element and where it is in
/// `Ctx::body`.
struct OpenElement {
    tag: &'static str,
    /// Where its start tag begins.
    start: usize,
    /// Where its content begins, right after the start tag.
    content: usize,
}

impl OpenElement {
    /// Write the start tag `<{tag}{attrs}>`.
    fn write(cx: &mut Ctx, tag: &'static str, attrs: &str) -> Self {
        let start = cx.body.len();
        cx.body.push_str(&format!("<{tag}{attrs}>"));
        Self {
            tag,
            start,
            content: cx.body.len(),
        }
    }

    /// Write the end tag. An element still without content (a list without
    /// items, or one reopened after a table with nothing left to hold) goes
    /// instead.
    fn close(&self, cx: &mut Ctx) {
        if cx.body.len() == self.content {
            cx.body.truncate(self.start);
        } else {
            cx.body.push_str(&format!("</{}>", self.tag));
        }
    }
}

/// A `text:list` that is open, and the item open in it.
pub(super) struct OpenList {
    list: OpenElement,
    /// Its list style.
    style: &'static str,
    /// The `xml:id` of an outermost list, which the list reopened after a
    /// table continues.
    id: Option<String>,
    /// The open item: a `text:list-item`, or a `text:list-header` for an
    /// item continued after a table.
    item: Option<OpenElement>,
}

pub(super) fn document(cx: &mut Ctx, doc: &ExportDoc) {
    let meta = cx.meta;
    text_paragraph(cx, "Title", &meta.title);
    let subtitle = format!("Exported from knot · {}", meta.exported_on);
    text_paragraph(cx, "Subtitle", &subtitle);
    blocks(cx, &doc.blocks, TEXT_WIDTH_CM);
}

/// Write blocks whose content may use `avail_cm` of width.
pub(super) fn blocks(cx: &mut Ctx, items: &[Block], avail_cm: f64) {
    for b in items {
        block(cx, b, avail_cm);
    }
}

fn block(cx: &mut Ctx, b: &Block, avail_cm: f64) {
    match b {
        Block::Paragraph(content) => {
            let style = if cx.quote_depth > 0 {
                "Quotations"
            } else {
                match cx.cell {
                    Some(cell) if cell.header => "Table_20_Heading",
                    Some(_) => "Table_20_Contents",
                    None => "Standard",
                }
            };
            open_paragraph(cx, style);
            inlines(cx, content);
            cx.body.push_str("</text:p>");
        }
        Block::Heading { level, content } => {
            let level = (*level).clamp(1, 6);
            let attrs = format!(r#" text:outline-level="{level}""#);
            open(cx, "text:h", HEADINGS[usize::from(level - 1)], &attrs);
            inlines(cx, content);
            cx.body.push_str("</text:h>");
        }
        Block::Quote(inner) => {
            cx.quote_depth += 1;
            blocks(cx, inner, (avail_cm - QUOTE_INDENT_CM).max(0.0));
            cx.quote_depth -= 1;
        }
        Block::Code { text, .. } => text_paragraph(cx, "Preformatted_20_Text", text),
        Block::Rule => {
            open_paragraph(cx, "Horizontal_20_Line");
            cx.body.push_str("</text:p>");
        }
        Block::List(l) => list(cx, l, avail_cm),
        Block::Table(t) => table::table(cx, t, avail_cm),
        Block::Image { source, alt } => picture::image(cx, source, alt, avail_cm),
        Block::Mermaid { key, source } => picture::mermaid(cx, key, source, avail_cm),
        Block::Board { key, label, .. } => picture::board(cx, key, label, avail_cm),
        Block::Attachment { url, name, size } => picture::attachment(cx, url, name, *size),
    }
}

/// A paragraph in `style` holding `text` as one unmarked run.
pub(super) fn text_paragraph(cx: &mut Ctx, style: &'static str, text: &str) {
    open_paragraph(cx, style);
    run(cx, text, &Marks::default());
    cx.body.push_str("</text:p>");
}

/// Open a `text:p` in `style`, aligned like the table cell it is in.
pub(super) fn open_paragraph(cx: &mut Ctx, style: &'static str) {
    open(cx, "text:p", style, "");
}

/// Open a paragraph element `tag` in `style` (see [`Ctx::para_style`]) with
/// extra `attrs`. Leading spaces of its text become `text:s`.
fn open(cx: &mut Ctx, tag: &str, style: &'static str, attrs: &str) {
    let style = cx.para_style(style);
    cx.body
        .push_str(&format!(r#"<{tag} text:style-name="{style}"{attrs}>"#));
    cx.after_char = false;
}

/// A list. Each one is its own `text:list` that continues no other, so a
/// numbered list starts at its `start`; nested lists nest, and their depth
/// picks the list style's level. ODF list styles have ten levels: a list
/// nested deeper continues the innermost list instead, its items becoming
/// items there, and what follows it in the enclosing item stays in the last
/// of them.
fn list(cx: &mut Ctx, l: &List, avail_cm: f64) {
    if cx.lists.len() >= LIST_LEVELS as usize {
        for item in &l.items {
            close_item(cx);
            list_item(cx, item, None, avail_cm);
        }
        return;
    }
    let style = if l.ordered {
        "Numbering_20_123"
    } else {
        "List_20_Bullet"
    };
    let id = cx.lists.is_empty().then(|| {
        cx.last_list_id += 1;
        format!("list{}", cx.last_list_id)
    });
    let id_attr = id
        .as_ref()
        .map(|id| format!(r#" xml:id="{id}""#))
        .unwrap_or_default();
    let attrs = format!(r#" text:style-name="{style}"{id_attr}"#);
    let list = OpenElement::write(cx, "text:list", &attrs);
    cx.lists.push(OpenList {
        list,
        style,
        id,
        item: None,
    });
    let inner = (avail_cm - LIST_INDENT_CM).max(0.0);
    for (i, item) in l.items.iter().enumerate() {
        let start = (l.ordered && i == 0).then_some(l.start);
        list_item(cx, item, start, inner);
        close_item(cx);
    }
    if let Some(open) = cx.lists.pop() {
        open.list.close(cx);
    }
}

/// Open an item in the innermost list and write its blocks, leaving it open
/// for the caller to close. Its first paragraph carries the bullet or
/// number, so an item that doesn't start with a paragraph gets an empty one.
/// A checklist item stays in its list, at its level, but takes the
/// label-less `List_20_Check` style, and its first paragraph starts with a
/// box character instead. An ordered list's first item carries its start.
fn list_item(cx: &mut Ctx, item: &ListItem, start: Option<u32>, avail_cm: f64) {
    let mut attrs = String::new();
    if let Some(start) = start {
        attrs.push_str(&format!(r#" text:start-value="{start}""#));
    }
    if item.checked.is_some() {
        attrs.push_str(r#" text:style-override="List_20_Check""#);
    }
    let open = OpenElement::write(cx, "text:list-item", &attrs);
    if let Some(list) = cx.lists.last_mut() {
        list.item = Some(open);
    }

    let (lead, tail) = match item.blocks.split_first() {
        Some((Block::Paragraph(content), tail)) => (content.as_slice(), tail),
        _ => (&[][..], &item.blocks[..]),
    };
    open_paragraph(cx, "List_20_Paragraph");
    if let Some(checked) = item.checked {
        let marker = if checked { "\u{2611} " } else { "\u{2610} " };
        run(cx, marker, &Marks::default());
    }
    inlines(cx, lead);
    cx.body.push_str("</text:p>");

    for b in tail {
        match b {
            Block::Paragraph(content) => {
                open_paragraph(cx, "List_20_Paragraph");
                inlines(cx, content);
                cx.body.push_str("</text:p>");
            }
            _ => block(cx, b, avail_cm),
        }
    }
}

/// Close the open item of the innermost list, if any.
fn close_item(cx: &mut Ctx) {
    if let Some(item) = cx.lists.last_mut().and_then(|l| l.item.take()) {
        item.close(cx);
    }
}

/// Close every open list, innermost first, before a block no list item may
/// hold (a table). Returns them for [`reopen_lists`].
pub(super) fn close_lists(cx: &mut Ctx) -> Vec<OpenList> {
    let lists = std::mem::take(&mut cx.lists);
    for l in lists.iter().rev() {
        if let Some(item) = &l.item {
            item.close(cx);
        }
        l.list.close(cx);
    }
    lists
}

/// Reopen the lists [`close_lists`] closed. The outermost continues the
/// original through `text:continue-list`, and each open item continues as a
/// list header, which has no bullet or number of its own. Whatever ends up
/// with nothing in it goes again when it closes.
pub(super) fn reopen_lists(cx: &mut Ctx, mut lists: Vec<OpenList>) {
    for l in &mut lists {
        let continues =
            l.id.as_ref()
                .map(|id| format!(r#" text:continue-list="{id}""#))
                .unwrap_or_default();
        let attrs = format!(r#" text:style-name="{}"{continues}"#, l.style);
        l.list = OpenElement::write(cx, "text:list", &attrs);
        if l.item.is_some() {
            l.item = Some(OpenElement::write(cx, "text:list-header", ""));
        }
    }
    cx.lists = lists;
}

/// How far, in cm, the open lists and quotes indent the text: where a table
/// written among them starts.
pub(super) fn indent_cm(cx: &Ctx, lists: &[OpenList]) -> f64 {
    LIST_INDENT_CM * lists.len() as f64 + QUOTE_INDENT_CM * f64::from(cx.quote_depth)
}

fn inlines(cx: &mut Ctx, content: &[Inline]) {
    for inline in content {
        match inline {
            Inline::Run(r) => run(cx, &r.text, &r.marks),
            Inline::Break => {
                cx.body.push_str("<text:line-break/>");
                cx.after_char = false;
            }
            Inline::Link { target, runs } => link(cx, target, runs),
        }
    }
}

/// Web and page links become `text:a`. A mention becomes the plain text
/// `@<display name>`, or keeps its stored text when the user is gone.
/// Anything else stays plain text, so no unsafe target reaches the file.
fn link(cx: &mut Ctx, target: &LinkTarget, runs: &[Run]) {
    let (meta, assets) = (cx.meta, cx.assets);
    let url = match target {
        LinkTarget::Web(url) => url.clone(),
        LinkTarget::Doc(id) => format!("{}/doc/{id}", meta.base_url.trim_end_matches('/')),
        LinkTarget::User(id) if assets.user_names.contains_key(id) => {
            let marks = runs
                .first()
                .map_or_else(Marks::default, |r| r.marks.clone());
            run(cx, &format!("@{}", assets.user_names[id]), &marks);
            return;
        }
        LinkTarget::User(_) | LinkTarget::Plain => {
            for r in runs {
                run(cx, &r.text, &r.marks);
            }
            return;
        }
    };
    cx.body.push_str(&format!(
        r#"<text:a xlink:type="simple" xlink:href="{}" text:style-name="Internet_20_link">"#,
        esc_attr(&url)
    ));
    for r in runs {
        run(cx, &r.text, &r.marks);
    }
    cx.body.push_str("</text:a>");
}

/// One run. Inline code takes the `Source_20_Text` character style; the
/// other marks an automatic style in a span inside it.
fn run(cx: &mut Ctx, text: &str, marks: &Marks) {
    if text.is_empty() {
        return;
    }
    let direct = cx.text_style(marks);
    if marks.code {
        cx.body
            .push_str(r#"<text:span text:style-name="Source_20_Text">"#);
    }
    if let Some(style) = &direct {
        cx.body
            .push_str(&format!(r#"<text:span text:style-name="{style}">"#));
    }
    push_text(cx, text);
    if direct.is_some() {
        cx.body.push_str("</text:span>");
    }
    if marks.code {
        cx.body.push_str("</text:span>");
    }
}

/// Write `text` so a reader sees exactly it. ODF collapses white space in
/// all text, so a tab becomes `text:tab`, a line break (`\n`, `\r\n` or
/// `\r`) `text:line-break`, and a run of spaces one literal space plus
/// `text:s` for the rest. A space that doesn't follow a character written
/// as character data (at the start of the paragraph, or after an element)
/// would be dropped, so such a run is all `text:s`. Characters XML forbids
/// are dropped.
fn push_text(cx: &mut Ctx, text: &str) {
    let out = &mut cx.body;
    let mut rest = text;
    loop {
        let end = rest.find([' ', '\t', '\n', '\r']).unwrap_or(rest.len());
        let chars = esc_text(&rest[..end]);
        if !chars.is_empty() {
            out.push_str(&chars);
            cx.after_char = true;
        }
        rest = &rest[end..];
        let len = match rest.as_bytes().first() {
            None => return,
            Some(b' ') => {
                let n = rest.len() - rest.trim_start_matches(' ').len();
                if cx.after_char {
                    out.push(' ');
                    if n > 1 {
                        out.push_str(&format!(r#"<text:s text:c="{}"/>"#, n - 1));
                    }
                } else {
                    out.push_str(&format!(r#"<text:s text:c="{n}"/>"#));
                }
                n
            }
            Some(b'\t') => {
                out.push_str("<text:tab/>");
                1
            }
            Some(_) => {
                out.push_str("<text:line-break/>");
                if rest.starts_with("\r\n") { 2 } else { 1 }
            }
        };
        cx.after_char = false;
        rest = &rest[len..];
    }
}
