mod common;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use common::{unzip, xml};
use knot_export::assets::{Assets, Meta};
use knot_export::docx::{DocxDefaults, write_docx_with};
use knot_export::{Block, ExportDoc, Inline, LinkTarget, Marks, Run, write_docx};
use roxmltree::{Document, Node};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const DC: &str = "http://purl.org/dc/elements/1.1/";
const DCTERMS: &str = "http://purl.org/dc/terms/";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const REL_TYPE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";

const DOC_ID: &str = "0b1c2d3e-4f50-4162-8374-8596a7b8c9d0";
const USER_ID: &str = "7a6b5c4d-3e2f-4101-9283-746556473829";
const HOSTILE: &str = "Q4 <draft> & \"final\" \u{1}😀 שלום";

fn meta() -> Meta {
    Meta {
        title: "Q4 Plan".into(),
        exported_on: "23 Oct 2026".into(),
        base_url: "https://knot.example".into(),
    }
}

/// The parts of an exported package.
struct Docx(HashMap<String, Vec<u8>>);

impl Docx {
    fn part(&self, name: &str) -> &str {
        let bytes = self
            .0
            .get(name)
            .unwrap_or_else(|| panic!("missing part {name}"));
        std::str::from_utf8(bytes).expect("UTF-8 part")
    }
}

struct Rel {
    kind: String,
    target: String,
    external: bool,
}

/// `word/_rels/document.xml.rels` by relationship id.
fn rels(docx: &Docx) -> HashMap<String, Rel> {
    let d = xml(docx.part("word/_rels/document.xml.rels"));
    d.descendants()
        .filter(|n| n.has_tag_name((PKG_REL, "Relationship")))
        .map(|n| {
            let rel = Rel {
                kind: n.attribute("Type").unwrap().to_owned(),
                target: n.attribute("Target").unwrap().to_owned(),
                external: n.attribute("TargetMode") == Some("External"),
            };
            (n.attribute("Id").unwrap().to_owned(), rel)
        })
        .collect()
}

fn export_with(blocks: Vec<Block>, assets: &Assets) -> Docx {
    let out = write_docx(&ExportDoc { blocks }, &meta(), assets).expect("export");
    Docx(unzip(&out.bytes))
}

fn export(blocks: Vec<Block>) -> Docx {
    export_with(blocks, &Assets::default())
}

fn run(text: &str) -> Run {
    Run {
        text: text.into(),
        marks: Marks::default(),
    }
}

fn para(text: &str) -> Block {
    Block::Paragraph(vec![Inline::Run(run(text))])
}

fn link(target: LinkTarget, text: &str) -> Inline {
    Inline::Link {
        target,
        runs: vec![run(text)],
    }
}

fn elements<'a, 'i>(n: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    n.children().filter(Node::is_element).collect()
}

/// First `w:` child element with this local name.
fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name((W, name)))
}

fn wattr<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.attribute((W, name))
}

fn val<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    wattr(n, "val")
}

fn p_style<'a>(p: Node<'a, '_>) -> Option<&'a str> {
    child(p, "pPr")
        .and_then(|ppr| child(ppr, "pStyle"))
        .and_then(val)
}

fn r_style<'a>(r: Node<'a, '_>) -> Option<&'a str> {
    child(r, "rPr")
        .and_then(|rpr| child(rpr, "rStyle"))
        .and_then(val)
}

/// Concatenated `w:t` text below a node.
fn text(n: Node) -> String {
    n.descendants()
        .filter(|d| d.has_tag_name((W, "t")))
        .filter_map(|t| t.text())
        .collect()
}

fn body<'a, 'i>(d: &'a Document<'i>) -> Node<'a, 'i> {
    d.descendants()
        .find(|n| n.has_tag_name((W, "body")))
        .expect("w:body")
}

/// Body children after the title and subtitle, without the closing `sectPr`.
fn content<'a, 'i>(d: &'a Document<'i>) -> Vec<Node<'a, 'i>> {
    let kids = elements(body(d));
    assert_eq!(kids.last().unwrap().tag_name().name(), "sectPr");
    kids[2..kids.len() - 1].to_vec()
}

fn style<'a, 'i>(styles: &'a Document<'i>, id: &str) -> Node<'a, 'i> {
    styles
        .descendants()
        .find(|n| n.has_tag_name((W, "style")) && wattr(*n, "styleId") == Some(id))
        .unwrap_or_else(|| panic!("no style {id}"))
}

/// Value of `w:style/w:name`.
fn style_name<'a>(s: Node<'a, '_>) -> &'a str {
    child(s, "name").and_then(val).expect("style name")
}

/// One of every block and inline this task writes.
fn kitchen_sink() -> Vec<Block> {
    vec![
        Block::Heading {
            level: 2,
            content: vec![Inline::Run(run("Heading"))],
        },
        Block::Paragraph(vec![
            Inline::Run(Run {
                text: "all marks".into(),
                marks: Marks {
                    bold: true,
                    italic: true,
                    underline: true,
                    strike: true,
                    code: false,
                },
            }),
            Inline::Run(Run {
                text: "code".into(),
                marks: Marks {
                    code: true,
                    ..Marks::default()
                },
            }),
            Inline::Break,
            link(LinkTarget::Web("https://a.example/".into()), "web"),
            link(LinkTarget::Doc(DOC_ID.into()), "page"),
        ]),
        Block::Quote(vec![para("quoted")]),
        Block::Code {
            language: "rust".into(),
            text: "fn main() {\n\tprintln!();\n}".into(),
        },
        Block::Rule,
    ]
}

#[test]
fn package_parts_and_relationships_resolve() {
    let docx = export(kitchen_sink());
    let names: HashSet<&str> = docx.0.keys().map(String::as_str).collect();

    let root = xml(docx.part("_rels/.rels"));
    for r in root
        .descendants()
        .filter(|n| n.has_tag_name((PKG_REL, "Relationship")))
    {
        let target = r.attribute("Target").unwrap();
        assert!(names.contains(target), "root target {target} missing");
    }

    let rels = rels(&docx);
    let doc = xml(docx.part("word/document.xml"));
    let ids: Vec<&str> = doc
        .descendants()
        .flat_map(|n| n.attributes())
        .filter(|a| a.namespace() == Some(R))
        .map(|a| a.value())
        .collect();
    assert!(ids.len() >= 3, "footer and two hyperlinks: {ids:?}");
    for id in &ids {
        assert!(rels.contains_key(*id), "{id} not in document.xml.rels");
    }
    for (id, rel) in &rels {
        if !rel.external {
            let part = format!("word/{}", rel.target);
            assert!(names.contains(part.as_str()), "{id} → {part} missing");
        }
    }

    let ct = xml(docx.part("[Content_Types].xml"));
    let overrides: HashSet<&str> = ct
        .descendants()
        .filter(|n| n.has_tag_name((CT, "Override")))
        .map(|n| n.attribute("PartName").unwrap().trim_start_matches('/'))
        .collect();
    let defaults: HashSet<&str> = ct
        .descendants()
        .filter(|n| n.has_tag_name((CT, "Default")))
        .map(|n| n.attribute("Extension").unwrap())
        .collect();
    for part in &overrides {
        assert!(names.contains(part), "override {part} missing");
    }
    for ext in ["png", "jpeg", "gif"] {
        assert!(defaults.contains(ext), "no default for {ext}");
    }
    for name in names.iter().filter(|n| **n != "[Content_Types].xml") {
        let ext = name.rsplit('.').next().unwrap();
        assert!(
            overrides.contains(name) || defaults.contains(ext),
            "{name} has no content type"
        );
    }

    let core = xml(docx.part("docProps/core.xml"));
    let find = |ns: &str, name: &str| {
        core.descendants()
            .find(|n| n.has_tag_name((ns, name)))
            .and_then(|n| n.text())
            .map(str::to_owned)
    };
    assert_eq!(find(DC, "title").as_deref(), Some("Q4 Plan"));
    assert_eq!(
        find(DCTERMS, "created").as_deref(),
        Some("2026-10-23T00:00:00Z")
    );
    assert_eq!(
        find(DCTERMS, "modified").as_deref(),
        Some("2026-10-23T00:00:00Z")
    );
    assert!(!core.descendants().any(|n| n.has_tag_name((DC, "creator"))));

    let app = xml(docx.part("docProps/app.xml"));
    let application = app
        .descendants()
        .find(|n| n.tag_name().name() == "Application")
        .and_then(|n| n.text());
    assert_eq!(application, Some("knot"));
}

#[test]
fn empty_page_has_title_subtitle_and_sectpr() {
    let docx = export(vec![]);
    let d = xml(docx.part("word/document.xml"));
    let kids = elements(body(&d));
    let tags: Vec<&str> = kids.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(tags, ["p", "p", "sectPr"]);

    assert_eq!(p_style(kids[0]), Some("Title"));
    assert_eq!(text(kids[0]), "Q4 Plan");
    assert_eq!(p_style(kids[1]), Some("Subtitle"));
    assert_eq!(text(kids[1]), "Exported from knot · 23 Oct 2026");

    let sect = kids[2];
    let size = child(sect, "pgSz").expect("pgSz");
    assert_eq!(
        (wattr(size, "w"), wattr(size, "h")),
        (Some("11906"), Some("16838"))
    );
    let margins = child(sect, "pgMar").expect("pgMar");
    for side in ["top", "right", "bottom", "left"] {
        assert_eq!(wattr(margins, side), Some("1417"), "margin {side}");
    }
    let footer = child(sect, "footerReference").expect("footerReference");
    assert_eq!(wattr(footer, "type"), Some("default"));
    let rels = rels(&docx);
    let rel = &rels[footer.attribute((R, "id")).unwrap()];
    assert_eq!(rel.kind, format!("{REL_TYPE}footer"));
    assert_eq!(rel.target, "footer1.xml");
}

#[test]
fn headings_use_heading_styles_with_outline_levels() {
    let docx = export(
        (1..=6)
            .map(|level| Block::Heading {
                level,
                content: vec![Inline::Run(run("h"))],
            })
            .collect(),
    );
    let d = xml(docx.part("word/document.xml"));
    let used: Vec<&str> = content(&d).into_iter().filter_map(p_style).collect();
    assert_eq!(
        used,
        [
            "Heading1", "Heading2", "Heading3", "Heading4", "Heading5", "Heading6"
        ]
    );

    let styles = xml(docx.part("word/styles.xml"));
    for level in 1..=6u8 {
        let s = style(&styles, &format!("Heading{level}"));
        assert_eq!(style_name(s), format!("heading {level}"));
        let outline = child(s, "pPr")
            .and_then(|ppr| child(ppr, "outlineLvl"))
            .and_then(val);
        assert_eq!(outline, Some((level - 1).to_string().as_str()));
    }
}

#[test]
fn style_ids_carry_builtin_names() {
    let docx = export(vec![]);
    let styles = xml(docx.part("word/styles.xml"));
    for (id, kind, name) in [
        ("Normal", "paragraph", Some("Normal")),
        ("Title", "paragraph", Some("Title")),
        ("Subtitle", "paragraph", Some("Subtitle")),
        ("Quote", "paragraph", Some("Quote")),
        ("Code", "paragraph", None),
        ("InlineCode", "character", None),
        ("ListParagraph", "paragraph", Some("List Paragraph")),
        ("TableGrid", "table", Some("Table Grid")),
        ("TableHeader", "paragraph", None),
        ("Hyperlink", "character", Some("Hyperlink")),
        ("Caption", "paragraph", Some("caption")),
        ("Footer", "paragraph", Some("footer")),
        ("HorizontalRule", "paragraph", None),
        ("TableNormal", "table", Some("Normal Table")),
    ] {
        let s = style(&styles, id);
        assert_eq!(wattr(s, "type"), Some(kind), "type of {id}");
        if let Some(name) = name {
            assert_eq!(style_name(s), name, "name of {id}");
        }
    }

    let default_table = style(&styles, "TableNormal");
    assert_eq!(wattr(default_table, "default"), Some("1"));
    let caption = child(style(&styles, "Caption"), "rPr").expect("Caption rPr");
    assert!(child(caption, "i").is_some(), "Caption is italic");
    let footer = child(style(&styles, "Footer"), "pPr").expect("Footer pPr");
    assert_eq!(child(footer, "jc").and_then(val), Some("center"));
    let rule = child(style(&styles, "HorizontalRule"), "pPr").expect("rule pPr");
    assert!(
        child(rule, "pBdr")
            .and_then(|b| child(b, "bottom"))
            .is_some()
    );
}

#[test]
fn every_referenced_style_exists() {
    let docx = export(kitchen_sink());
    let styles = xml(docx.part("word/styles.xml"));
    let ids: HashSet<&str> = styles
        .descendants()
        .filter(|n| n.has_tag_name((W, "style")))
        .filter_map(|n| wattr(n, "styleId"))
        .collect();
    let mut seen = HashSet::new();
    for part in ["word/document.xml", "word/footer1.xml"] {
        let d = xml(docx.part(part));
        for n in d.descendants().filter(|n| {
            ["pStyle", "rStyle", "tblStyle"]
                .iter()
                .any(|t| n.has_tag_name((W, *t)))
        }) {
            let id = val(n).expect("style reference");
            assert!(ids.contains(id), "{part} references missing style {id}");
            seen.insert(id.to_owned());
        }
    }
    for id in [
        "Title",
        "Subtitle",
        "Heading2",
        "InlineCode",
        "Hyperlink",
        "Quote",
        "Code",
        "HorizontalRule",
        "Footer",
    ] {
        assert!(seen.contains(id), "kitchen sink should use {id}");
    }
}

/// ECMA-376 (wml.xsd) order of the children Word checks strictly.
const SCHEMA_ORDER: &[(&str, &[&str])] = &[
    (
        "pPr",
        &[
            "pStyle",
            "keepNext",
            "keepLines",
            "pageBreakBefore",
            "framePr",
            "widowControl",
            "numPr",
            "suppressLineNumbers",
            "pBdr",
            "shd",
            "tabs",
            "suppressAutoHyphens",
            "kinsoku",
            "wordWrap",
            "overflowPunct",
            "topLinePunct",
            "autoSpaceDE",
            "autoSpaceDN",
            "bidi",
            "adjustRightInd",
            "snapToGrid",
            "spacing",
            "ind",
            "contextualSpacing",
            "mirrorIndents",
            "suppressOverlap",
            "jc",
            "textDirection",
            "textAlignment",
            "textboxTightWrap",
            "outlineLvl",
            "divId",
            "cnfStyle",
            "rPr",
            "sectPr",
            "pPrChange",
        ],
    ),
    (
        "rPr",
        &[
            "rStyle",
            "rFonts",
            "b",
            "bCs",
            "i",
            "iCs",
            "caps",
            "smallCaps",
            "strike",
            "dstrike",
            "outline",
            "shadow",
            "emboss",
            "imprint",
            "noProof",
            "snapToGrid",
            "vanish",
            "webHidden",
            "color",
            "spacing",
            "w",
            "kern",
            "position",
            "sz",
            "szCs",
            "highlight",
            "u",
            "effect",
            "bdr",
            "shd",
            "fitText",
            "vertAlign",
            "rtl",
            "cs",
            "em",
            "lang",
            "eastAsianLayout",
            "specVanish",
            "oMath",
        ],
    ),
    (
        "style",
        &[
            "name",
            "aliases",
            "basedOn",
            "next",
            "link",
            "autoRedefine",
            "hidden",
            "uiPriority",
            "semiHidden",
            "unhideWhenUsed",
            "qFormat",
            "locked",
            "personal",
            "personalCompose",
            "personalReply",
            "rsid",
            "pPr",
            "rPr",
            "tblPr",
            "trPr",
            "tcPr",
            "tblStylePr",
        ],
    ),
    (
        "tblPr",
        &[
            "tblStyle",
            "tblpPr",
            "tblOverlap",
            "bidiVisual",
            "tblStyleRowBandSize",
            "tblStyleColBandSize",
            "tblW",
            "jc",
            "tblCellSpacing",
            "tblInd",
            "tblBorders",
            "shd",
            "tblLayout",
            "tblCellMar",
            "tblLook",
            "tblCaption",
            "tblDescription",
        ],
    ),
    (
        "tblBorders",
        &[
            "top", "left", "start", "bottom", "right", "end", "insideH", "insideV",
        ],
    ),
    (
        "tblCellMar",
        &["top", "left", "start", "bottom", "right", "end"],
    ),
    (
        "pBdr",
        &["top", "left", "bottom", "right", "between", "bar"],
    ),
    (
        "sectPr",
        &[
            "headerReference",
            "footerReference",
            "footnotePr",
            "endnotePr",
            "type",
            "pgSz",
            "pgMar",
            "paperSrc",
            "pgBorders",
            "lnNumType",
            "pgNumType",
            "cols",
            "formProt",
            "vAlign",
            "noEndnote",
            "titlePg",
            "textDirection",
            "bidi",
            "rtlGutter",
            "docGrid",
            "printerSettings",
            "sectPrChange",
        ],
    ),
];

#[test]
fn properties_follow_schema_order() {
    let docx = export(kitchen_sink());
    for part in [
        "word/document.xml",
        "word/styles.xml",
        "word/footer1.xml",
        "word/settings.xml",
    ] {
        let d = xml(docx.part(part));
        for n in d
            .descendants()
            .filter(|n| n.tag_name().namespace() == Some(W))
        {
            let name = n.tag_name().name();
            let kids = elements(n);
            if matches!(name, "p" | "r") {
                for (i, k) in kids.iter().enumerate() {
                    let props = k.tag_name().name();
                    assert!(
                        !matches!(props, "pPr" | "rPr") || i == 0,
                        "{part}: {props} is not the first child of {name}"
                    );
                }
            }
            let Some((_, order)) = SCHEMA_ORDER.iter().find(|(p, _)| *p == name) else {
                continue;
            };
            let mut last = None;
            for k in kids {
                let kname = k.tag_name().name();
                let pos = order
                    .iter()
                    .position(|o| *o == kname)
                    .unwrap_or_else(|| panic!("{part}: unexpected {kname} in {name}"));
                assert!(
                    last.is_none_or(|l| l < pos),
                    "{part}: {kname} out of schema order in {name}"
                );
                last = Some(pos);
            }
        }
    }
}

#[test]
fn runs_marks_and_breaks() {
    let docx = export(vec![Block::Paragraph(vec![
        Inline::Run(Run {
            text: "loud".into(),
            marks: Marks {
                bold: true,
                italic: true,
                underline: true,
                strike: true,
                code: false,
            },
        }),
        Inline::Run(Run {
            text: "x = 1".into(),
            marks: Marks {
                code: true,
                ..Marks::default()
            },
        }),
        Inline::Break,
        Inline::Run(run(" a\tb ")),
    ])]);
    let d = xml(docx.part("word/document.xml"));
    let p = content(&d)[0];
    let runs: Vec<Node> = p.children().filter(|n| n.has_tag_name((W, "r"))).collect();
    assert_eq!(runs.len(), 4);

    let rpr = child(runs[0], "rPr").expect("marks");
    for mark in ["b", "i", "strike", "u"] {
        assert!(child(rpr, mark).is_some(), "missing {mark}");
    }
    assert_eq!(child(rpr, "u").and_then(val), Some("single"));
    assert_eq!(text(runs[0]), "loud");

    assert_eq!(r_style(runs[1]), Some("InlineCode"));
    assert!(child(runs[2], "br").is_some());

    let pieces: Vec<(&str, &str)> = elements(runs[3])
        .into_iter()
        .map(|n| (n.tag_name().name(), n.text().unwrap_or("")))
        .collect();
    assert_eq!(pieces, [("t", " a"), ("tab", ""), ("t", "b ")]);

    for t in d.descendants().filter(|n| n.has_tag_name((W, "t"))) {
        assert_eq!(t.attribute((XML_NS, "space")), Some("preserve"));
    }
}

#[test]
fn links_become_hyperlinks_with_external_targets() {
    let mut assets = Assets::default();
    assets.user_names.insert(USER_ID.into(), "Ada".into());
    let web = "https://a.example/x?y=1&z";
    let docx = export_with(
        vec![Block::Paragraph(vec![
            link(LinkTarget::Web(web.into()), "web"),
            Inline::Run(run(" ")),
            link(LinkTarget::Doc("d1".into()), "page"),
            link(LinkTarget::Web(web.into()), "again"),
            link(LinkTarget::User(USER_ID.into()), "@Old Name"),
            link(LinkTarget::User("gone".into()), "@Gone"),
        ])],
        &assets,
    );
    let rels = rels(&docx);
    let d = xml(docx.part("word/document.xml"));
    let links: Vec<Node> = d
        .descendants()
        .filter(|n| n.has_tag_name((W, "hyperlink")))
        .collect();
    let ids: Vec<&str> = links
        .iter()
        .map(|l| l.attribute((R, "id")).expect("r:id"))
        .collect();
    let targets: Vec<&str> = ids.iter().map(|id| rels[*id].target.as_str()).collect();
    assert_eq!(targets, [web, "https://knot.example/doc/d1", web]);
    assert_eq!(ids[0], ids[2], "one relationship per target");
    assert_ne!(ids[0], ids[1]);
    for id in &ids {
        assert_eq!(rels[*id].kind, format!("{REL_TYPE}hyperlink"));
        assert!(rels[*id].external);
    }
    assert!(
        docx.part("word/_rels/document.xml.rels")
            .contains("x?y=1&amp;z")
    );
    for r in links
        .iter()
        .flat_map(|l| l.children().filter(|n| n.has_tag_name((W, "r"))))
    {
        assert_eq!(r_style(r), Some("Hyperlink"));
    }

    let p = content(&d)[0];
    assert_eq!(text(p), "web pageagain@Ada@Gone");
    for t in p.descendants().filter(|n| n.text() == Some("@Ada")) {
        assert!(
            !t.ancestors().any(|a| a.has_tag_name((W, "hyperlink"))),
            "mentions are plain text"
        );
    }
}

#[test]
fn unsafe_links_have_no_relationship() {
    let docx = export(vec![Block::Paragraph(vec![link(
        LinkTarget::Plain,
        "knot://time/2026-10-23",
    )])]);
    let d = xml(docx.part("word/document.xml"));
    assert!(!d.descendants().any(|n| n.has_tag_name((W, "hyperlink"))));
    let p = content(&d)[0];
    let r = child(p, "r").expect("plain run");
    assert_eq!(text(r), "knot://time/2026-10-23");
    assert_eq!(r_style(r), None);

    let mut kinds: Vec<String> = rels(&docx).into_values().map(|r| r.kind).collect();
    kinds.sort();
    let expected: Vec<String> = ["footer", "numbering", "settings", "styles"]
        .iter()
        .map(|k| format!("{REL_TYPE}{k}"))
        .collect();
    assert_eq!(kinds, expected);
}

#[test]
fn code_block_is_one_shaded_paragraph() {
    let docx = export(vec![Block::Code {
        language: "text".into(),
        text: "a\n  b".into(),
    }]);
    let d = xml(docx.part("word/document.xml"));
    let blocks = content(&d);
    assert_eq!(blocks.len(), 1);
    assert_eq!(p_style(blocks[0]), Some("Code"));
    let pieces: Vec<(&str, &str)> = blocks[0]
        .descendants()
        .filter(|n| ["t", "br", "tab"].iter().any(|t| n.has_tag_name((W, *t))))
        .map(|n| (n.tag_name().name(), n.text().unwrap_or("")))
        .collect();
    assert_eq!(pieces, [("t", "a"), ("br", ""), ("t", "  b")]);

    let styles = xml(docx.part("word/styles.xml"));
    let code = style(&styles, "Code");
    let shd = child(code, "pPr")
        .and_then(|p| child(p, "shd"))
        .expect("shaded");
    assert!(wattr(shd, "fill").is_some_and(|f| f != "auto"));
    let font = child(code, "rPr")
        .and_then(|r| child(r, "rFonts"))
        .and_then(|f| wattr(f, "ascii"));
    assert_eq!(font, Some("Consolas"));
}

#[test]
fn quote_and_rule_styles() {
    let docx = export(vec![Block::Quote(vec![para("quoted")]), Block::Rule]);
    let d = xml(docx.part("word/document.xml"));
    let blocks = content(&d);
    assert_eq!(blocks.len(), 2);
    assert_eq!(p_style(blocks[0]), Some("Quote"));
    assert_eq!(text(blocks[0]), "quoted");
    assert_eq!(p_style(blocks[1]), Some("HorizontalRule"));
    assert!(child(blocks[1], "r").is_none());

    let styles = xml(docx.part("word/styles.xml"));
    let indent = child(style(&styles, "Quote"), "pPr")
        .and_then(|p| child(p, "ind"))
        .and_then(|i| wattr(i, "left"));
    assert_eq!(indent, Some("567"));
}

#[test]
fn settings_declare_compat_15_and_footer_fields() {
    let docx = export(vec![]);
    let settings = xml(docx.part("word/settings.xml"));
    let compat = settings
        .descendants()
        .find(|n| {
            n.has_tag_name((W, "compatSetting")) && wattr(*n, "name") == Some("compatibilityMode")
        })
        .expect("compatibilityMode");
    assert_eq!(val(compat), Some("15"));
    assert_eq!(
        wattr(compat, "uri"),
        Some("http://schemas.microsoft.com/office/word")
    );
    assert!(
        !settings
            .descendants()
            .any(|n| n.has_tag_name((W, "updateFields")))
    );

    let footer = xml(docx.part("word/footer1.xml"));
    let instr: Vec<&str> = footer
        .descendants()
        .filter(|n| n.has_tag_name((W, "instrText")))
        .filter_map(|n| n.text())
        .collect();
    assert_eq!(instr, [" PAGE ", " NUMPAGES "]);
    let begins = footer
        .descendants()
        .filter(|n| n.has_tag_name((W, "fldChar")) && wattr(*n, "fldCharType") == Some("begin"))
        .count();
    assert_eq!(begins, 2);
    let p = footer
        .descendants()
        .find(|n| n.has_tag_name((W, "p")))
        .expect("footer paragraph");
    assert_eq!(p_style(p), Some("Footer"));
    assert_eq!(text(p), "Page 1 of 1");
}

#[test]
fn text_with_xml_specials_and_controls_is_well_formed() {
    let mut assets = Assets::default();
    assets.user_names.insert(USER_ID.into(), HOSTILE.into());
    let doc = ExportDoc {
        blocks: vec![
            para(HOSTILE),
            Block::Heading {
                level: 1,
                content: vec![Inline::Run(run(HOSTILE))],
            },
            Block::Code {
                language: HOSTILE.into(),
                text: HOSTILE.into(),
            },
            Block::Paragraph(vec![
                link(
                    LinkTarget::Web("https://a.example/?q=<a>&b=\"c\"".into()),
                    HOSTILE,
                ),
                link(LinkTarget::User(USER_ID.into()), "@x"),
            ]),
        ],
    };
    let meta = Meta {
        title: HOSTILE.into(),
        ..meta()
    };
    let out = write_docx(&doc, &meta, &assets).expect("export");
    let docx = Docx(unzip(&out.bytes));
    for name in docx.0.keys() {
        if name.ends_with(".xml") || name.ends_with(".rels") {
            xml(docx.part(name));
        }
    }

    let clean = HOSTILE.replace('\u{1}', "");
    let core = xml(docx.part("docProps/core.xml"));
    let title = core
        .descendants()
        .find(|n| n.has_tag_name((DC, "title")))
        .and_then(|n| n.text());
    assert_eq!(title, Some(clean.as_str()));
    let d = xml(docx.part("word/document.xml"));
    let kids = elements(body(&d));
    assert_eq!(text(kids[0]), clean);
    let texts: Vec<String> = content(&d).into_iter().map(text).collect();
    assert_eq!(
        texts,
        [
            clean.clone(),
            clean.clone(),
            clean.clone(),
            format!("{clean}@{clean}")
        ]
    );
    let rels = rels(&docx);
    assert!(
        rels.values()
            .any(|r| r.target == "https://a.example/?q=<a>&b=\"c\"")
    );
}

#[test]
fn package_defaults_are_written_as_given() {
    let builtin = DocxDefaults::builtin();
    let defaults = DocxDefaults {
        styles_xml: Cow::Owned(builtin.styles_xml.replace("Calibri", "Arial")),
        numbering_xml: builtin.numbering_xml.clone(),
        footer_xml: Cow::Owned(builtin.footer_xml.replace("Page ", "Seite ")),
    };
    let out = write_docx_with(
        &ExportDoc {
            blocks: vec![para("x")],
        },
        &meta(),
        &Assets::default(),
        &defaults,
    )
    .expect("export");
    let docx = Docx(unzip(&out.bytes));
    assert_eq!(docx.part("word/styles.xml"), defaults.styles_xml);
    assert_eq!(docx.part("word/footer1.xml"), defaults.footer_xml);
    assert_eq!(docx.part("word/numbering.xml"), defaults.numbering_xml);
    assert!(docx.part("word/styles.xml").contains("Arial"));
}
