//! DOCX (Office Open XML) writer: hand-written XML in an in-memory zip.
//!
//! All formatting goes through named styles. The parts a company template
//! would replace (styles, numbering, footer) come in as [`DocxDefaults`].

mod body;
mod styles;

use std::borrow::Cow;
use std::collections::HashMap;

use crate::assets::{Assets, ExportError, Fallbacks, Meta, Output};
use crate::model::ExportDoc;
use crate::xml::{ZipOut, esc_attr, esc_text};

/// A4 portrait with 2.5 cm margins, in twips.
const PAGE_WIDTH_TWIPS: u32 = 11906;
const PAGE_HEIGHT_TWIPS: u32 = 16838;
const MARGIN_TWIPS: u32 = 1417;
/// Width available to top-level blocks (16 cm).
const TEXT_WIDTH_TWIPS: u32 = PAGE_WIDTH_TWIPS - 2 * MARGIN_TWIPS;

const REL_TYPE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";

/// Relationships every document has, as (type, target). They take ids
/// `rId1`..; generated relationships follow.
const FIXED_RELS: [(&str, &str); 4] = [
    ("styles", "styles.xml"),
    ("numbering", "numbering.xml"),
    ("settings", "settings.xml"),
    ("footer", "footer1.xml"),
];
/// Id of the `footer1.xml` relationship (the fourth fixed one).
const FOOTER_RID: &str = "rId4";

/// The template-ready package defaults (spec §10): the contents of
/// `word/styles.xml`, `word/numbering.xml` and `word/footer1.xml`.
pub struct DocxDefaults {
    pub styles_xml: Cow<'static, str>,
    pub numbering_xml: Cow<'static, str>,
    pub footer_xml: Cow<'static, str>,
}

impl DocxDefaults {
    /// knot's neutral look (spec §7).
    pub fn builtin() -> Self {
        Self {
            styles_xml: Cow::Borrowed(styles::STYLES_XML),
            numbering_xml: Cow::Borrowed(styles::NUMBERING_XML),
            footer_xml: Cow::Borrowed(styles::FOOTER_XML),
        }
    }
}

/// Write `doc` as a DOCX package with the built-in defaults.
pub fn write_docx(doc: &ExportDoc, meta: &Meta, assets: &Assets) -> Result<Output, ExportError> {
    write_docx_with(doc, meta, assets, &DocxDefaults::builtin())
}

/// Write `doc` as a DOCX package on top of the given package defaults.
pub fn write_docx_with(
    doc: &ExportDoc,
    meta: &Meta,
    assets: &Assets,
    defaults: &DocxDefaults,
) -> Result<Output, ExportError> {
    let mut cx = Ctx::new(meta, assets);
    body::document(&mut cx, doc);

    let mut zip = ZipOut::new();
    zip.deflated("[Content_Types].xml", CONTENT_TYPES.as_bytes())?;
    zip.deflated("_rels/.rels", ROOT_RELS.as_bytes())?;
    zip.deflated("docProps/core.xml", core_xml(meta).as_bytes())?;
    zip.deflated("docProps/app.xml", APP_XML.as_bytes())?;
    zip.deflated("word/document.xml", cx.xml.as_bytes())?;
    zip.deflated("word/_rels/document.xml.rels", cx.rels_xml().as_bytes())?;
    zip.deflated("word/styles.xml", defaults.styles_xml.as_bytes())?;
    zip.deflated("word/numbering.xml", defaults.numbering_xml.as_bytes())?;
    zip.deflated("word/settings.xml", SETTINGS_XML.as_bytes())?;
    zip.deflated("word/footer1.xml", defaults.footer_xml.as_bytes())?;
    Ok(Output {
        bytes: zip.finish()?,
        fallbacks: cx.fallbacks,
    })
}

struct Relationship {
    kind: &'static str,
    target: String,
    external: bool,
}

/// Writer state for one document.
struct Ctx<'a> {
    meta: &'a Meta,
    assets: &'a Assets,
    /// `word/document.xml` as written so far.
    xml: String,
    fallbacks: Fallbacks,
    /// How many quotes enclose the block being written.
    quote_depth: u32,
    /// `rels[i]` has id `rId{i + 1}`.
    rels: Vec<Relationship>,
    /// Hyperlink target to its relationship id.
    hyperlinks: HashMap<String, String>,
    /// Last drawing id handed out; ids are document-wide.
    last_doc_pr: u32,
}

impl<'a> Ctx<'a> {
    fn new(meta: &'a Meta, assets: &'a Assets) -> Self {
        let rels = FIXED_RELS
            .iter()
            .map(|(kind, target)| Relationship {
                kind,
                target: (*target).to_owned(),
                external: false,
            })
            .collect();
        Self {
            meta,
            assets,
            xml: String::new(),
            fallbacks: Fallbacks::default(),
            quote_depth: 0,
            rels,
            hyperlinks: HashMap::new(),
            last_doc_pr: 0,
        }
    }

    fn add_rel(&mut self, kind: &'static str, target: String, external: bool) -> String {
        self.rels.push(Relationship {
            kind,
            target,
            external,
        });
        format!("rId{}", self.rels.len())
    }

    /// Relationship id of an external hyperlink; one per unique target.
    fn hyperlink(&mut self, target: &str) -> String {
        if let Some(id) = self.hyperlinks.get(target) {
            return id.clone();
        }
        let id = self.add_rel("hyperlink", target.to_owned(), true);
        self.hyperlinks.insert(target.to_owned(), id.clone());
        id
    }

    /// The next drawing id (`wp:docPr id`), unique and positive.
    #[expect(dead_code, reason = "pictures take their drawing ids from here")]
    fn doc_pr_id(&mut self) -> u32 {
        self.last_doc_pr += 1;
        self.last_doc_pr
    }

    fn rels_xml(&self) -> String {
        let mut s = String::from(concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        ));
        for (i, rel) in self.rels.iter().enumerate() {
            let mode = if rel.external {
                r#" TargetMode="External""#
            } else {
                ""
            };
            s.push_str(&format!(
                r#"<Relationship Id="rId{}" Type="{REL_TYPE}{}" Target="{}"{mode}/>"#,
                i + 1,
                rel.kind,
                esc_attr(&rel.target),
            ));
        }
        s.push_str("</Relationships>");
        s
    }
}

const CONTENT_TYPES: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
    r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>"#,
    r#"<Default Extension="xml" ContentType="application/xml"/>"#,
    r#"<Default Extension="png" ContentType="image/png"/>"#,
    r#"<Default Extension="jpeg" ContentType="image/jpeg"/>"#,
    r#"<Default Extension="gif" ContentType="image/gif"/>"#,
    r#"<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>"#,
    r#"<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>"#,
    r#"<Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>"#,
    r#"<Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/>"#,
    r#"<Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/>"#,
    r#"<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>"#,
    r#"<Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>"#,
    r#"</Types>"#,
);

const ROOT_RELS: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>"#,
    r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>"#,
    r#"<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>"#,
    r#"</Relationships>"#,
);

const APP_XML: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties">"#,
    r#"<Application>knot</Application></Properties>"#,
);

/// Word 2013+ compatibility mode, so Word doesn't open the file in
/// Compatibility Mode. No `updateFields`: it would prompt on open.
const SETTINGS_XML: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
    r#"<w:defaultTabStop w:val="720"/>"#,
    r#"<w:characterSpacingControl w:val="doNotCompress"/>"#,
    r#"<w:compat><w:compatSetting w:name="compatibilityMode" w:uri="http://schemas.microsoft.com/office/word" w:val="15"/></w:compat>"#,
    r#"</w:settings>"#,
);

/// Core properties: the title and the export date. No creator, so the
/// exporter's name stays out of forwarded files.
fn core_xml(meta: &Meta) -> String {
    let dates = w3cdtf(&meta.exported_on)
        .map(|d| {
            format!(
                concat!(
                    r#"<dcterms:created xsi:type="dcterms:W3CDTF">{d}</dcterms:created>"#,
                    r#"<dcterms:modified xsi:type="dcterms:W3CDTF">{d}</dcterms:modified>"#,
                ),
                d = d
            )
        })
        .unwrap_or_default();
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" "#,
            r#"xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" "#,
            r#"xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">"#,
            r#"<dc:title>{title}</dc:title>{dates}</cp:coreProperties>"#,
        ),
        title = esc_text(&meta.title),
        dates = dates,
    )
}

/// `Meta::exported_on` ("23 Oct 2026", the server's UTC date) as a W3CDTF
/// timestamp at midnight UTC; `None` when it isn't in that form.
fn w3cdtf(date: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = date.split(' ');
    let day: u8 = parts.next()?.parse().ok()?;
    let month = parts.next()?;
    let month = MONTHS.iter().position(|m| *m == month)? + 1;
    let year: u16 = parts.next()?.parse().ok()?;
    (parts.next().is_none() && (1..=31).contains(&day) && year <= 9999)
        .then(|| format!("{year:04}-{month:02}-{day:02}T00:00:00Z"))
}
