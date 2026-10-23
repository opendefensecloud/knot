//! `word/document.xml`: the title block, the blocks and the section
//! properties.
//!
//! Block emitters take `avail_twips`, the width their content may use. It
//! starts at the text width and shrinks inside quotes, list items and table
//! cells, so tables and pictures can size themselves to it.

use crate::model::{Block, ExportDoc, Inline, LinkTarget, Marks, Run};
use crate::xml::esc_text;

use super::{Ctx, FOOTER_RID, MARGIN_TWIPS, PAGE_HEIGHT_TWIPS, PAGE_WIDTH_TWIPS, TEXT_WIDTH_TWIPS};

/// Left indent of the Quote style, in twips.
const QUOTE_INDENT_TWIPS: u32 = 567;

pub(super) fn document(cx: &mut Ctx, doc: &ExportDoc) {
    cx.xml.push_str(concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
        "<w:body>",
    ));
    let meta = cx.meta;
    text_paragraph(cx, "Title", &meta.title);
    let subtitle = format!("Exported from knot · {}", meta.exported_on);
    text_paragraph(cx, "Subtitle", &subtitle);

    blocks(cx, &doc.blocks, TEXT_WIDTH_TWIPS);

    cx.xml.push_str(&format!(
        concat!(
            r#"<w:sectPr><w:footerReference w:type="default" r:id="{footer}"/>"#,
            r#"<w:pgSz w:w="{width}" w:h="{height}"/>"#,
            r#"<w:pgMar w:top="{m}" w:right="{m}" w:bottom="{m}" w:left="{m}" "#,
            r#"w:header="708" w:footer="708" w:gutter="0"/></w:sectPr>"#,
            "</w:body></w:document>",
        ),
        footer = FOOTER_RID,
        width = PAGE_WIDTH_TWIPS,
        height = PAGE_HEIGHT_TWIPS,
        m = MARGIN_TWIPS,
    ));
}

/// Write blocks whose content may use `avail_twips` of width.
fn blocks(cx: &mut Ctx, items: &[Block], avail_twips: u32) {
    for b in items {
        block(cx, b, avail_twips);
    }
}

fn block(cx: &mut Ctx, b: &Block, avail_twips: u32) {
    match b {
        Block::Paragraph(content) => {
            let style = (cx.quote_depth > 0).then_some("Quote");
            open_paragraph(cx, style);
            inlines(cx, content);
            cx.xml.push_str("</w:p>");
        }
        Block::Heading { level, content } => {
            let style = format!("Heading{}", (*level).clamp(1, 6));
            open_paragraph(cx, Some(&style));
            inlines(cx, content);
            cx.xml.push_str("</w:p>");
        }
        Block::Quote(inner) => {
            cx.quote_depth += 1;
            blocks(cx, inner, avail_twips.saturating_sub(QUOTE_INDENT_TWIPS));
            cx.quote_depth -= 1;
        }
        Block::Code { text, .. } => text_paragraph(cx, "Code", text),
        Block::Rule => {
            open_paragraph(cx, Some("HorizontalRule"));
            cx.xml.push_str("</w:p>");
        }
        // Not written yet: an empty Normal paragraph keeps the document valid.
        Block::List(_)
        | Block::Table(_)
        | Block::Mermaid { .. }
        | Block::Image { .. }
        | Block::Board { .. }
        | Block::Attachment { .. } => cx.xml.push_str("<w:p/>"),
    }
}

/// A paragraph in `style` holding `text` as one unmarked run.
fn text_paragraph(cx: &mut Ctx, style: &str, text: &str) {
    open_paragraph(cx, Some(style));
    run(cx, text, &Marks::default(), None);
    cx.xml.push_str("</w:p>");
}

/// Open a `w:p`, with its paragraph style unless it is Normal.
fn open_paragraph(cx: &mut Ctx, style: Option<&str>) {
    cx.xml.push_str("<w:p>");
    if let Some(style) = style {
        cx.xml.push_str(r#"<w:pPr><w:pStyle w:val=""#);
        cx.xml.push_str(style);
        cx.xml.push_str(r#""/></w:pPr>"#);
    }
}

fn inlines(cx: &mut Ctx, content: &[Inline]) {
    for inline in content {
        match inline {
            Inline::Run(r) => run(cx, &r.text, &r.marks, None),
            Inline::Break => cx.xml.push_str("<w:r><w:br/></w:r>"),
            Inline::Link { target, runs } => link(cx, target, runs),
        }
    }
}

/// Web and page links become hyperlinks. A mention becomes the plain text
/// `@<display name>`, or keeps its stored text when the user is gone. Anything
/// else stays plain text, so no unsafe target reaches the file.
fn link(cx: &mut Ctx, target: &LinkTarget, runs: &[Run]) {
    let (meta, assets) = (cx.meta, cx.assets);
    let url = match target {
        LinkTarget::Web(url) => url.clone(),
        LinkTarget::Doc(id) => format!("{}/doc/{id}", meta.base_url.trim_end_matches('/')),
        LinkTarget::User(id) if assets.user_names.contains_key(id) => {
            let marks = runs
                .first()
                .map_or_else(Marks::default, |r| r.marks.clone());
            run(cx, &format!("@{}", assets.user_names[id]), &marks, None);
            return;
        }
        LinkTarget::User(_) | LinkTarget::Plain => {
            for r in runs {
                run(cx, &r.text, &r.marks, None);
            }
            return;
        }
    };
    let id = cx.hyperlink(&url);
    cx.xml
        .push_str(&format!(r#"<w:hyperlink r:id="{id}" w:history="1">"#));
    for r in runs {
        run(cx, &r.text, &r.marks, Some("Hyperlink"));
    }
    cx.xml.push_str("</w:hyperlink>");
}

/// One `w:r`. Tabs become `w:tab`, line breaks `w:br`, and the text between
/// them a space-preserving `w:t`. A run takes one character style, so a
/// link's style wins over inline code.
fn run(cx: &mut Ctx, text: &str, marks: &Marks, char_style: Option<&str>) {
    if text.is_empty() {
        return;
    }
    let out = &mut cx.xml;
    out.push_str("<w:r>");
    run_props(
        out,
        marks,
        char_style.or(marks.code.then_some("InlineCode")),
    );
    let mut rest = text;
    while let Some(i) = rest.find(['\t', '\n', '\r']) {
        push_text(out, &rest[..i]);
        let tail = &rest[i..];
        out.push_str(if tail.starts_with('\t') {
            "<w:tab/>"
        } else {
            "<w:br/>"
        });
        rest = &tail[if tail.starts_with("\r\n") { 2 } else { 1 }..];
    }
    push_text(out, rest);
    out.push_str("</w:r>");
}

/// `w:rPr` in schema order: `rStyle, b, bCs, i, iCs, strike, u`.
fn run_props(out: &mut String, marks: &Marks, style: Option<&str>) {
    if style.is_none() && !(marks.bold || marks.italic || marks.strike || marks.underline) {
        return;
    }
    out.push_str("<w:rPr>");
    if let Some(style) = style {
        out.push_str(r#"<w:rStyle w:val=""#);
        out.push_str(style);
        out.push_str(r#""/>"#);
    }
    if marks.bold {
        out.push_str("<w:b/><w:bCs/>");
    }
    if marks.italic {
        out.push_str("<w:i/><w:iCs/>");
    }
    if marks.strike {
        out.push_str("<w:strike/>");
    }
    if marks.underline {
        out.push_str(r#"<w:u w:val="single"/>"#);
    }
    out.push_str("</w:rPr>");
}

fn push_text(out: &mut String, text: &str) {
    let text = esc_text(text);
    if !text.is_empty() {
        out.push_str(r#"<w:t xml:space="preserve">"#);
        out.push_str(&text);
        out.push_str("</w:t>");
    }
}
