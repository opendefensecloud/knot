mod common;

use std::borrow::Cow;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use common::docx::{
    Docx, PKG_REL, SCHEMA_ORDER, W, body, child, children, content, elements, grid_cols, grid_span,
    p_style, r_style, rels, style, style_name, tables, tbl_w, tc_prop, tc_w, text, v_merge, val,
    wattr,
};
use common::{unzip, xml};
use knot_export::assets::{Assets, Fallbacks, ImageAsset, Meta, PicFormat, Picture};
use knot_export::docx::{DocxDefaults, write_docx_with};
use knot_export::{
    Align, Block, Cell, ExportDoc, ImageSource, Inline, LinkTarget, List, ListItem, Marks, Run,
    Table, write_docx,
};
use roxmltree::Node;

const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
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
    let mut blocks = kitchen_sink();
    blocks.push(sample_table());
    let docx = export(blocks);
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
        "TableGrid",
        "TableHeader",
    ] {
        assert!(seen.contains(id), "kitchen sink should use {id}");
    }
}

#[test]
fn properties_follow_schema_order() {
    let mut blocks = kitchen_sink();
    blocks.push(list(
        true,
        1,
        vec![item(vec![para("a"), code("b")]), check_item(true, "c")],
    ));
    blocks.push(sample_table());
    let docx = export(blocks);
    for part in [
        "word/document.xml",
        "word/numbering.xml",
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
            let props = [
                ("p", "pPr"),
                ("r", "rPr"),
                ("tbl", "tblPr"),
                ("tr", "trPr"),
                ("tc", "tcPr"),
            ]
            .iter()
            .find(|(el, _)| *el == name);
            if let Some((_, props)) = props {
                for (i, k) in kids.iter().enumerate() {
                    assert!(
                        k.tag_name().name() != *props || i == 0,
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

fn list(ordered: bool, start: u32, items: Vec<ListItem>) -> Block {
    Block::List(List {
        ordered,
        start,
        items,
    })
}

fn item(blocks: Vec<Block>) -> ListItem {
    ListItem {
        checked: None,
        blocks,
    }
}

fn check_item(checked: bool, text: &str) -> ListItem {
    ListItem {
        checked: Some(checked),
        blocks: vec![para(text)],
    }
}

fn code(text: &str) -> Block {
    Block::Code {
        language: String::new(),
        text: text.into(),
    }
}

/// `(numId, ilvl)` of a paragraph's `w:numPr`.
fn num_pr(p: Node) -> Option<(String, u32)> {
    let num_pr = child(child(p, "pPr")?, "numPr")?;
    let ilvl = child(num_pr, "ilvl").and_then(val)?.parse().unwrap();
    let id = child(num_pr, "numId").and_then(val)?.to_owned();
    Some((id, ilvl))
}

/// The `w:ind w:left` of a paragraph.
fn ind_left(p: Node) -> Option<u32> {
    let ind = child(child(p, "pPr")?, "ind")?;
    wattr(ind, "left").map(|v| v.parse().unwrap())
}

/// A `w:num` of numbering.xml.
struct Num {
    id: String,
    abstract_id: String,
    /// `(ilvl, startOverride)`.
    over: Option<(u32, u32)>,
}

fn nums(docx: &Docx) -> Vec<Num> {
    let d = xml(docx.part("word/numbering.xml"));
    d.descendants()
        .filter(|n| n.has_tag_name((W, "num")))
        .map(|n| Num {
            id: wattr(n, "numId").unwrap().to_owned(),
            abstract_id: child(n, "abstractNumId").and_then(val).unwrap().to_owned(),
            over: child(n, "lvlOverride").map(|o| {
                let start = child(o, "startOverride").and_then(val).unwrap();
                (
                    wattr(o, "ilvl").unwrap().parse().unwrap(),
                    start.parse().unwrap(),
                )
            }),
        })
        .collect()
}

#[test]
fn each_ordered_list_restarts_with_its_start() {
    let docx = export(vec![
        list(true, 1, vec![item(vec![para("a")])]),
        para("between"),
        list(true, 5, vec![item(vec![para("b")])]),
    ]);
    let d = xml(docx.part("word/document.xml"));
    let ps = content(&d);
    let (first, _) = num_pr(ps[0]).expect("first list item numbered");
    let (second, _) = num_pr(ps[2]).expect("second list item numbered");
    assert_ne!(first, second);
    let nums = nums(&docx);
    let find = |id: &str| nums.iter().find(|n| n.id == id).unwrap();
    assert_eq!(find(&first).abstract_id, "1");
    assert_eq!(
        find(&first).over,
        Some((0, 1)),
        "start 1 is still overridden"
    );
    assert_eq!(find(&second).over, Some((0, 5)));
}

#[test]
fn nested_mixed_lists_use_depth_as_ilvl() {
    let docx = export(vec![list(
        false,
        1,
        vec![item(vec![
            para("a"),
            list(
                true,
                3,
                vec![item(vec![
                    para("b"),
                    list(false, 1, vec![item(vec![para("c")])]),
                ])],
            ),
        ])],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let ps = content(&d);
    assert_eq!(ps.len(), 3);
    let nums = nums(&docx);
    let abstract_of = |p: Node| {
        let (id, ilvl) = num_pr(p).expect("numbered");
        let n = nums.iter().find(|n| n.id == id).unwrap();
        (n.abstract_id.clone(), ilvl, n.over)
    };
    assert_eq!(abstract_of(ps[0]), ("0".into(), 0, None));
    assert_eq!(abstract_of(ps[1]), ("1".into(), 1, Some((1, 3))));
    assert_eq!(abstract_of(ps[2]), ("0".into(), 2, None));
    for p in &ps {
        assert_eq!(p_style(*p), Some("ListParagraph"));
    }
}

#[test]
fn checklist_items_use_box_characters() {
    let docx = export(vec![list(
        false,
        1,
        vec![check_item(true, "done"), check_item(false, "todo")],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let ps = content(&d);
    assert_eq!(ps.len(), 2);
    assert_eq!(text(ps[0]), "\u{2611} done");
    assert_eq!(text(ps[1]), "\u{2610} todo");
    for p in &ps {
        assert_eq!(num_pr(*p), None);
        assert_eq!(p_style(*p), Some("ListParagraph"));
        assert_eq!(ind_left(*p), Some(720));
    }
    let first_t = ps[0]
        .descendants()
        .find(|n| n.has_tag_name((W, "t")))
        .unwrap();
    assert!(first_t.text().unwrap().starts_with("\u{2611} "));
}

#[test]
fn nested_checklist_indents_by_depth() {
    let docx = export(vec![list(
        false,
        1,
        vec![item(vec![
            para("plain"),
            list(false, 1, vec![check_item(true, "inner")]),
        ])],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let ps = content(&d);
    assert_eq!(ind_left(ps[1]), Some(1440));
    assert_eq!(num_pr(ps[1]), None);
}

#[test]
fn deep_list_clamps_to_level_9() {
    let mut inner = vec![item(vec![para("leaf")])];
    for _ in 0..11 {
        inner = vec![item(vec![para("x"), list(false, 1, inner)])];
    }
    let docx = export(vec![list(false, 1, inner)]);
    let d = xml(docx.part("word/document.xml"));
    let levels: Vec<u32> = content(&d)
        .into_iter()
        .map(|p| num_pr(p).expect("numbered").1)
        .collect();
    assert_eq!(levels.len(), 12);
    assert_eq!(levels.iter().max(), Some(&8));
    assert_eq!(levels[..9], [0, 1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(levels[9..].iter().all(|l| *l == 8));
}

#[test]
fn list_item_with_several_blocks() {
    let docx = export(vec![list(
        false,
        1,
        vec![item(vec![para("lead"), code("let x = 1;"), para("tail")])],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let ps = content(&d);
    assert_eq!(ps.len(), 3);
    assert!(num_pr(ps[0]).is_some());
    assert_eq!(p_style(ps[1]), Some("Code"));
    assert_eq!(num_pr(ps[1]), None);
    assert_eq!(ind_left(ps[1]), Some(720));
    assert_eq!(p_style(ps[2]), Some("ListParagraph"));
    assert_eq!(num_pr(ps[2]), None);
    assert_eq!(ind_left(ps[2]), Some(720));
}

#[test]
fn item_without_leading_paragraph_still_has_a_marker() {
    let docx = export(vec![list(true, 1, vec![item(vec![code("x")])])]);
    let d = xml(docx.part("word/document.xml"));
    let ps = content(&d);
    assert_eq!(ps.len(), 2);
    assert!(num_pr(ps[0]).is_some());
    assert_eq!(text(ps[0]), "");
    assert_eq!(p_style(ps[1]), Some("Code"));
}

#[test]
fn every_num_id_exists_in_numbering() {
    let docx = export(vec![
        list(
            false,
            1,
            vec![item(vec![
                para("a"),
                list(true, 2, vec![item(vec![para("b")])]),
            ])],
        ),
        list(true, 1, vec![item(vec![para("c")])]),
    ]);
    let d = xml(docx.part("word/document.xml"));
    let used: HashSet<String> = d
        .descendants()
        .filter(|n| n.has_tag_name((W, "numId")))
        .filter_map(|n| val(n).map(str::to_owned))
        .collect();
    assert_eq!(used.len(), 3);
    let defined: Vec<String> = nums(&docx).into_iter().map(|n| n.id).collect();
    let unique: HashSet<&String> = defined.iter().collect();
    assert_eq!(unique.len(), defined.len(), "numIds are unique");
    for id in &used {
        assert!(defined.contains(id), "numId {id} has no w:num");
    }
}

#[test]
fn numbering_defines_both_abstract_nums_before_any_num() {
    let docx = export(vec![list(true, 1, vec![item(vec![para("a")])])]);
    let d = xml(docx.part("word/numbering.xml"));
    let kids = elements(d.root_element());
    let tags: Vec<&str> = kids.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(tags, ["abstractNum", "abstractNum", "num"]);
    let lvls_of = |abs: Node<'_, '_>| -> Vec<String> {
        elements(abs)
            .into_iter()
            .filter(|n| n.has_tag_name((W, "lvl")))
            .map(|l| child(l, "lvlText").and_then(val).unwrap().to_owned())
            .collect()
    };
    let bullets = lvls_of(kids[0]);
    assert_eq!(bullets.len(), 9);
    assert_eq!(
        bullets[..4],
        ["\u{2022}", "\u{25E6}", "\u{25AA}", "\u{2022}"]
    );
    let decimals = lvls_of(kids[1]);
    assert_eq!(decimals.len(), 9);
    assert_eq!(decimals[0], "%1.");
    assert_eq!(decimals[8], "%9.");
    let lvl2 = elements(kids[1])
        .into_iter()
        .filter(|n| n.has_tag_name((W, "lvl")))
        .nth(2)
        .unwrap();
    let ind = child(child(lvl2, "pPr").unwrap(), "ind").unwrap();
    assert_eq!(wattr(ind, "left"), Some("2160"));
    assert_eq!(wattr(ind, "hanging"), Some("360"));
}

fn cell(col: usize, blocks: Vec<Block>) -> Cell {
    Cell {
        col,
        colspan: 1,
        rowspan: 1,
        header: false,
        align: Align::Left,
        blocks,
    }
}

fn td(col: usize, text: &str) -> Cell {
    cell(col, vec![para(text)])
}

fn th(col: usize, text: &str) -> Cell {
    Cell {
        header: true,
        ..td(col, text)
    }
}

fn table(shares: Vec<f64>, rows: Vec<Vec<Cell>>) -> Block {
    Block::Table(Table { shares, rows })
}

/// `n` equal column shares.
fn even(n: usize) -> Vec<f64> {
    vec![1.0 / n as f64; n]
}

/// A table with a header row, both kinds of span and a centred cell.
fn sample_table() -> Block {
    table(
        even(3),
        vec![
            vec![th(0, "h1"), th(1, "h2"), th(2, "h3")],
            vec![
                Cell {
                    colspan: 2,
                    rowspan: 2,
                    align: Align::Center,
                    ..td(0, "merged")
                },
                td(2, "x"),
            ],
            vec![td(2, "y")],
        ],
    )
}

fn rows<'a, 'i>(tbl: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    children(tbl, "tr")
}

fn cells<'a, 'i>(tr: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    children(tr, "tc")
}

/// Each row's cells as "text, span N, restart|continue", leaving out what a
/// cell doesn't have.
fn shape(tbl: Node) -> Vec<Vec<String>> {
    rows(tbl)
        .into_iter()
        .map(|tr| {
            cells(tr)
                .into_iter()
                .map(|tc| {
                    let mut parts = vec![text(tc)];
                    if grid_span(tc) > 1 {
                        parts.push(format!("span {}", grid_span(tc)));
                    }
                    parts.extend(v_merge(tc).map(str::to_owned));
                    parts.retain(|p| !p.is_empty());
                    parts.join(", ")
                })
                .collect()
        })
        .collect()
}

/// The `w:jc` of a paragraph.
fn jc<'a>(p: Node<'a, '_>) -> Option<&'a str> {
    child(child(p, "pPr")?, "jc").and_then(val)
}

#[test]
fn grid_columns_follow_shares_and_sum_to_text_width() {
    let docx = export(vec![
        table(vec![0.3, 0.7], vec![vec![td(0, "a"), td(1, "b")]]),
        table(even(11), vec![vec![td(0, "c")]]),
    ]);
    let d = xml(docx.part("word/document.xml"));
    let tbls = tables(&d);
    assert_eq!(tbls.len(), 2);

    let pr = child(tbls[0], "tblPr").expect("tblPr");
    assert_eq!(child(pr, "tblStyle").and_then(val), Some("TableGrid"));
    assert_eq!(tbl_w(tbls[0]), 9072);
    let layout = child(pr, "tblLayout").and_then(|l| wattr(l, "type"));
    assert_eq!(layout, Some("fixed"));
    assert_eq!(grid_cols(tbls[0]), [2722, 6350]);
    let widths: Vec<u32> = cells(rows(tbls[0])[0]).into_iter().map(tc_w).collect();
    assert_eq!(widths, [2722, 6350]);

    let cols = grid_cols(tbls[1]);
    assert_eq!(cols[..10], [825; 10]);
    assert_eq!(
        cols[10], 822,
        "the rounding remainder goes on the last column"
    );
    assert_eq!(cols.iter().sum::<u32>(), 9072);
}

#[test]
fn spans_are_encoded() {
    let docx = export(vec![table(
        even(3),
        vec![
            vec![
                Cell {
                    colspan: 2,
                    rowspan: 2,
                    ..td(0, "A")
                },
                td(2, "B"),
            ],
            vec![Cell {
                rowspan: 2,
                ..td(2, "C")
            }],
            vec![Cell {
                colspan: 2,
                ..td(0, "D")
            }],
        ],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let tbl = tables(&d)[0];
    assert_eq!(
        shape(tbl),
        vec![
            vec!["A, span 2, restart", "B"],
            vec!["span 2, continue", "C, restart"],
            vec!["D, span 2", "continue"],
        ]
    );

    let continuations: Vec<Node> = d
        .descendants()
        .filter(|n| n.has_tag_name((W, "tc")) && v_merge(*n) == Some("continue"))
        .collect();
    assert_eq!(continuations.len(), 2);
    for tc in continuations {
        assert_eq!(
            val(tc_prop(tc, "vMerge").unwrap()),
            None,
            "a bare <w:vMerge/>"
        );
        let kids = elements(tc);
        let tags: Vec<&str> = kids.iter().map(|k| k.tag_name().name()).collect();
        assert_eq!(tags, ["tcPr", "p"]);
        assert!(elements(kids[1]).is_empty(), "an empty <w:p/>");
    }
    assert_eq!(tc_w(cells(rows(tbl)[0])[0]), 6048);
    assert_eq!(tc_w(cells(rows(tbl)[1])[0]), 6048, "as wide as its restart");
}

#[test]
fn spans_are_clamped_to_the_table() {
    let docx = export(vec![
        table(
            even(2),
            vec![
                vec![
                    Cell {
                        rowspan: 5,
                        ..td(0, "A")
                    },
                    Cell {
                        colspan: 4,
                        ..td(1, "B")
                    },
                ],
                vec![td(1, "C")],
            ],
        ),
        // A span that runs into a merged cell stops short of it.
        table(
            even(3),
            vec![
                vec![
                    td(0, "a"),
                    Cell {
                        rowspan: 2,
                        ..td(1, "b")
                    },
                    td(2, "c"),
                ],
                vec![Cell {
                    colspan: 3,
                    ..td(0, "wide")
                }],
            ],
        ),
    ]);
    let d = xml(docx.part("word/document.xml"));
    let tbls = tables(&d);
    assert_eq!(
        shape(tbls[0]),
        vec![vec!["A, restart", "B"], vec!["continue", "C"]]
    );
    assert_eq!(tc_w(cells(rows(tbls[0])[0])[1]), 4536);
    assert_eq!(
        shape(tbls[1]),
        vec![vec!["a", "b, restart", "c"], vec!["wide", "continue", ""]]
    );
}

#[test]
fn header_rows_repeat_and_are_shaded() {
    let docx = export(vec![table(
        even(2),
        vec![
            vec![
                Cell {
                    rowspan: 2,
                    ..th(0, "H1")
                },
                th(1, "H2"),
            ],
            vec![td(1, "body")],
            vec![th(0, "side"), td(1, "x")],
        ],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let trs = rows(tables(&d)[0]);
    let repeats: Vec<bool> = trs
        .iter()
        .map(|tr| {
            child(*tr, "trPr")
                .and_then(|p| child(p, "tblHeader"))
                .is_some()
        })
        .collect();
    assert_eq!(repeats, [true, false, false]);

    let tcs: Vec<Node> = trs.iter().flat_map(|tr| cells(*tr)).collect();
    let expected = [
        ("H1", true, Some("TableHeader")),
        ("H2", true, Some("TableHeader")),
        ("", true, None),
        ("body", false, None),
        ("side", true, Some("TableHeader")),
        ("x", false, None),
    ];
    assert_eq!(tcs.len(), expected.len());
    for (tc, (label, shaded, style)) in tcs.iter().zip(expected) {
        assert_eq!(text(*tc), label);
        let shd = tc_prop(*tc, "shd");
        assert_eq!(shd.is_some(), shaded, "shading of {label:?}");
        if let Some(shd) = shd {
            assert_eq!(wattr(shd, "val"), Some("clear"));
            assert_eq!(wattr(shd, "fill"), Some("F2F2F2"));
        }
        let p_styles: Vec<Option<&str>> = children(*tc, "p").into_iter().map(p_style).collect();
        assert_eq!(p_styles, [style], "paragraph style in {label:?}");
    }
}

#[test]
fn rows_are_padded_and_cells_end_with_paragraph() {
    let nested = table(even(2), vec![vec![td(0, "in"), td(1, "ner")]]);
    let docx = export(vec![table(
        even(3),
        vec![
            vec![td(0, "a"), td(1, "b"), td(2, "c")],
            vec![td(0, "short")],
            vec![td(0, "x"), td(2, "z")],
            vec![cell(0, vec![nested]), cell(1, vec![])],
        ],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let tbl = tables(&d)[0];
    assert_eq!(
        shape(tbl),
        vec![
            vec!["a", "b", "c"],
            vec!["short", "span 2"],
            vec!["x", "", "z"],
            vec!["inner", "", ""],
        ]
    );
    for tc in d.descendants().filter(|n| n.has_tag_name((W, "tc"))) {
        let last = elements(tc).pop().unwrap();
        assert!(
            last.has_tag_name((W, "p")),
            "{} ends with a paragraph",
            text(tc)
        );
    }
    let holder = cells(rows(tbl)[3])[0];
    let tags: Vec<&str> = elements(holder)
        .iter()
        .map(|n| n.tag_name().name())
        .collect();
    assert_eq!(tags, ["tcPr", "tbl", "p"]);
}

#[test]
fn adjacent_tables_are_separated() {
    let t = || table(even(1), vec![vec![td(0, "t")]]);
    let docx = export(vec![
        t(),
        t(),
        table(vec![], vec![]),
        t(),
        para("after"),
        t(),
        table(even(1), vec![vec![cell(0, vec![t(), t()])]]),
    ]);
    let d = xml(docx.part("word/document.xml"));
    let blocks = content(&d);
    let tags: Vec<&str> = blocks.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(
        tags,
        ["tbl", "p", "tbl", "p", "tbl", "p", "tbl", "p", "tbl"]
    );
    for sep in [blocks[1], blocks[3], blocks[7]] {
        assert!(elements(sep).is_empty(), "an empty separator paragraph");
    }
    assert_eq!(text(blocks[5]), "after");

    let holder = cells(rows(blocks[8])[0])[0];
    let tags: Vec<&str> = elements(holder)
        .iter()
        .map(|n| n.tag_name().name())
        .collect();
    assert_eq!(tags, ["tcPr", "tbl", "p", "tbl", "p"]);
}

#[test]
fn nested_table_fits_its_cell() {
    let inner = || table(vec![0.3, 0.7], vec![vec![td(0, "i"), td(1, "j")]]);
    let docx = export(vec![table(
        vec![0.5, 0.5],
        vec![
            vec![cell(0, vec![inner()]), td(1, "b")],
            vec![Cell {
                colspan: 2,
                ..cell(0, vec![inner()])
            }],
        ],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let tbls = tables(&d);
    assert_eq!(tbls.len(), 3);
    assert_eq!(tbl_w(tbls[0]), 9072);
    assert_eq!(grid_cols(tbls[0]), [4536, 4536]);
    for (tbl, width) in [(tbls[1], 4536 - 216), (tbls[2], 9072 - 216)] {
        assert_eq!(tbl_w(tbl), width);
        assert_eq!(grid_cols(tbl).iter().sum::<u32>(), width);
    }
    assert_eq!(grid_cols(tbls[1]), [1296, 3024]);
}

#[test]
fn cell_align_sets_paragraph_jc() {
    let docx = export(vec![table(
        even(3),
        vec![vec![
            Cell {
                align: Align::Center,
                ..cell(
                    0,
                    vec![para("c"), list(false, 1, vec![item(vec![para("li")])])],
                )
            },
            Cell {
                align: Align::Right,
                ..td(1, "r")
            },
            td(2, "l"),
        ]],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let aligns: Vec<Vec<Option<&str>>> = cells(rows(tables(&d)[0])[0])
        .into_iter()
        .map(|tc| children(tc, "p").into_iter().map(jc).collect())
        .collect();
    assert_eq!(
        aligns,
        vec![
            vec![Some("center"), Some("center")],
            vec![Some("right")],
            vec![None]
        ]
    );
}

#[test]
fn cell_content_starts_at_the_cell_edge() {
    let inner = || {
        table(
            even(1),
            vec![vec![cell(
                0,
                vec![para("cell"), list(false, 1, vec![item(vec![para("li")])])],
            )]],
        )
    };
    let docx = export(vec![
        Block::Quote(vec![inner()]),
        list(false, 1, vec![item(vec![para("lead"), inner()])]),
    ]);
    let d = xml(docx.part("word/document.xml"));
    let tbls = tables(&d);
    assert_eq!(tbl_w(tbls[0]), 9072 - 567);
    assert_eq!(tbl_w(tbls[1]), 9072 - 720);
    for tbl in tbls {
        let ps = children(cells(rows(tbl)[0])[0], "p");
        assert_eq!(text(ps[0]), "cell");
        assert_eq!(p_style(ps[0]), None, "no Quote style inside a cell");
        assert_eq!(ind_left(ps[0]), None, "no list indent inside a cell");
        assert_eq!(text(ps[1]), "li");
        assert_eq!(num_pr(ps[1]).map(|(_, ilvl)| ilvl), Some(0));
    }
}

#[test]
fn empty_tables_write_nothing_and_parse() {
    let docx = export(vec![
        table(vec![], vec![]),
        table(vec![], vec![vec![]]),
        table(vec![1.0], vec![]),
        para("after"),
    ]);
    for name in docx.0.keys() {
        if name.ends_with(".xml") || name.ends_with(".rels") {
            xml(docx.part(name));
        }
    }
    let d = xml(docx.part("word/document.xml"));
    assert!(tables(&d).is_empty());
    let blocks = content(&d);
    assert_eq!(blocks.len(), 1);
    assert_eq!(text(blocks[0]), "after");
}

#[test]
fn large_page_exports_valid_docx_quickly() {
    let mut blocks: Vec<Block> = (0..2000)
        .map(|i| para(&format!("Paragraph {i} of a long page, with some text.")))
        .collect();
    let grid = (0..50)
        .map(|r| (0..20).map(|c| td(c, &format!("r{r}c{c}"))).collect())
        .collect();
    blocks.push(table(even(20), grid));

    let started = Instant::now();
    let out = write_docx(&ExportDoc { blocks }, &meta(), &Assets::default()).expect("export");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "export took {elapsed:?}");

    let docx = Docx(unzip(&out.bytes));
    for name in docx.0.keys() {
        if name.ends_with(".xml") || name.ends_with(".rels") {
            xml(docx.part(name));
        }
    }
    let d = xml(docx.part("word/document.xml"));
    assert_eq!(content(&d).len(), 2001);
    let trs = rows(tables(&d)[0]);
    assert_eq!(trs.len(), 50);
    assert!(trs.iter().all(|tr| cells(*tr).len() == 20));
}

const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const PIC: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";

const BLOB_A: &str = "aaaaaaaa-1111-4222-8333-444444444444";
const BLOB_B: &str = "bbbbbbbb-1111-4222-8333-444444444444";

fn picture(format: PicFormat, width_px: u32, height_px: u32, scale: u32) -> Picture {
    Picture {
        format,
        bytes: vec![1, 2, 3, width_px as u8],
        width_px,
        height_px,
        scale,
    }
}

fn image_block(blob: &str, alt: &str) -> Block {
    Block::Image {
        source: ImageSource::Blob(blob.into()),
        alt: alt.into(),
    }
}

/// Every `wp:extent` in the body as (cx, cy).
fn extents(d: &roxmltree::Document) -> Vec<(u64, u64)> {
    d.descendants()
        .filter(|n| n.has_tag_name((WP, "extent")))
        .map(|n| {
            (
                n.attribute("cx").unwrap().parse().unwrap(),
                n.attribute("cy").unwrap().parse().unwrap(),
            )
        })
        .collect()
}

#[test]
fn embedded_image_has_media_part_and_unique_docpr() {
    let mut assets = Assets::default();
    assets.images.insert(
        BLOB_A.into(),
        ImageAsset::Embedded(picture(PicFormat::Png, 100, 50, 1)),
    );
    assets.images.insert(
        BLOB_B.into(),
        ImageAsset::Embedded(picture(PicFormat::Jpeg, 80, 40, 1)),
    );
    let docx = export_with(
        vec![image_block(BLOB_A, "First <one>"), image_block(BLOB_B, "")],
        &assets,
    );
    assert_eq!(docx.0["word/media/image1.png"], [1, 2, 3, 100]);
    assert_eq!(docx.0["word/media/image2.jpeg"], [1, 2, 3, 80]);

    let ct = xml(docx.part("[Content_Types].xml"));
    for (ext, mime) in [
        ("png", "image/png"),
        ("jpeg", "image/jpeg"),
        ("gif", "image/gif"),
    ] {
        assert!(ct.descendants().any(|n| n.has_tag_name((CT, "Default"))
            && n.attribute("Extension") == Some(ext)
            && n.attribute("ContentType") == Some(mime)));
    }

    let d = xml(docx.part("word/document.xml"));
    let root = d.root_element();
    for (prefix, ns) in [("wp", WP), ("a", A), ("pic", PIC)] {
        assert_eq!(root.lookup_namespace_uri(Some(prefix)), Some(ns));
    }
    let doc_prs: Vec<_> = d
        .descendants()
        .filter(|n| n.has_tag_name((WP, "docPr")))
        .collect();
    assert_eq!(doc_prs.len(), 2);
    let ids: HashSet<u32> = doc_prs
        .iter()
        .map(|n| n.attribute("id").unwrap().parse().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(!ids.contains(&0));
    assert_eq!(doc_prs[0].attribute("descr"), Some("First <one>"));

    let rels = rels(&docx);
    let blips: Vec<_> = d
        .descendants()
        .filter(|n| n.has_tag_name((A, "blip")))
        .map(|n| &rels[n.attribute((R, "embed")).unwrap()])
        .collect();
    assert_eq!(blips.len(), 2);
    assert_eq!(blips[0].target, "media/image1.png");
    assert_eq!(blips[1].target, "media/image2.jpeg");
    assert!(
        blips
            .iter()
            .all(|r| r.kind == format!("{REL_TYPE}image") && !r.external)
    );
    // Word reports corruption without pic:spPr.
    assert_eq!(
        d.descendants()
            .filter(|n| n.has_tag_name((PIC, "spPr")))
            .count(),
        2
    );
    assert_eq!(extents(&d), [(952_500, 476_250), (762_000, 381_000)]);
}

#[test]
fn diagram_picture_is_half_size_and_capped() {
    let mut assets = Assets::default();
    assets
        .diagrams
        .insert("m1".into(), picture(PicFormat::Png, 400, 200, 2));
    assets
        .diagrams
        .insert("m2".into(), picture(PicFormat::Png, 4000, 1000, 2));
    let docx = export_with(
        vec![
            Block::Mermaid {
                key: "m1".into(),
                source: "graph TD".into(),
            },
            Block::Mermaid {
                key: "m2".into(),
                source: "graph LR".into(),
            },
        ],
        &assets,
    );
    let d = xml(docx.part("word/document.xml"));
    assert_eq!(extents(&d), [(1_905_000, 952_500), (5_760_720, 1_440_180)]);
    let descr: Vec<_> = d
        .descendants()
        .filter(|n| n.has_tag_name((WP, "docPr")))
        .map(|n| n.attribute("descr").unwrap())
        .collect();
    assert_eq!(descr, ["Mermaid diagram", "Mermaid diagram"]);
}

#[test]
fn board_picture_is_described_by_its_label() {
    let mut assets = Assets::default();
    assets
        .diagrams
        .insert("b1".into(), picture(PicFormat::Png, 200, 100, 2));
    let docx = export_with(
        vec![Block::Board {
            key: "b1".into(),
            board_id: "x".into(),
            label: "Roadmap".into(),
        }],
        &assets,
    );
    let d = xml(docx.part("word/document.xml"));
    let pr = d
        .descendants()
        .find(|n| n.has_tag_name((WP, "docPr")))
        .unwrap();
    assert_eq!(pr.attribute("descr"), Some("Diagram: Roadmap"));
}

#[test]
fn image_in_table_cell_fits_the_cell() {
    let mut assets = Assets::default();
    assets.images.insert(
        BLOB_A.into(),
        ImageAsset::Embedded(picture(PicFormat::Png, 3000, 1000, 1)),
    );
    let cell = Cell {
        col: 0,
        colspan: 1,
        rowspan: 1,
        header: false,
        align: Align::Left,
        blocks: vec![image_block(BLOB_A, "wide")],
    };
    let docx = export_with(vec![table(vec![0.5, 0.5], vec![vec![cell]])], &assets);
    let d = xml(docx.part("word/document.xml"));
    let max = (4536 - 216) * 635u64;
    let e = extents(&d);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].0, max);
    assert_eq!(e[0].1, max / 3);
}

#[test]
fn tall_image_fits_the_page_height() {
    // A phone screenshot: at the 16 cm text width it would be 34.6 cm
    // tall, past the page. It shrinks to 21.2 cm, which with the Normal
    // paragraph's 1.15 line spacing and spacing after still fits the
    // 24.7 cm of body.
    let mut assets = Assets::default();
    assets.images.insert(
        BLOB_A.into(),
        ImageAsset::Embedded(picture(PicFormat::Png, 1170, 2532, 1)),
    );
    let docx = export_with(vec![image_block(BLOB_A, "screenshot")], &assets);
    let d = xml(docx.part("word/document.xml"));
    assert_eq!(extents(&d), [(3_532_240, 7_644_130)]);
}

#[test]
fn same_blob_twice_shares_one_media_part() {
    let mut assets = Assets::default();
    assets.images.insert(
        BLOB_A.into(),
        ImageAsset::Embedded(picture(PicFormat::Gif, 10, 10, 1)),
    );
    let docx = export_with(
        vec![image_block(BLOB_A, "a"), image_block(BLOB_A, "b")],
        &assets,
    );
    let media: Vec<_> = docx
        .0
        .keys()
        .filter(|k| k.starts_with("word/media/"))
        .collect();
    assert_eq!(media, ["word/media/image1.gif"]);
    let d = xml(docx.part("word/document.xml"));
    let ids: HashSet<_> = d
        .descendants()
        .filter(|n| n.has_tag_name((WP, "docPr")))
        .map(|n| n.attribute("id").unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
}

#[test]
fn fallbacks_are_rendered_and_counted() {
    let (doc, assets) = common::fallback_fixture();
    let out = write_docx(&doc, &meta(), &assets).expect("export");
    assert_eq!(
        out.fallbacks,
        Fallbacks {
            diagram: 2,
            image_unavailable: 2,
            image_over_budget: 1,
            unknown_element: 0,
        }
    );
    let docx = Docx(unzip(&out.bytes));
    let d = xml(docx.part("word/document.xml"));
    let paras: Vec<(Option<&str>, String)> = content(&d)
        .into_iter()
        .map(|p| (p_style(p), text(p)))
        .collect();
    assert_eq!(
        paras,
        [
            // The line break is a w:br, which text() skips.
            (Some("Code"), "graph TD  A --> B".to_owned()),
            (Some("Caption"), "Mermaid diagram (not included)".to_owned()),
            (
                Some("Caption"),
                "Diagram: Roadmap (not included)".to_owned()
            ),
            (
                Some("Caption"),
                "[Image not available] \u{2014} Org chart".to_owned()
            ),
            (Some("Caption"), "[Image too large to include]".to_owned()),
            (
                Some("Caption"),
                "[Image not available] \u{2014} Local file".to_owned()
            ),
        ]
    );
    // The over-budget placeholder links to the blob.
    let rels = rels(&docx);
    let link = d
        .descendants()
        .find(|n| n.has_tag_name((W, "hyperlink")))
        .expect("hyperlink");
    let rel = &rels[link.attribute((R, "id")).unwrap()];
    assert_eq!(
        rel.target,
        format!(
            "https://knot.example/api/blobs/{}",
            common::OVER_BUDGET_BLOB
        )
    );
    assert!(rel.external);
    assert_eq!(text(link), "[Image too large to include]");
}

#[test]
fn absent_and_unlabelled_images_fall_back() {
    let out = write_docx(
        &ExportDoc {
            blocks: vec![
                image_block(BLOB_A, ""),
                Block::Image {
                    source: ImageSource::Unsupported,
                    alt: String::new(),
                },
            ],
        },
        &meta(),
        &Assets::default(),
    )
    .expect("export");
    assert_eq!(out.fallbacks.image_unavailable, 2);
    let docx = Docx(unzip(&out.bytes));
    let d = xml(docx.part("word/document.xml"));
    let texts: Vec<_> = content(&d).into_iter().map(text).collect();
    assert_eq!(texts, ["[Image not available]", "[Image not available]"]);
}

#[test]
fn external_image_is_a_link() {
    let docx = export(vec![
        Block::Image {
            source: ImageSource::External("https://img.example/a.png?x=1&y=2".into()),
            alt: "Remote".into(),
        },
        Block::Image {
            source: ImageSource::External("https://img.example/b.png".into()),
            alt: String::new(),
        },
    ]);
    let d = xml(docx.part("word/document.xml"));
    let rels = rels(&docx);
    let got: Vec<_> = d
        .descendants()
        .filter(|n| n.has_tag_name((W, "hyperlink")))
        .map(|l| {
            (
                text(l),
                rels[l.attribute((R, "id")).unwrap()].target.clone(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                "Remote".to_owned(),
                "https://img.example/a.png?x=1&y=2".to_owned()
            ),
            (
                "https://img.example/b.png".to_owned(),
                "https://img.example/b.png".to_owned()
            ),
        ]
    );
}

#[test]
fn attachment_line_links_back_to_knot() {
    let url = "/api/blobs/cccccccc-1111-4222-8333-444444444444";
    let docx = export(vec![
        Block::Attachment {
            url: url.into(),
            name: "budget.xlsx".into(),
            size: Some(49152),
        },
        Block::Attachment {
            url: "javascript:alert(1)".into(),
            name: "odd".into(),
            size: None,
        },
    ]);
    let d = xml(docx.part("word/document.xml"));
    let ps = content(&d);
    assert_eq!(text(ps[0]), "\u{1F4CE} budget.xlsx (48 KB)");
    assert_eq!(p_style(ps[0]), None);
    let link = child(ps[0], "hyperlink").expect("hyperlink");
    let rels = rels(&docx);
    assert_eq!(
        rels[link.attribute((R, "id")).unwrap()].target,
        format!("https://knot.example{url}")
    );
    // No size, and no link for a target that isn't a knot path.
    assert_eq!(text(ps[1]), "\u{1F4CE} odd");
    assert!(child(ps[1], "hyperlink").is_none());
}

#[test]
fn table_in_list_item_is_indented() {
    let docx = export(vec![list(
        false,
        1,
        vec![item(vec![
            para("lead"),
            table(even(2), vec![vec![td(0, "a"), td(1, "b")]]),
        ])],
    )]);
    let d = xml(docx.part("word/document.xml"));
    let tbl = tables(&d)[0];
    let pr = child(tbl, "tblPr").expect("tblPr");
    let names: Vec<&str> = elements(pr).iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(names, ["tblStyle", "tblW", "tblInd", "tblLayout"]);
    let ind = child(pr, "tblInd").unwrap();
    assert_eq!(wattr(ind, "w"), Some("720"));
    assert_eq!(wattr(ind, "type"), Some("dxa"));
    assert_eq!(tbl_w(tbl), 9072 - 720);
}

#[test]
fn table_in_quote_is_indented_and_top_level_is_not() {
    let docx = export(vec![
        Block::Quote(vec![table(even(1), vec![vec![td(0, "a")]])]),
        para("between"),
        table(even(1), vec![vec![td(0, "b")]]),
    ]);
    let d = xml(docx.part("word/document.xml"));
    let tbls = tables(&d);
    let ind = child(child(tbls[0], "tblPr").unwrap(), "tblInd").unwrap();
    assert_eq!(wattr(ind, "w"), Some("567"));
    assert!(child(child(tbls[1], "tblPr").unwrap(), "tblInd").is_none());
}

/// Most `word/document.xml` a crafted table may make per grid position the
/// walker counts: a merge continuation is an empty `w:tc` of about 80 bytes.
const BYTES_PER_GRID_CELL: usize = 100;

#[test]
fn amplification_is_bounded() {
    use common::{build, row_spanning_table};
    use knot_export::{MAX_PAGE_GRID_CELLS, MAX_TABLE_GRID_CELLS, walk};
    // A table inside every limit: 64 columns of cells spanning 64 rows.
    // Between them the page holds empty rows; the file, a `vMerge`
    // continuation cell per column.
    let rows = MAX_TABLE_GRID_CELLS / 64;
    let walked = walk(&build(vec![row_spanning_table(64, 64, rows)]));
    assert!(
        walked.unknown_elements.is_empty(),
        "{:?}",
        walked.unknown_elements
    );
    assert!(matches!(walked.doc.blocks.as_slice(), [Block::Table(_)]));
    let docx = export(walked.doc.blocks);
    let body = docx.part("word/document.xml");
    let restarts = rows.div_ceil(64);
    assert_eq!(body.matches("<w:vMerge/>").count(), 64 * (rows - restarts));
    assert!(
        body.len() < 64 * rows * BYTES_PER_GRID_CELL,
        "{}",
        body.len()
    );

    // A page of them: the page budget keeps three, and degrades the rest.
    let walked = walk(&build(
        (0..6).map(|_| row_spanning_table(64, 64, rows)).collect(),
    ));
    assert_eq!(walked.unknown_elements, ["table-limit"; 3]);
    let docx = export(walked.doc.blocks);
    let len = docx.part("word/document.xml").len();
    assert!(len < MAX_PAGE_GRID_CELLS * BYTES_PER_GRID_CELL, "{len}");
}

#[test]
fn xml_past_the_cap_fails_the_export() {
    use knot_export::ExportError;
    use knot_export::xml::MAX_XML_BYTES;
    // Only a page far past the walker's limits gets here: the writer stops
    // and fails instead of building the rest.
    let text = "x".repeat(1 << 20);
    let blocks = vec![para(&text); MAX_XML_BYTES / text.len() + 1];
    let result = write_docx(&ExportDoc { blocks }, &meta(), &Assets::default());
    assert!(
        matches!(result, Err(ExportError::TooLarge)),
        "{:?}",
        result.err()
    );
}

#[test]
fn output_buffer_is_sized_up_front() {
    // A megabyte of picture: grown by doubling, the buffer would end up
    // about twice that.
    let mut assets = Assets::default();
    let pic = Picture {
        format: PicFormat::Png,
        bytes: common::noise(1 << 20),
        width_px: 10,
        height_px: 10,
        scale: 1,
    };
    assets
        .images
        .insert(BLOB_A.into(), ImageAsset::Embedded(pic));
    let blocks = vec![Block::Image {
        source: ImageSource::Blob(BLOB_A.into()),
        alt: String::new(),
    }];
    let out = write_docx(&ExportDoc { blocks }, &meta(), &assets).expect("export");
    assert!(out.bytes.len() > 1 << 20);
    let spare = out.bytes.capacity() - out.bytes.len();
    assert!(spare < 256 * 1024, "{spare} bytes spare");
}
