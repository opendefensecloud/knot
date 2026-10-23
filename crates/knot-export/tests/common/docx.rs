//! DOCX-specific roxmltree helpers shared by the DOCX test files.

use std::collections::HashMap;

use roxmltree::{Document, Node};

use super::xml;

pub const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
pub const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// The parts of an exported package.
pub struct Docx(pub HashMap<String, Vec<u8>>);

impl Docx {
    pub fn part(&self, name: &str) -> &str {
        let bytes = self
            .0
            .get(name)
            .unwrap_or_else(|| panic!("missing part {name}"));
        std::str::from_utf8(bytes).expect("UTF-8 part")
    }
}

pub struct Rel {
    pub kind: String,
    pub target: String,
    pub external: bool,
}

/// `word/_rels/document.xml.rels` by relationship id.
pub fn rels(docx: &Docx) -> HashMap<String, Rel> {
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

pub fn elements<'a, 'i>(n: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    n.children().filter(Node::is_element).collect()
}

/// First `w:` child element with this local name.
pub fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name((W, name)))
}

pub fn wattr<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.attribute((W, name))
}

pub fn val<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    wattr(n, "val")
}

pub fn p_style<'a>(p: Node<'a, '_>) -> Option<&'a str> {
    child(p, "pPr")
        .and_then(|ppr| child(ppr, "pStyle"))
        .and_then(val)
}

pub fn r_style<'a>(r: Node<'a, '_>) -> Option<&'a str> {
    child(r, "rPr")
        .and_then(|rpr| child(rpr, "rStyle"))
        .and_then(val)
}

/// Concatenated `w:t` text below a node.
pub fn text(n: Node) -> String {
    n.descendants()
        .filter(|d| d.has_tag_name((W, "t")))
        .filter_map(|t| t.text())
        .collect()
}

pub fn body<'a, 'i>(d: &'a Document<'i>) -> Node<'a, 'i> {
    d.descendants()
        .find(|n| n.has_tag_name((W, "body")))
        .expect("w:body")
}

/// Body children after the title and subtitle, without the closing `sectPr`.
pub fn content<'a, 'i>(d: &'a Document<'i>) -> Vec<Node<'a, 'i>> {
    let kids = elements(body(d));
    assert_eq!(kids.last().unwrap().tag_name().name(), "sectPr");
    kids[2..kids.len() - 1].to_vec()
}

pub fn style<'a, 'i>(styles: &'a Document<'i>, id: &str) -> Node<'a, 'i> {
    styles
        .descendants()
        .find(|n| n.has_tag_name((W, "style")) && wattr(*n, "styleId") == Some(id))
        .unwrap_or_else(|| panic!("no style {id}"))
}

/// Value of `w:style/w:name`.
pub fn style_name<'a>(s: Node<'a, '_>) -> &'a str {
    child(s, "name").and_then(val).expect("style name")
}

/// ECMA-376 (wml.xsd) order of the children Word checks strictly.
pub const SCHEMA_ORDER: &[(&str, &[&str])] = &[
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
