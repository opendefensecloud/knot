mod common;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use common::odt::{
    DC, FO, MANIFEST, META, OFFICE, Odt, STYLE, TABLE, TEXT, XLINK, XML_NS, all, child, children,
    cm, content, elements, list_depth, office_text, prop, style, style_name, text,
};
use common::{unzip, xml};
use knot_export::assets::{Assets, Meta};
use knot_export::odt::{OdtDefaults, write_odt_with};
use knot_export::{
    Align, Block, Cell, ExportDoc, Inline, LinkTarget, List, ListItem, Marks, Run, Table, write_odt,
};
use roxmltree::{Document, Node};

const MIMETYPE: &str = "application/vnd.oasis.opendocument.text";
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

fn export_with(blocks: Vec<Block>, assets: &Assets) -> Odt {
    let out = write_odt(&ExportDoc { blocks }, &meta(), assets).expect("export");
    Odt(unzip(&out.bytes))
}

fn export(blocks: Vec<Block>) -> Odt {
    export_with(blocks, &Assets::default())
}

/// Parse every XML part; panics on the first malformed one.
fn parse_all(odt: &Odt) {
    for name in odt.0.keys() {
        if name.ends_with(".xml") {
            xml(odt.part(name));
        }
    }
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

fn code(text: &str) -> Block {
    Block::Code {
        language: String::new(),
        text: text.into(),
    }
}

/// One of every text block and inline.
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
        code("fn main() {\n\tprintln!();\n}"),
        Block::Rule,
    ]
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

fn check_item(checked: bool, blocks: Vec<Block>) -> ListItem {
    ListItem {
        checked: Some(checked),
        blocks,
    }
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

fn assert_close(got: f64, want: f64, what: &str) {
    assert!((got - want).abs() < 1e-9, "{what}: {got} != {want}");
}

/// The children of a paragraph as "text", "s <c>", "tab" or "line-break".
fn pieces(p: Node) -> Vec<String> {
    p.children()
        .map(|n| {
            if n.is_text() {
                n.text().unwrap().to_owned()
            } else if n.has_tag_name((TEXT, "s")) {
                format!("s {}", n.attribute((TEXT, "c")).unwrap_or("-"))
            } else {
                n.tag_name().name().to_owned()
            }
        })
        .collect()
}

/// The style a node names with `ns:style-name`, from `content.xml`'s
/// automatic styles.
fn auto_style<'a, 'i>(d: &'a Document<'i>, n: Node, ns: &str) -> Node<'a, 'i> {
    style(d, n.attribute((ns, "style-name")).expect("style-name"))
}

// Package.

#[test]
fn mimetype_is_first_and_stored_and_manifest_is_complete() {
    let out = write_odt(
        &ExportDoc {
            blocks: kitchen_sink(),
        },
        &meta(),
        &Assets::default(),
    )
    .expect("export");
    let bytes = &out.bytes;

    // The first local header: stored, no extra field, then the name and the
    // bytes of the media type.
    assert_eq!(&bytes[..4], b"PK\x03\x04");
    assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 0, "stored");
    let name_len = usize::from(u16::from_le_bytes([bytes[26], bytes[27]]));
    assert_eq!(
        u16::from_le_bytes([bytes[28], bytes[29]]),
        0,
        "no extra field"
    );
    assert_eq!(&bytes[30..30 + name_len], b"mimetype");
    let data = 30 + name_len;
    assert_eq!(&bytes[data..data + MIMETYPE.len()], MIMETYPE.as_bytes());

    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("zip");
    let first = zip.by_index(0).expect("first entry");
    assert_eq!(first.name(), "mimetype");
    assert_eq!(first.compression(), zip::CompressionMethod::Stored);
    drop(first);

    let odt = Odt(unzip(bytes));
    assert_eq!(odt.part("mimetype"), MIMETYPE);
    parse_all(&odt);

    let manifest = xml(odt.part("META-INF/manifest.xml"));
    assert_eq!(
        manifest.root_element().attribute((MANIFEST, "version")),
        Some("1.3")
    );
    let entries: HashMap<&str, &str> = all(&manifest, MANIFEST, "file-entry")
        .into_iter()
        .map(|e| {
            (
                e.attribute((MANIFEST, "full-path")).unwrap(),
                e.attribute((MANIFEST, "media-type")).unwrap(),
            )
        })
        .collect();
    assert_eq!(entries.get("/"), Some(&MIMETYPE));
    for name in odt.0.keys() {
        if name != "mimetype" && !name.starts_with("META-INF/") {
            assert!(
                entries.contains_key(name.as_str()),
                "{name} not in manifest"
            );
        }
    }
    for path in entries.keys().filter(|p| **p != "/") {
        assert!(odt.0.contains_key(*path), "manifest lists missing {path}");
    }
    for part in ["content.xml", "styles.xml", "meta.xml"] {
        assert_eq!(entries.get(part), Some(&"text/xml"), "{part}");
        let d = xml(odt.part(part));
        assert_eq!(
            d.root_element().attribute((OFFICE, "version")),
            Some("1.3"),
            "{part}"
        );
    }

    let meta_xml = xml(odt.part("meta.xml"));
    let find = |ns: &str, name: &str| {
        meta_xml
            .descendants()
            .find(|n| n.has_tag_name((ns, name)))
            .and_then(|n| n.text())
    };
    assert_eq!(find(META, "generator"), Some("knot"));
    assert_eq!(find(DC, "title"), Some("Q4 Plan"));
    for (ns, name) in [(DC, "creator"), (META, "initial-creator")] {
        assert!(find(ns, name).is_none(), "no {name}");
    }
}

#[test]
fn empty_page_has_title_subtitle_and_footer_fields() {
    let odt = export(vec![]);
    let d = xml(odt.part("content.xml"));
    let kids = elements(office_text(&d));
    assert_eq!(kids.len(), 2);
    let expected = [
        ("Title", "Q4 Plan"),
        ("Subtitle", "Exported from knot · 23 Oct 2026"),
    ];
    for (p, (style, copy)) in kids.iter().zip(expected) {
        assert!(p.has_tag_name((TEXT, "p")));
        assert_eq!(style_name(*p), Some(style));
        assert_eq!(text(*p), copy);
    }

    let styles = xml(odt.part("styles.xml"));
    let master = all(&styles, STYLE, "master-page")
        .into_iter()
        .find(|m| m.attribute((STYLE, "name")) == Some("Standard"))
        .expect("master page Standard");
    let layout = all(&styles, STYLE, "page-layout")
        .into_iter()
        .find(|l| l.attribute((STYLE, "name")) == master.attribute((STYLE, "page-layout-name")))
        .expect("page layout");
    let page = child(layout, STYLE, "page-layout-properties").expect("page properties");
    assert_eq!(page.attribute((FO, "page-width")), Some("21cm"));
    assert_eq!(page.attribute((FO, "page-height")), Some("29.7cm"));
    assert_eq!(
        page.attribute((STYLE, "print-orientation")),
        Some("portrait")
    );
    for side in ["top", "left", "right"] {
        let margin = page.attribute((FO, format!("margin-{side}").as_str()));
        assert_close(cm(margin.unwrap()), 2.5, side);
    }
    let footer_props = child(layout, STYLE, "footer-style")
        .and_then(|f| child(f, STYLE, "header-footer-properties"))
        .expect("footer properties");
    let bottom = cm(page.attribute((FO, "margin-bottom")).unwrap())
        + cm(footer_props.attribute((FO, "min-height")).unwrap())
        + cm(footer_props.attribute((FO, "margin-top")).unwrap());
    assert_close(bottom, 2.5, "bottom margin, footer height and spacing");

    let footer = child(master, STYLE, "footer").expect("footer");
    let p = child(footer, TEXT, "p").expect("footer paragraph");
    assert_eq!(style_name(p), Some("Footer"));
    assert!(child(p, TEXT, "page-number").is_some());
    assert!(child(p, TEXT, "page-count").is_some());
    assert_eq!(text(p), "Page 1 of 1");
    let footer_style = style(&styles, "Footer");
    assert_eq!(prop(footer_style, FO, "text-align"), Some("center"));
}

#[test]
fn package_defaults_are_written_as_given_odt() {
    let builtin = OdtDefaults::builtin();
    let defaults = OdtDefaults {
        styles_xml: Cow::Owned(builtin.styles_xml.replace("Calibri", "Arial")),
    };
    let out = write_odt_with(
        &ExportDoc {
            blocks: vec![para("x")],
        },
        &meta(),
        &Assets::default(),
        &defaults,
    )
    .expect("export");
    let odt = Odt(unzip(&out.bytes));
    assert_eq!(odt.part("styles.xml"), defaults.styles_xml);
    assert!(odt.part("styles.xml").contains("Arial"));
}

// Styles.

#[test]
fn styles_cover_the_vocabulary() {
    let odt = export(vec![]);
    let styles = xml(odt.part("styles.xml"));
    for (name, family, display) in [
        ("Standard", "paragraph", None),
        ("Title", "paragraph", Some("Title")),
        ("Subtitle", "paragraph", Some("Subtitle")),
        ("Quotations", "paragraph", Some("Quotations")),
        (
            "Preformatted_20_Text",
            "paragraph",
            Some("Preformatted Text"),
        ),
        ("Source_20_Text", "text", Some("Source Text")),
        ("List_20_Paragraph", "paragraph", Some("List Paragraph")),
        ("Table_20_Contents", "paragraph", Some("Table Contents")),
        ("Table_20_Heading", "paragraph", Some("Table Heading")),
        ("Internet_20_link", "text", Some("Internet link")),
        ("Caption", "paragraph", Some("Caption")),
        ("Footer", "paragraph", Some("Footer")),
        ("Horizontal_20_Line", "paragraph", Some("Horizontal Line")),
    ] {
        let s = style(&styles, name);
        assert!(s.has_tag_name((STYLE, "style")), "{name} is a style:style");
        assert_eq!(s.attribute((STYLE, "family")), Some(family), "{name}");
        if let Some(display) = display {
            assert_eq!(s.attribute((STYLE, "display-name")), Some(display));
        }
    }

    let pre = style(&styles, "Preformatted_20_Text");
    assert_eq!(prop(pre, STYLE, "font-name"), Some("Consolas"));
    assert_eq!(prop(pre, FO, "background-color"), Some("#f5f5f5"));
    let source = style(&styles, "Source_20_Text");
    assert_eq!(prop(source, STYLE, "font-name"), Some("Consolas"));
    assert_eq!(
        prop(style(&styles, "Caption"), FO, "font-style"),
        Some("italic")
    );
    let fonts: Vec<&str> = all(&styles, STYLE, "font-face")
        .into_iter()
        .filter_map(|f| f.attribute((STYLE, "name")))
        .collect();
    assert_eq!(fonts, ["Calibri", "Consolas"]);
    let default = all(&styles, STYLE, "default-style")
        .into_iter()
        .find(|s| s.attribute((STYLE, "family")) == Some("paragraph"))
        .expect("default paragraph style");
    assert_eq!(prop(default, STYLE, "font-name"), Some("Calibri"));
}

#[test]
fn headings_have_outline_levels_and_styles() {
    let heading = |level| Block::Heading {
        level,
        content: vec![Inline::Run(run("h"))],
    };
    let odt = export((0..=7).map(heading).collect());
    let d = xml(odt.part("content.xml"));
    let got: Vec<(&str, String, String)> = content(&d)
        .into_iter()
        .map(|h| {
            (
                h.tag_name().name(),
                h.attribute((TEXT, "outline-level")).unwrap().to_owned(),
                style_name(h).unwrap().to_owned(),
            )
        })
        .collect();
    let expected: Vec<(&str, String, String)> = [1, 1, 2, 3, 4, 5, 6, 6]
        .into_iter()
        .map(|l| ("h", l.to_string(), format!("Heading_20_{l}")))
        .collect();
    assert_eq!(got, expected, "levels 0 and 7 are clamped");

    let styles = xml(odt.part("styles.xml"));
    for level in 1..=6 {
        let s = style(&styles, &format!("Heading_20_{level}"));
        assert_eq!(
            s.attribute((STYLE, "display-name")),
            Some(format!("Heading {level}").as_str())
        );
        assert_eq!(
            s.attribute((STYLE, "default-outline-level")),
            Some(level.to_string().as_str())
        );
        assert_eq!(prop(s, FO, "font-weight"), Some("bold"));
    }
    let outline = all(&styles, TEXT, "outline-style");
    assert_eq!(outline.len(), 1);
    let levels = children(outline[0], TEXT, "outline-level-style");
    assert_eq!(levels.len(), 10);
    for l in levels {
        assert_eq!(l.attribute((STYLE, "num-format")), Some(""), "unnumbered");
    }
}

#[test]
fn list_styles_have_ten_levels_and_the_same_indents() {
    let odt = export(vec![]);
    let styles = xml(odt.part("styles.xml"));
    let mut indents = Vec::new();
    for (name, level_tag) in [
        ("List_20_Bullet", "list-level-style-bullet"),
        ("Numbering_20_123", "list-level-style-number"),
        ("List_20_Check", "list-level-style-number"),
    ] {
        let s = style(&styles, name);
        assert!(s.has_tag_name((TEXT, "list-style")), "{name}");
        let levels = elements(s);
        assert_eq!(levels.len(), 10, "{name}");
        let mut margins = Vec::new();
        for (i, l) in levels.into_iter().enumerate() {
            assert!(l.has_tag_name((TEXT, level_tag)), "{name} level {i}");
            assert_eq!(
                l.attribute((TEXT, "level")),
                Some((i + 1).to_string().as_str())
            );
            let align = l
                .descendants()
                .find(|n| n.has_tag_name((STYLE, "list-level-label-alignment")))
                .expect("label alignment");
            let margin = cm(align.attribute((FO, "margin-left")).unwrap());
            assert_close(margin, 1.27 * (i + 1) as f64, name);
            margins.push(margin);
            if name == "List_20_Check" {
                assert_eq!(l.attribute((STYLE, "num-format")), Some(""), "no label");
            }
        }
        indents.push(margins);
    }
    assert_eq!(indents[0], indents[1]);
    assert_eq!(indents[0], indents[2]);
    let numbers = elements(style(&styles, "Numbering_20_123"));
    assert_eq!(numbers[0].attribute((STYLE, "num-format")), Some("1"));
}

#[test]
fn every_referenced_style_exists_odt() {
    let mut blocks = kitchen_sink();
    blocks.push(list(
        false,
        1,
        vec![
            item(vec![para("plain")]),
            check_item(true, vec![para("done")]),
        ],
    ));
    blocks.push(list(
        true,
        1,
        vec![item(vec![
            para("lead"),
            table(
                vec![0.5, 0.5],
                vec![
                    vec![th(0, "h"), th(1, "h")],
                    vec![
                        Cell {
                            align: Align::Center,
                            ..td(0, "c")
                        },
                        cell(1, vec![code("x")]),
                    ],
                ],
            ),
        ])],
    ));
    let odt = export(blocks);
    let styles = xml(odt.part("styles.xml"));
    let content_xml = xml(odt.part("content.xml"));

    // Declared styles by (kind, name). A list style's kind is "list".
    let mut declared = HashSet::new();
    for d in [&styles, &content_xml] {
        for n in d.descendants().filter(Node::is_element) {
            let Some(name) = n.attribute((STYLE, "name")) else {
                continue;
            };
            let kind = match n.tag_name().name() {
                "style" => n.attribute((STYLE, "family")).unwrap(),
                "list-style" => "list",
                other => other,
            };
            declared.insert((kind.to_owned(), name.to_owned()));
        }
    }

    let mut seen = HashSet::new();
    for d in [&styles, &content_xml] {
        for n in d.descendants().filter(Node::is_element) {
            let family = n.attribute((STYLE, "family"));
            let refs: Vec<(&str, Option<&str>)> =
                match (n.tag_name().namespace(), n.tag_name().name()) {
                    (Some(TEXT), "p" | "h") => vec![("paragraph", style_name(n))],
                    (Some(TEXT), "span" | "a") => vec![("text", style_name(n))],
                    (Some(TEXT), "list") => vec![("list", style_name(n))],
                    (Some(TEXT), "list-item") => {
                        vec![("list", n.attribute((TEXT, "style-override")))]
                    }
                    (Some(TABLE), "table" | "table-column" | "table-cell") => {
                        let kind = match n.tag_name().name() {
                            "table" => "table",
                            "table-column" => "table-column",
                            _ => "table-cell",
                        };
                        vec![(kind, n.attribute((TABLE, "style-name")))]
                    }
                    (Some(STYLE), "style") => vec![
                        (family.unwrap(), n.attribute((STYLE, "parent-style-name"))),
                        (family.unwrap(), n.attribute((STYLE, "next-style-name"))),
                    ],
                    (Some(STYLE), "master-page") => {
                        vec![("page-layout", n.attribute((STYLE, "page-layout-name")))]
                    }
                    _ => vec![],
                };
            for (kind, name) in refs {
                let Some(name) = name else { continue };
                assert!(
                    declared.contains(&(kind.to_owned(), name.to_owned())),
                    "{} references missing {kind} style {name}",
                    n.tag_name().name()
                );
                seen.insert(name.to_owned());
            }
        }
    }
    for name in [
        "Title",
        "Subtitle",
        "Heading_20_2",
        "Source_20_Text",
        "Internet_20_link",
        "Quotations",
        "Preformatted_20_Text",
        "Horizontal_20_Line",
        "Footer",
        "List_20_Bullet",
        "Numbering_20_123",
        "List_20_Check",
        "List_20_Paragraph",
        "Table_20_Contents",
        "Table_20_Heading",
    ] {
        assert!(seen.contains(name), "the sample should use {name}");
    }
}

// Text.

#[test]
fn marks_links_and_mentions() {
    let mut assets = Assets::default();
    assets.user_names.insert(USER_ID.into(), "Ada".into());
    let bold = Marks {
        bold: true,
        ..Marks::default()
    };
    let web = "https://a.example/x?y=1&z";
    let odt = export_with(
        vec![Block::Paragraph(vec![
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
                text: "b1".into(),
                marks: bold.clone(),
            }),
            Inline::Run(Run {
                text: "b2".into(),
                marks: bold,
            }),
            Inline::Run(Run {
                text: "x = 1".into(),
                marks: Marks {
                    code: true,
                    ..Marks::default()
                },
            }),
            link(LinkTarget::Web(web.into()), "web"),
            link(LinkTarget::Doc("d1".into()), "page"),
            link(LinkTarget::User(USER_ID.into()), "@Old Name"),
            link(LinkTarget::User("gone".into()), "@Gone"),
            link(LinkTarget::Plain, "knot://time/2026-10-23"),
        ])],
        &assets,
    );
    let d = xml(odt.part("content.xml"));
    let p = content(&d)[0];
    assert_eq!(style_name(p), Some("Standard"));
    assert_eq!(
        text(p),
        "loudb1b2x = 1webpage@Ada@Goneknot://time/2026-10-23"
    );

    let spans = children(p, TEXT, "span");
    assert_eq!(spans.len(), 4);
    let loud = auto_style(&d, spans[0], TEXT);
    assert_eq!(loud.attribute((STYLE, "family")), Some("text"));
    for (ns, name, value) in [
        (FO, "font-weight", "bold"),
        (FO, "font-style", "italic"),
        (STYLE, "text-underline-style", "solid"),
        (STYLE, "text-line-through-style", "solid"),
    ] {
        assert_eq!(prop(loud, ns, name), Some(value), "{name}");
    }
    assert_eq!(
        style_name(spans[1]),
        style_name(spans[2]),
        "one automatic style per mark set"
    );
    assert_ne!(style_name(spans[0]), style_name(spans[1]));
    assert_eq!(style_name(spans[3]), Some("Source_20_Text"));
    assert_eq!(text(spans[3]), "x = 1");

    let links = children(p, TEXT, "a");
    let got: Vec<(Option<&str>, String)> = links
        .iter()
        .map(|a| (a.attribute((XLINK, "href")), text(*a)))
        .collect();
    assert_eq!(
        got,
        [
            (Some(web), "web".to_owned()),
            (Some("https://knot.example/doc/d1"), "page".to_owned()),
        ]
    );
    for a in &links {
        assert_eq!(a.attribute((XLINK, "type")), Some("simple"));
        assert_eq!(style_name(*a), Some("Internet_20_link"));
    }
    assert!(odt.part("content.xml").contains("x?y=1&amp;z"));
}

#[test]
fn code_preserves_spaces_and_tabs() {
    let odt = export(vec![code("  a\tb   c\r\n d\re")]);
    let d = xml(odt.part("content.xml"));
    let blocks = content(&d);
    assert_eq!(blocks.len(), 1, "one paragraph");
    let p = blocks[0];
    assert_eq!(style_name(p), Some("Preformatted_20_Text"));
    assert_eq!(
        pieces(p),
        [
            "s 2",
            "a",
            "tab",
            "b ",
            "s 2",
            "c",
            "line-break",
            "s 1",
            "d",
            "line-break",
            "e"
        ]
    );
    assert_eq!(text(p), "  a\tb   c\n d\ne");
}

#[test]
fn normal_text_preserves_tabs_and_double_spaces() {
    let bold = Marks {
        bold: true,
        ..Marks::default()
    };
    let meta = Meta {
        title: "  Indented  title".into(),
        ..meta()
    };
    let blocks = vec![
        para("a\tb  c"),
        // A space after another run's trailing space would collapse.
        Block::Paragraph(vec![
            Inline::Run(run("x ")),
            Inline::Run(Run {
                text: " y".into(),
                marks: bold,
            }),
            Inline::Break,
            Inline::Run(run(" z ")),
        ]),
    ];
    let out = write_odt(&ExportDoc { blocks }, &meta, &Assets::default()).expect("export");
    let odt = Odt(unzip(&out.bytes));
    let d = xml(odt.part("content.xml"));
    let title = elements(office_text(&d))[0];
    assert_eq!(pieces(title), ["s 2", "Indented ", "s 1", "title"]);

    let blocks = content(&d);
    assert_eq!(pieces(blocks[0]), ["a", "tab", "b ", "s 1", "c"]);
    assert_eq!(pieces(blocks[1]), ["x ", "span", "line-break", "s 1", "z "]);
    let span = child(blocks[1], TEXT, "span").unwrap();
    assert_eq!(pieces(span), ["s 1", "y"]);
    assert_eq!(text(blocks[1]), "x  y\n z ");
}

#[test]
fn quote_and_rule_styles_odt() {
    let odt = export(vec![Block::Quote(vec![para("quoted")]), Block::Rule]);
    let d = xml(odt.part("content.xml"));
    let blocks = content(&d);
    assert_eq!(blocks.len(), 2);
    assert_eq!(style_name(blocks[0]), Some("Quotations"));
    assert_eq!(text(blocks[0]), "quoted");
    assert_eq!(style_name(blocks[1]), Some("Horizontal_20_Line"));
    assert!(blocks[1].children().next().is_none(), "an empty paragraph");

    let styles = xml(odt.part("styles.xml"));
    let quote = style(&styles, "Quotations");
    assert_close(
        cm(prop(quote, FO, "margin-left").unwrap()),
        1.0,
        "quote indent",
    );
    assert!(prop(style(&styles, "Horizontal_20_Line"), FO, "border-bottom").is_some());
}

#[test]
fn text_with_xml_specials_and_controls_is_well_formed_odt() {
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
    let out = write_odt(&doc, &meta, &assets).expect("export");
    let odt = Odt(unzip(&out.bytes));
    parse_all(&odt);

    let clean = HOSTILE.replace('\u{1}', "");
    let meta_xml = xml(odt.part("meta.xml"));
    let title = meta_xml
        .descendants()
        .find(|n| n.has_tag_name((DC, "title")))
        .and_then(|n| n.text());
    assert_eq!(title, Some(clean.as_str()));
    let d = xml(odt.part("content.xml"));
    assert_eq!(text(elements(office_text(&d))[0]), clean);
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
    let a = all(&d, TEXT, "a")[0];
    assert_eq!(
        a.attribute((XLINK, "href")),
        Some("https://a.example/?q=<a>&b=\"c\"")
    );
}

// Lists.

#[test]
fn lists_restart_and_checklists_use_boxes() {
    let odt = export(vec![
        list(
            true,
            5,
            vec![item(vec![para("five")]), item(vec![para("six")])],
        ),
        para("between"),
        list(true, 1, vec![item(vec![para("one")])]),
        list(
            false,
            1,
            vec![
                check_item(true, vec![para("done")]),
                check_item(false, vec![para("todo")]),
            ],
        ),
    ]);
    let d = xml(odt.part("content.xml"));
    let blocks = content(&d);
    let tags: Vec<&str> = blocks.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(tags, ["list", "p", "list", "list"]);
    let lists = [blocks[0], blocks[2], blocks[3]];
    let styles: Vec<Option<&str>> = lists.iter().map(|l| style_name(*l)).collect();
    assert_eq!(
        styles,
        [
            Some("Numbering_20_123"),
            Some("Numbering_20_123"),
            Some("List_20_Bullet")
        ]
    );
    let ids: HashSet<&str> = lists
        .iter()
        .map(|l| l.attribute((XML_NS, "id")).expect("xml:id"))
        .collect();
    assert_eq!(ids.len(), 3, "unique ids");
    for l in lists {
        assert_eq!(l.attribute((TEXT, "continue-list")), None);
        assert_eq!(l.attribute((TEXT, "continue-numbering")), None);
    }

    let starts = |l: Node| -> Vec<Option<String>> {
        children(l, TEXT, "list-item")
            .into_iter()
            .map(|i| i.attribute((TEXT, "start-value")).map(str::to_owned))
            .collect()
    };
    assert_eq!(starts(blocks[0]), [Some("5".to_owned()), None]);
    assert_eq!(starts(blocks[2]), [Some("1".to_owned())], "restarts at 1");
    assert_eq!(starts(blocks[3]), [None, None]);

    let checks = children(blocks[3], TEXT, "list-item");
    let got: Vec<(Option<&str>, String)> = checks
        .iter()
        .map(|i| {
            (
                i.attribute((TEXT, "style-override")),
                text(child(*i, TEXT, "p").unwrap()),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (Some("List_20_Check"), "\u{2611} done".to_owned()),
            (Some("List_20_Check"), "\u{2610} todo".to_owned()),
        ]
    );
    for p in all(&d, TEXT, "p")
        .into_iter()
        .filter(|p| list_depth(*p) > 0)
    {
        assert_eq!(style_name(p), Some("List_20_Paragraph"));
    }
}

#[test]
fn mixed_nested_checklist_keeps_levels() {
    let odt = export(vec![list(
        false,
        1,
        vec![
            item(vec![para("plain")]),
            check_item(
                true,
                vec![
                    para("parent"),
                    list(false, 1, vec![check_item(true, vec![para("child")])]),
                ],
            ),
        ],
    )]);
    let d = xml(odt.part("content.xml"));
    let got: Vec<(String, usize, Option<&str>)> = all(&d, TEXT, "list-item")
        .into_iter()
        .map(|i| {
            (
                text(child(i, TEXT, "p").unwrap()),
                list_depth(i),
                i.attribute((TEXT, "style-override")),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("plain".to_owned(), 1, None),
            ("\u{2611} parent".to_owned(), 1, Some("List_20_Check")),
            ("\u{2611} child".to_owned(), 2, Some("List_20_Check")),
        ]
    );
    let nested = all(&d, TEXT, "list")[1];
    assert_eq!(style_name(nested), Some("List_20_Bullet"));
    assert_eq!(
        nested.attribute((XML_NS, "id")),
        None,
        "only the outer list has an id"
    );
}

#[test]
fn item_without_leading_paragraph_still_has_a_marker_paragraph() {
    let odt = export(vec![list(true, 1, vec![item(vec![code("x")])])]);
    let d = xml(odt.part("content.xml"));
    let it = all(&d, TEXT, "list-item")[0];
    let ps = children(it, TEXT, "p");
    assert_eq!(ps.len(), 2);
    assert_eq!(style_name(ps[0]), Some("List_20_Paragraph"));
    assert_eq!(text(ps[0]), "");
    assert_eq!(style_name(ps[1]), Some("Preformatted_20_Text"));
}

#[test]
fn deep_list_clamps_to_ten_levels() {
    let mut inner = vec![item(vec![para("leaf")])];
    for i in 0..11 {
        inner = vec![item(vec![
            para(&format!("x{i}")),
            list(false, 1, inner),
            para(&format!("after{i}")),
        ])];
    }
    let odt = export(vec![list(false, 1, inner)]);
    parse_all(&odt);
    let d = xml(odt.part("content.xml"));
    let lists = all(&d, TEXT, "list");
    assert_eq!(lists.len(), 10);
    assert_eq!(lists.iter().map(|l| list_depth(*l)).max(), Some(9));
    let ps = all(&d, TEXT, "p");
    let texts: Vec<String> = ps.iter().skip(2).map(|p| text(*p)).collect();
    let mut expected: Vec<String> = (0..11).rev().map(|i| format!("x{i}")).collect();
    expected.push("leaf".into());
    expected.extend((0..11).map(|i| format!("after{i}")));
    assert_eq!(texts, expected, "every paragraph, in order");
    // The two levels past the tenth continue the tenth list as items.
    let deepest = lists[9];
    let items = children(deepest, TEXT, "list-item");
    let firsts: Vec<String> = items
        .iter()
        .map(|i| text(child(*i, TEXT, "p").unwrap()))
        .collect();
    assert_eq!(firsts, ["x1", "x0", "leaf"]);
}

// Tables.

/// The rows of a table, header rows first.
fn rows<'a, 'i>(tbl: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    elements(tbl)
        .into_iter()
        .flat_map(|n| {
            if n.has_tag_name((TABLE, "table-header-rows")) {
                children(n, TABLE, "table-row")
            } else if n.has_tag_name((TABLE, "table-row")) {
                vec![n]
            } else {
                vec![]
            }
        })
        .collect()
}

/// Each row's cells as "text, span N, rows N", or "covered".
fn shape(tbl: Node) -> Vec<Vec<String>> {
    rows(tbl)
        .into_iter()
        .map(|tr| {
            elements(tr)
                .into_iter()
                .map(|c| {
                    if c.has_tag_name((TABLE, "covered-table-cell")) {
                        return "covered".to_owned();
                    }
                    assert!(c.has_tag_name((TABLE, "table-cell")));
                    let mut parts = vec![text(c)];
                    if let Some(n) = c.attribute((TABLE, "number-columns-spanned")) {
                        parts.push(format!("span {n}"));
                    }
                    if let Some(n) = c.attribute((TABLE, "number-rows-spanned")) {
                        parts.push(format!("rows {n}"));
                    }
                    parts.retain(|p| !p.is_empty());
                    parts.join(", ")
                })
                .collect()
        })
        .collect()
}

/// Widths of a table's columns in cm, from their automatic styles.
fn column_widths(d: &Document, tbl: Node) -> Vec<f64> {
    children(tbl, TABLE, "table-column")
        .into_iter()
        .map(|c| cm(prop(auto_style(d, c, TABLE), STYLE, "column-width").unwrap()))
        .collect()
}

#[test]
fn tables_have_columns_spans_and_covered_cells() {
    let odt = export(vec![table(
        vec![0.3, 0.7],
        vec![
            vec![th(0, "h1"), th(1, "h2")],
            vec![Cell {
                colspan: 2,
                align: Align::Center,
                ..td(0, "wide")
            }],
            vec![
                td(0, "a"),
                Cell {
                    align: Align::Right,
                    ..td(1, "b")
                },
            ],
            vec![
                Cell {
                    rowspan: 2,
                    ..td(0, "tall")
                },
                td(1, "c"),
            ],
            vec![td(1, "d")],
        ],
    )]);
    parse_all(&odt);
    let d = xml(odt.part("content.xml"));
    let tbl = all(&d, TABLE, "table")[0];
    let tstyle = auto_style(&d, tbl, TABLE);
    assert_eq!(tstyle.attribute((STYLE, "family")), Some("table"));
    assert_close(
        cm(prop(tstyle, STYLE, "width").unwrap()),
        16.0,
        "table width",
    );
    assert_eq!(
        prop(tstyle, FO, "margin-left"),
        None,
        "no indent at the top"
    );

    let widths = column_widths(&d, tbl);
    assert_eq!(widths.len(), 2);
    assert_close(widths[0], 4.8, "first column");
    assert_close(widths[1], 11.2, "second column");
    let rel: Vec<u32> = children(tbl, TABLE, "table-column")
        .into_iter()
        .map(|c| {
            let s = auto_style(&d, c, TABLE);
            assert_eq!(s.attribute((STYLE, "family")), Some("table-column"));
            let w = prop(s, STYLE, "rel-column-width").unwrap();
            w.strip_suffix('*').expect("N*").parse().unwrap()
        })
        .collect();
    assert_eq!(
        rel,
        [
            (0.3f64 * 65535.0).round() as u32,
            (0.7f64 * 65535.0).round() as u32
        ]
    );

    let heads = children(tbl, TABLE, "table-header-rows");
    assert_eq!(heads.len(), 1);
    assert_eq!(children(heads[0], TABLE, "table-row").len(), 1);
    assert_eq!(children(tbl, TABLE, "table-row").len(), 4);
    assert_eq!(
        shape(tbl),
        vec![
            vec!["h1", "h2"],
            vec!["wide, span 2", "covered"],
            vec!["a", "b"],
            vec!["tall, rows 2", "c"],
            vec!["covered", "d"],
        ]
    );

    for c in all(&d, TABLE, "table-cell") {
        let s = auto_style(&d, c, TABLE);
        assert_eq!(s.attribute((STYLE, "family")), Some("table-cell"));
        assert_eq!(prop(s, FO, "border"), Some("0.5pt solid #bfbfbf"));
        let header = c
            .ancestors()
            .any(|a| a.has_tag_name((TABLE, "table-header-rows")));
        let background = prop(s, FO, "background-color");
        assert_eq!(background, header.then_some("#f2f2f2"), "{}", text(c));
    }

    let p_of = |label: &str| {
        let p = all(&d, TEXT, "p")
            .into_iter()
            .find(|p| text(*p) == label)
            .unwrap();
        let s = auto_style(&d, p, TEXT);
        assert_eq!(s.attribute((STYLE, "family")), Some("paragraph"));
        (
            s.attribute((STYLE, "parent-style-name")),
            prop(s, FO, "text-align"),
        )
    };
    assert_eq!(p_of("h1"), (Some("Table_20_Heading"), Some("start")));
    assert_eq!(p_of("wide"), (Some("Table_20_Contents"), Some("center")));
    assert_eq!(p_of("a"), (Some("Table_20_Contents"), Some("start")));
    assert_eq!(p_of("b"), (Some("Table_20_Contents"), Some("end")));
}

#[test]
fn spans_are_clamped_to_the_table_and_the_header_group() {
    let odt = export(vec![
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
        // A header cell's row span stops at the last header row.
        table(
            even(2),
            vec![
                vec![
                    Cell {
                        rowspan: 3,
                        ..th(0, "H")
                    },
                    th(1, "H2"),
                ],
                vec![td(1, "x")],
                vec![td(0, "y"), td(1, "z")],
            ],
        ),
        // A column span that runs into a merged cell stops short of it.
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
    parse_all(&odt);
    let d = xml(odt.part("content.xml"));
    let tbls = all(&d, TABLE, "table");
    assert_eq!(
        shape(tbls[0]),
        vec![vec!["A, rows 2", "B"], vec!["covered", "C"]]
    );
    assert_eq!(
        shape(tbls[1]),
        vec![vec!["H", "H2"], vec!["", "x"], vec!["y", "z"]]
    );
    assert_eq!(children(tbls[1], TABLE, "table-header-rows").len(), 1);
    assert_eq!(
        shape(tbls[2]),
        vec![vec!["a", "b, rows 2", "c"], vec!["wide", "covered", ""]]
    );
}

#[test]
fn rows_are_padded_to_the_grid() {
    let odt = export(vec![table(
        even(3),
        vec![
            vec![td(0, "a"), td(1, "b"), td(2, "c")],
            vec![td(0, "short")],
            vec![td(0, "x"), td(2, "z")],
            vec![cell(0, vec![]), td(1, "m")],
        ],
    )]);
    let d = xml(odt.part("content.xml"));
    let tbl = all(&d, TABLE, "table")[0];
    assert_eq!(
        shape(tbl),
        vec![
            vec!["a", "b", "c"],
            vec!["short", "span 2", "covered"],
            vec!["x", "", "z"],
            vec!["", "m", ""],
        ]
    );
    for tr in rows(tbl) {
        assert_eq!(elements(tr).len(), 3, "every row spans the grid");
    }
}

#[test]
fn empty_tables_write_nothing_and_parse_odt() {
    let odt = export(vec![
        table(vec![], vec![]),
        table(vec![], vec![vec![]]),
        table(vec![1.0], vec![]),
        para("after"),
        list(
            false,
            1,
            vec![item(vec![para("lead"), table(vec![], vec![vec![]])])],
        ),
    ]);
    parse_all(&odt);
    let d = xml(odt.part("content.xml"));
    assert!(all(&d, TABLE, "table").is_empty());
    let blocks = content(&d);
    let tags: Vec<&str> = blocks.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(tags, ["p", "list"], "the list isn't split");
    assert_eq!(text(blocks[0]), "after");
}

#[test]
fn table_inside_list_item_is_valid_odf() {
    let odt = export(vec![list(
        true,
        3,
        vec![
            item(vec![
                para("lead"),
                table(even(2), vec![vec![td(0, "a"), td(1, "b")]]),
                para("tail"),
            ]),
            item(vec![para("next")]),
        ],
    )]);
    parse_all(&odt);
    let d = xml(odt.part("content.xml"));
    for tbl in all(&d, TABLE, "table") {
        assert!(
            !tbl.ancestors().any(|a| a.has_tag_name((TEXT, "list"))),
            "no table inside a list"
        );
    }
    let blocks = content(&d);
    let tags: Vec<&str> = blocks.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(tags, ["list", "table", "list"]);
    let id = blocks[0].attribute((XML_NS, "id")).expect("xml:id");
    assert_eq!(blocks[2].attribute((TEXT, "continue-list")), Some(id));
    assert_eq!(style_name(blocks[2]), Some("Numbering_20_123"));
    assert_eq!(blocks[2].attribute((XML_NS, "id")), None);

    // The rest of the item has no number of its own; the next item does.
    let kids = elements(blocks[2]);
    let got: Vec<(&str, String, Option<&str>)> = kids
        .iter()
        .map(|k| {
            (
                k.tag_name().name(),
                text(*k),
                k.attribute((TEXT, "start-value")),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("list-header", "tail".to_owned(), None),
            ("list-item", "next".to_owned(), None),
        ]
    );

    // It lines up with the item's text.
    let tstyle = auto_style(&d, blocks[1], TABLE);
    assert_close(cm(prop(tstyle, FO, "margin-left").unwrap()), 1.27, "indent");
    assert_close(
        cm(prop(tstyle, STYLE, "width").unwrap()),
        16.0 - 1.27,
        "width",
    );
}

#[test]
fn a_table_ending_an_item_leaves_no_empty_list() {
    let t = || table(even(1), vec![vec![td(0, "t")]]);
    let odt = export(vec![
        list(false, 1, vec![item(vec![para("a"), t(), t()])]),
        list(
            false,
            1,
            vec![item(vec![
                para("b"),
                list(false, 1, vec![item(vec![para("c"), t()])]),
            ])],
        ),
        list(false, 1, vec![]),
    ]);
    parse_all(&odt);
    let d = xml(odt.part("content.xml"));
    let tags: Vec<&str> = content(&d).iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(tags, ["list", "table", "table", "list", "table"]);
    for n in all(&d, TEXT, "list")
        .into_iter()
        .chain(all(&d, TEXT, "list-header"))
    {
        assert!(!elements(n).is_empty(), "an empty {}", n.tag_name().name());
    }
}

#[test]
fn table_in_nested_item_reopens_every_level() {
    let odt = export(vec![list(
        false,
        1,
        vec![
            item(vec![
                para("outer"),
                list(
                    true,
                    2,
                    vec![
                        item(vec![
                            para("inner"),
                            table(even(1), vec![vec![td(0, "t")]]),
                            para("inner tail"),
                        ]),
                        item(vec![para("inner next")]),
                    ],
                ),
                para("outer tail"),
            ]),
            item(vec![para("outer next")]),
        ],
    )]);
    parse_all(&odt);
    let d = xml(odt.part("content.xml"));
    let blocks = content(&d);
    let tags: Vec<&str> = blocks.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(tags, ["list", "table", "list"]);
    let id = blocks[0].attribute((XML_NS, "id")).unwrap();
    let reopened = blocks[2];
    assert_eq!(reopened.attribute((TEXT, "continue-list")), Some(id));

    let outer = elements(reopened);
    let outer_tags: Vec<&str> = outer.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(outer_tags, ["list-header", "list-item"]);
    let header = elements(outer[0]);
    let inner_tags: Vec<&str> = header.iter().map(|n| n.tag_name().name()).collect();
    assert_eq!(inner_tags, ["list", "p"]);
    assert_eq!(style_name(header[0]), Some("Numbering_20_123"));
    assert_eq!(text(header[1]), "outer tail");
    let inner: Vec<(&str, String)> = elements(header[0])
        .into_iter()
        .map(|n| (n.tag_name().name(), text(n)))
        .collect();
    assert_eq!(
        inner,
        [
            ("list-header", "inner tail".to_owned()),
            ("list-item", "inner next".to_owned()),
        ]
    );
    assert_eq!(text(outer[1]), "outer next");

    let tstyle = auto_style(&d, blocks[1], TABLE);
    assert_close(cm(prop(tstyle, FO, "margin-left").unwrap()), 2.54, "indent");
    assert_close(
        cm(prop(tstyle, STYLE, "width").unwrap()),
        16.0 - 2.54,
        "width",
    );
}

#[test]
fn table_in_quote_is_indented_and_cell_content_starts_at_the_cell_edge() {
    let inner = || {
        table(
            vec![0.25, 0.75],
            vec![vec![
                cell(
                    0,
                    vec![para("cell"), list(false, 1, vec![item(vec![para("li")])])],
                ),
                cell(1, vec![table(even(2), vec![vec![td(0, "n"), td(1, "m")]])]),
            ]],
        )
    };
    let odt = export(vec![
        Block::Quote(vec![inner()]),
        para("between"),
        table(even(1), vec![vec![td(0, "top")]]),
    ]);
    parse_all(&odt);
    let d = xml(odt.part("content.xml"));
    let tbls = all(&d, TABLE, "table");
    assert_eq!(tbls.len(), 3);
    let quoted = auto_style(&d, tbls[0], TABLE);
    assert_close(cm(prop(quoted, FO, "margin-left").unwrap()), 1.0, "indent");
    assert_close(cm(prop(quoted, STYLE, "width").unwrap()), 15.0, "width");
    assert_eq!(column_widths(&d, tbls[0]), [3.75, 11.25]);

    // The nested table fills its cell, less the cell's padding, unindented.
    let nested = auto_style(&d, tbls[1], TABLE);
    assert_eq!(prop(nested, FO, "margin-left"), None);
    assert_close(
        cm(prop(nested, STYLE, "width").unwrap()),
        11.25 - 0.38,
        "nested",
    );

    let first = elements(rows(tbls[0])[0])[0];
    let p = child(first, TEXT, "p").unwrap();
    let s = auto_style(&d, p, TEXT);
    assert_eq!(
        s.attribute((STYLE, "parent-style-name")),
        Some("Table_20_Contents"),
        "no Quotations inside a cell"
    );
    let li = all(&d, TEXT, "list-item")[0];
    let s = auto_style(&d, child(li, TEXT, "p").unwrap(), TEXT);
    assert_eq!(
        s.attribute((STYLE, "parent-style-name")),
        Some("List_20_Paragraph")
    );

    let top = auto_style(&d, tbls[2], TABLE);
    assert_eq!(prop(top, FO, "margin-left"), None);
}

#[test]
fn amplification_is_bounded_odt() {
    use common::{build, el, p};
    use yrs::Any;
    let span = |n: f64| vec![("colspan", Any::Number(n)), ("rowspan", Any::Number(n))];
    let mut rows = vec![el(
        "table_row",
        vec![],
        (0..64)
            .map(|_| el("table_cell", span(1000.), vec![p("x")]))
            .collect(),
    )];
    for _ in 0..63 {
        rows.push(el(
            "table_row",
            vec![],
            vec![el(
                "table_cell",
                vec![("rowspan", Any::Number(1000.))],
                vec![p("y")],
            )],
        ));
    }
    let walked = knot_export::walk(&build(vec![el("table", vec![], rows)]));
    let odt = export(walked.doc.blocks);
    assert!(odt.part("content.xml").len() < 2_000_000);
}
