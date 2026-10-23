//! Pictures, and what stands in for them: frames for embedded images and
//! rendered diagrams, captions for the ones that are missing, and links for
//! external images and attachments. Placeholders, wording and fallback
//! counting match the DOCX writer.

use crate::assets::{ImageAsset, PicFormat, Picture};
use crate::media::fit_cm;
use crate::model::ImageSource;
use crate::xml::{esc_attr, esc_text};

use super::Ctx;
use super::body::{open_paragraph, text_paragraph};

/// Smallest frame side written, in cm: a zero size is invalid.
const MIN_CM: f64 = 0.001;

/// A body image. Embedded ones become frames; the rest become a caption or
/// a link, and the fallback counts say which.
pub(super) fn image(cx: &mut Ctx, source: &ImageSource, alt: &str, avail_cm: f64) {
    match source {
        ImageSource::Blob(id) => {
            let assets = cx.assets;
            match assets.images.get(id) {
                Some(ImageAsset::Embedded(pic)) => {
                    frame(cx, &format!("image:{id}"), pic, alt, avail_cm);
                }
                Some(ImageAsset::OverBudget) => {
                    cx.fallbacks.image_over_budget += 1;
                    let url = format!("{}/api/blobs/{id}", base_url(cx));
                    open_paragraph(cx, "Caption");
                    link(cx, "[Image too large to include]", &url);
                    cx.body.push_str("</text:p>");
                }
                Some(ImageAsset::Unavailable) | None => unavailable(cx, alt),
            }
        }
        ImageSource::Unsupported => unavailable(cx, alt),
        ImageSource::External(url) => {
            open_paragraph(cx, "Standard");
            link(cx, if alt.is_empty() { url } else { alt }, url);
            cx.body.push_str("</text:p>");
        }
    }
}

/// A Mermaid block: the rendered diagram, or its source and a note.
pub(super) fn mermaid(cx: &mut Ctx, key: &str, source: &str, avail_cm: f64) {
    let assets = cx.assets;
    if let Some(pic) = assets.diagrams.get(key) {
        frame(
            cx,
            &format!("diagram:{key}"),
            pic,
            "Mermaid diagram",
            avail_cm,
        );
        return;
    }
    cx.fallbacks.diagram += 1;
    text_paragraph(cx, "Preformatted_20_Text", source);
    text_paragraph(cx, "Caption", "Mermaid diagram (not included)");
}

/// A board block: the rendered diagram, or a note with the board's label.
pub(super) fn board(cx: &mut Ctx, key: &str, label: &str, avail_cm: f64) {
    let descr = format!("Diagram: {label}");
    let assets = cx.assets;
    if let Some(pic) = assets.diagrams.get(key) {
        frame(cx, &format!("diagram:{key}"), pic, &descr, avail_cm);
        return;
    }
    cx.fallbacks.diagram += 1;
    text_paragraph(cx, "Caption", &format!("{descr} (not included)"));
}

/// A line naming the file and its size, linked back to knot when the
/// attachment lives on this instance.
pub(super) fn attachment(cx: &mut Ctx, url: &str, name: &str, size: Option<u64>) {
    let mut line = format!("\u{1F4CE} {name}");
    if let Some(size) = size {
        line.push_str(&format!(" ({})", human_size(size)));
    }
    if url.starts_with('/') {
        let target = format!("{}{url}", base_url(cx));
        open_paragraph(cx, "Standard");
        link(cx, &line, &target);
        cx.body.push_str("</text:p>");
    } else {
        text_paragraph(cx, "Standard", &line);
    }
}

fn base_url<'a>(cx: &Ctx<'a>) -> &'a str {
    cx.meta.base_url.trim_end_matches('/')
}

fn unavailable(cx: &mut Ctx, alt: &str) {
    cx.fallbacks.image_unavailable += 1;
    let text = if alt.is_empty() {
        "[Image not available]".to_owned()
    } else {
        format!("[Image not available] \u{2014} {alt}")
    };
    text_paragraph(cx, "Caption", &text);
}

/// `text` as a hyperlink to `url`, in the open paragraph.
fn link(cx: &mut Ctx, text: &str, url: &str) {
    cx.body.push_str(&format!(
        r#"<text:a xlink:type="simple" xlink:href="{}" text:style-name="Internet_20_link">{}</text:a>"#,
        esc_attr(url),
        esc_text(text),
    ));
    cx.after_char = true;
}

/// 48 KB, 1.5 MB: binary units, whole kilobytes.
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    match bytes {
        0..KB => format!("{bytes} B"),
        KB..MB => format!("{} KB", (bytes + KB / 2) / KB),
        MB..GB => format!("{:.1} MB", bytes as f64 / MB as f64),
        _ => format!("{:.1} GB", bytes as f64 / GB as f64),
    }
}

/// An as-character frame holding `pic` in its own paragraph, at most
/// `avail_cm` wide. `key` identifies the picture, so one used twice is
/// stored once.
fn frame(cx: &mut Ctx, key: &str, pic: &Picture, descr: &str, avail_cm: f64) {
    let (n, ext) = cx.media_part(key, pic);
    let (w, h) = fit_cm(pic, avail_cm);
    let (w, h) = (w.max(MIN_CM), h.max(MIN_CM));
    let name = cx.next_frame_name();
    let style = cx.frame_style();
    open_paragraph(cx, "Standard");
    cx.body.push_str(&format!(
        concat!(
            r#"<draw:frame draw:style-name="{style}" draw:name="{name}" text:anchor-type="as-char" "#,
            r#"svg:width="{w:.3}cm" svg:height="{h:.3}cm">"#,
            r#"<draw:image xlink:href="Pictures/img{n}.{ext}" xlink:type="simple" "#,
            r#"xlink:show="embed" xlink:actuate="onLoad"/>"#,
            r#"<svg:desc>{descr}</svg:desc></draw:frame></text:p>"#,
        ),
        style = style,
        name = esc_attr(&name),
        w = w,
        h = h,
        n = n,
        ext = ext,
        descr = esc_text(descr),
    ));
    cx.after_char = false;
}

/// File extension of a picture's part. JPEG is `jpeg`, as in the DOCX writer.
pub(super) fn extension(format: PicFormat) -> &'static str {
    match format {
        PicFormat::Png => "png",
        PicFormat::Jpeg => "jpeg",
        PicFormat::Gif => "gif",
    }
}

/// Media type of a picture's part, for the manifest.
pub(super) fn media_type(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpeg" => "image/jpeg",
        _ => "image/gif",
    }
}
