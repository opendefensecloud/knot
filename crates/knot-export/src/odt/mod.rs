//! ODT (OpenDocument text) writer: hand-written XML in an in-memory zip.
//!
//! All formatting goes through named styles, except the marks of a run and a
//! table's widths and borders, which are automatic styles in `content.xml`.
//! The part a company template would replace (`styles.xml`) comes in as
//! [`OdtDefaults`].

mod body;
mod styles;
mod table;

use std::borrow::Cow;
use std::collections::HashMap;

use crate::assets::{Assets, ExportError, Fallbacks, Meta, Output};
use crate::model::{ExportDoc, Marks};
use crate::xml::{ZipOut, esc_attr, esc_text};

const MIMETYPE: &str = "application/vnd.oasis.opendocument.text";

/// Width available to top-level blocks: A4 less two 2.5 cm margins.
const TEXT_WIDTH_CM: f64 = 16.0;
/// Left margin of the Quotations style.
const QUOTE_INDENT_CM: f64 = 1.0;
/// Indent step per list level.
const LIST_INDENT_CM: f64 = 1.27;
/// Levels of a list style; deeper lists continue the deepest level.
const LIST_LEVELS: u32 = 10;

/// The template-ready package defaults (spec §10): the contents of
/// `styles.xml`, with the named styles, the list styles, the page layout and
/// the footer.
pub struct OdtDefaults {
    pub styles_xml: Cow<'static, str>,
}

impl OdtDefaults {
    /// knot's neutral look (spec §7).
    pub fn builtin() -> Self {
        Self {
            styles_xml: Cow::Owned(styles::builtin_xml()),
        }
    }
}

/// Write `doc` as an ODT package with the built-in defaults.
pub fn write_odt(doc: &ExportDoc, meta: &Meta, assets: &Assets) -> Result<Output, ExportError> {
    write_odt_with(doc, meta, assets, &OdtDefaults::builtin())
}

/// Write `doc` as an ODT package on top of the given package defaults.
pub fn write_odt_with(
    doc: &ExportDoc,
    meta: &Meta,
    assets: &Assets,
    defaults: &OdtDefaults,
) -> Result<Output, ExportError> {
    let mut cx = Ctx::new(meta, assets);
    body::document(&mut cx, doc);

    let content = cx.content_xml();
    let meta = meta_xml(meta);
    // Every part but the mimetype and the manifest, as (path, media type,
    // bytes); the manifest lists them.
    let parts = [
        ("content.xml", "text/xml", content.as_bytes()),
        ("styles.xml", "text/xml", defaults.styles_xml.as_bytes()),
        ("meta.xml", "text/xml", meta.as_bytes()),
    ];
    let mut zip = ZipOut::new();
    // First and uncompressed, so the file type can be read at a fixed offset.
    zip.stored("mimetype", MIMETYPE.as_bytes())?;
    for (path, _, bytes) in &parts {
        zip.deflated(path, bytes)?;
    }
    let manifest = manifest_xml(parts.iter().map(|(path, kind, _)| (*path, *kind)));
    zip.deflated("META-INF/manifest.xml", manifest.as_bytes())?;
    Ok(Output {
        bytes: zip.finish()?,
        fallbacks: cx.fallbacks,
    })
}

/// The table cell the block being written is in.
#[derive(Clone, Copy)]
struct CellCx {
    header: bool,
    /// `fo:text-align` of its paragraphs.
    align: &'static str,
}

/// Writer state for one document.
struct Ctx<'a> {
    meta: &'a Meta,
    assets: &'a Assets,
    /// The `office:text` content written so far.
    body: String,
    /// The automatic styles `body` uses, for `content.xml`.
    auto_styles: String,
    fallbacks: Fallbacks,
    /// Whether the last thing written in the open paragraph is a character
    /// other than white space, so a space may follow as character data.
    after_char: bool,
    /// How many quotes enclose the block being written, in its table cell.
    quote_depth: u32,
    /// The lists open around the block being written, outermost first, in
    /// its table cell.
    lists: Vec<body::OpenList>,
    /// The table cell the block being written is in.
    cell: Option<CellCx>,
    /// Last `xml:id` number given to an outermost list.
    last_list_id: u32,
    /// Last table number; it names the table and its styles.
    last_table: u32,
    /// Automatic text style per set of run marks.
    text_styles: HashMap<u8, String>,
    /// Automatic paragraph style per (parent style, alignment), for cells.
    para_styles: HashMap<(&'static str, &'static str), String>,
    /// Whether the table cell styles are in `auto_styles` yet.
    cell_styles: bool,
}

impl<'a> Ctx<'a> {
    fn new(meta: &'a Meta, assets: &'a Assets) -> Self {
        Self {
            meta,
            assets,
            body: String::new(),
            auto_styles: String::new(),
            fallbacks: Fallbacks::default(),
            after_char: false,
            quote_depth: 0,
            lists: Vec::new(),
            cell: None,
            last_list_id: 0,
            last_table: 0,
            text_styles: HashMap::new(),
            para_styles: HashMap::new(),
            cell_styles: false,
        }
    }

    /// The automatic text style carrying a run's bold, italic, underline and
    /// strike marks; `None` when it has none. One style per combination.
    fn text_style(&mut self, marks: &Marks) -> Option<String> {
        let key = u8::from(marks.bold)
            | u8::from(marks.italic) << 1
            | u8::from(marks.underline) << 2
            | u8::from(marks.strike) << 3;
        if key == 0 {
            return None;
        }
        if let Some(name) = self.text_styles.get(&key) {
            return Some(name.clone());
        }
        let name = format!("T{}", self.text_styles.len() + 1);
        self.auto_styles.push_str(&format!(
            r#"<style:style style:name="{name}" style:family="text"><style:text-properties"#
        ));
        for (on, props) in [
            (marks.bold, BOLD),
            (marks.italic, ITALIC),
            (marks.underline, UNDERLINE),
            (marks.strike, STRIKE),
        ] {
            if on {
                self.auto_styles.push_str(props);
            }
        }
        self.auto_styles.push_str("/></style:style>");
        self.text_styles.insert(key, name.clone());
        Some(name)
    }

    /// The paragraph style to write for `base`: `base` itself, or inside a
    /// table cell an automatic style based on it with the cell's alignment.
    fn para_style(&mut self, base: &'static str) -> String {
        let Some(cell) = self.cell else {
            return base.to_owned();
        };
        let key = (base, cell.align);
        if let Some(name) = self.para_styles.get(&key) {
            return name.clone();
        }
        let name = format!("P{}", self.para_styles.len() + 1);
        self.auto_styles.push_str(&format!(
            concat!(
                r#"<style:style style:name="{name}" style:family="paragraph" style:parent-style-name="{base}">"#,
                r#"<style:paragraph-properties fo:text-align="{align}"/></style:style>"#,
            ),
            name = name,
            base = base,
            align = cell.align,
        ));
        self.para_styles.insert(key, name.clone());
        name
    }

    fn content_xml(&self) -> String {
        let mut s = String::with_capacity(
            CONTENT_HEAD.len() + self.auto_styles.len() + self.body.len() + 200,
        );
        s.push_str(CONTENT_HEAD);
        s.push_str("<office:automatic-styles>");
        s.push_str(&self.auto_styles);
        s.push_str("</office:automatic-styles><office:body><office:text>");
        s.push_str(&self.body);
        s.push_str("</office:text></office:body></office:document-content>");
        s
    }
}

const BOLD: &str =
    r#" fo:font-weight="bold" style:font-weight-asian="bold" style:font-weight-complex="bold""#;
const ITALIC: &str =
    r#" fo:font-style="italic" style:font-style-asian="italic" style:font-style-complex="italic""#;
const UNDERLINE: &str = r#" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color""#;
const STRIKE: &str =
    r#" style:text-line-through-style="solid" style:text-line-through-type="single""#;

const CONTENT_HEAD: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?>"#,
    r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
    r#"xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" "#,
    r#"xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" "#,
    r#"xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" "#,
    r#"xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" "#,
    r#"xmlns:xlink="http://www.w3.org/1999/xlink" "#,
    r#"office:version="1.3">"#,
);

/// The title and the generator. No creator, so the exporter's name stays
/// out of forwarded files.
fn meta_xml(meta: &Meta) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
            r#"xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" "#,
            r#"xmlns:dc="http://purl.org/dc/elements/1.1/" office:version="1.3">"#,
            r#"<office:meta><meta:generator>knot</meta:generator><dc:title>{title}</dc:title></office:meta>"#,
            r#"</office:document-meta>"#,
        ),
        title = esc_text(&meta.title),
    )
}

/// The manifest: the package itself, then `parts` as (path, media type).
fn manifest_xml<'p>(parts: impl IntoIterator<Item = (&'p str, &'p str)>) -> String {
    let mut s = String::from(concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">"#,
    ));
    s.push_str(&format!(
        r#"<manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="{MIMETYPE}"/>"#
    ));
    for (path, media_type) in parts {
        s.push_str(&format!(
            r#"<manifest:file-entry manifest:full-path="{}" manifest:media-type="{}"/>"#,
            esc_attr(path),
            esc_attr(media_type),
        ));
    }
    s.push_str("</manifest:manifest>");
    s
}
