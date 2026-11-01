//! Pictures, and what stands in for them: inline drawings for embedded
//! images and rendered diagrams, captions for the ones that are missing, and
//! links for external images and attachments.

use crate::assets::{ImageAsset, Picture};
use crate::media::{base_url, fit_emu, human_size};
use crate::model::{ImageSource, Marks};
use crate::xml::esc_attr;

use super::body::{open_paragraph, run};
use super::{Ctx, TEXT_HEIGHT_TWIPS};

/// EMU per twip (914400 EMU per inch, 1440 twips per inch).
const EMU_PER_TWIP: u64 = 635;

/// Tallest picture, in EMU: one that still fits on a page. Word lays an
/// inline picture out in a Normal paragraph, whose 1.15 line spacing
/// (`w:line="276"`) it applies to the picture's line too, followed by
/// 160 twips of spacing. About 21.2 cm.
const MAX_HEIGHT_EMU: u64 = (TEXT_HEIGHT_TWIPS as u64 - 160) * 240 / 276 * EMU_PER_TWIP;

/// A body image. Embedded ones become drawings; the rest become a caption
/// or a link, and the fallback counts say which.
pub(super) fn image(cx: &mut Ctx, source: &ImageSource, alt: &str, avail_twips: u32) {
    match source {
        ImageSource::Blob(id) => {
            let assets = cx.assets;
            match assets.images.get(id) {
                Some(ImageAsset::Embedded(pic)) => {
                    drawing(cx, &format!("image:{id}"), pic, alt, avail_twips);
                }
                Some(ImageAsset::OverBudget) => {
                    cx.fallbacks.image_over_budget += 1;
                    let url = format!("{}/api/blobs/{id}", base_url(&cx.meta.base_url));
                    caption_link(cx, "[Image too large to include]", &url);
                }
                Some(ImageAsset::Unavailable) | None => unavailable(cx, alt),
            }
        }
        ImageSource::Unsupported => unavailable(cx, alt),
        ImageSource::External(url) => {
            open_paragraph(cx, None);
            link_run(cx, if alt.is_empty() { url } else { alt }, url);
            cx.xml.push_str("</w:p>");
        }
    }
}

/// A Mermaid block: the rendered diagram, or its source and a note.
pub(super) fn mermaid(cx: &mut Ctx, key: &str, source: &str, avail_twips: u32) {
    let assets = cx.assets;
    if let Some(pic) = assets.diagrams.get(key) {
        drawing(
            cx,
            &format!("diagram:{key}"),
            pic,
            "Mermaid diagram",
            avail_twips,
        );
        return;
    }
    cx.fallbacks.diagram += 1;
    open_paragraph(cx, Some("Code"));
    run(cx, source, &Marks::default(), None);
    cx.xml.push_str("</w:p>");
    caption(cx, "Mermaid diagram (not included)");
}

/// A board block: the rendered diagram, or a note with the board's label.
pub(super) fn board(cx: &mut Ctx, key: &str, label: &str, avail_twips: u32) {
    let descr = format!("Diagram: {label}");
    let assets = cx.assets;
    if let Some(pic) = assets.diagrams.get(key) {
        drawing(cx, &format!("diagram:{key}"), pic, &descr, avail_twips);
        return;
    }
    cx.fallbacks.diagram += 1;
    caption(cx, &format!("{descr} (not included)"));
}

/// A line naming the file and its size, linked back to knot when the
/// attachment lives on this instance.
pub(super) fn attachment(cx: &mut Ctx, url: &str, name: &str, size: Option<u64>) {
    let mut line = format!("\u{1F4CE} {name}");
    if let Some(size) = size {
        line.push_str(&format!(" ({})", human_size(size)));
    }
    open_paragraph(cx, None);
    if url.starts_with('/') {
        let target = format!("{}{url}", base_url(&cx.meta.base_url));
        link_run(cx, &line, &target);
    } else {
        run(cx, &line, &Marks::default(), None);
    }
    cx.xml.push_str("</w:p>");
}

fn unavailable(cx: &mut Ctx, alt: &str) {
    cx.fallbacks.image_unavailable += 1;
    let text = if alt.is_empty() {
        "[Image not available]".to_owned()
    } else {
        format!("[Image not available] \u{2014} {alt}")
    };
    caption(cx, &text);
}

fn caption(cx: &mut Ctx, text: &str) {
    open_paragraph(cx, Some("Caption"));
    run(cx, text, &Marks::default(), None);
    cx.xml.push_str("</w:p>");
}

fn caption_link(cx: &mut Ctx, text: &str, url: &str) {
    open_paragraph(cx, Some("Caption"));
    link_run(cx, text, url);
    cx.xml.push_str("</w:p>");
}

/// `text` as a hyperlink to `url`.
fn link_run(cx: &mut Ctx, text: &str, url: &str) {
    let id = cx.hyperlink(url);
    cx.xml
        .push_str(&format!(r#"<w:hyperlink r:id="{id}" w:history="1">"#));
    run(cx, text, &Marks::default(), Some("Hyperlink"));
    cx.xml.push_str("</w:hyperlink>");
}

/// An inline drawing of `pic` in its own paragraph, at most `avail_twips`
/// wide and a page high. `key` identifies the picture, so one used twice is
/// stored once.
fn drawing<'a>(cx: &mut Ctx<'a>, key: &str, pic: &'a Picture, descr: &str, avail_twips: u32) {
    let (n, rid, ext) = cx.media_part(key, pic);
    let (cx_emu, cy_emu) = fit_emu(pic, u64::from(avail_twips) * EMU_PER_TWIP, MAX_HEIGHT_EMU);
    // A zero extent is invalid; a collapsed width or height is still drawn.
    let (w, h) = (cx_emu.max(1), cy_emu.max(1));
    let id = cx.doc_pr_id();
    let descr = esc_attr(descr);
    open_paragraph(cx, None);
    cx.xml.push_str(&format!(
        concat!(
            r#"<w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0">"#,
            r#"<wp:extent cx="{w}" cy="{h}"/><wp:effectExtent l="0" t="0" r="0" b="0"/>"#,
            r#"<wp:docPr id="{id}" name="Picture {id}" descr="{descr}"/>"#,
            r#"<wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr>"#,
            r#"<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic>"#,
            r#"<pic:nvPicPr><pic:cNvPr id="{id}" name="image{n}.{ext}" descr="{descr}"/><pic:cNvPicPr/></pic:nvPicPr>"#,
            r#"<pic:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>"#,
            r#"<pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{w}" cy="{h}"/></a:xfrm>"#,
            r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>"#,
            r#"</pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"#,
        ),
        w = w,
        h = h,
        id = id,
        descr = descr,
        n = n,
        ext = ext,
        rid = rid,
    ));
}
