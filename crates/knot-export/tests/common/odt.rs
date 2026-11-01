//! ODT-specific roxmltree helpers shared by the ODT test files.

use std::collections::HashMap;

use roxmltree::{Document, Node};

pub const OFFICE: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
pub const STYLE: &str = "urn:oasis:names:tc:opendocument:xmlns:style:1.0";
pub const TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
pub const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
pub const FO: &str = "urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0";
pub const XLINK: &str = "http://www.w3.org/1999/xlink";
pub const META: &str = "urn:oasis:names:tc:opendocument:xmlns:meta:1.0";
pub const DC: &str = "http://purl.org/dc/elements/1.1/";
pub const MANIFEST: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// The parts of an exported package.
pub struct Odt(pub HashMap<String, Vec<u8>>);

impl Odt {
    pub fn part(&self, name: &str) -> &str {
        let bytes = self
            .0
            .get(name)
            .unwrap_or_else(|| panic!("missing part {name}"));
        std::str::from_utf8(bytes).expect("UTF-8 part")
    }
}

pub fn elements<'a, 'i>(n: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    n.children().filter(Node::is_element).collect()
}

/// First child element `ns:name`.
pub fn child<'a, 'i>(n: Node<'a, 'i>, ns: &str, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name((ns, name)))
}

/// Child elements `ns:name`.
pub fn children<'a, 'i>(n: Node<'a, 'i>, ns: &str, name: &str) -> Vec<Node<'a, 'i>> {
    n.children()
        .filter(|c| c.has_tag_name((ns, name)))
        .collect()
}

/// Every element `ns:name` in a part, in document order.
pub fn all<'a, 'i>(d: &'a Document<'i>, ns: &str, name: &str) -> Vec<Node<'a, 'i>> {
    d.descendants()
        .filter(|n| n.has_tag_name((ns, name)))
        .collect()
}

/// The text a reader sees below a node: character data, with `text:s` as
/// its spaces, `text:tab` as a tab and `text:line-break` as a line feed.
pub fn text(n: Node) -> String {
    let mut s = String::new();
    for d in n.descendants() {
        if d.is_text() {
            s.push_str(d.text().unwrap_or(""));
        } else if d.has_tag_name((TEXT, "s")) {
            let c: usize = d.attribute((TEXT, "c")).map_or(1, |c| c.parse().unwrap());
            s.extend(std::iter::repeat_n(' ', c));
        } else if d.has_tag_name((TEXT, "tab")) {
            s.push('\t');
        } else if d.has_tag_name((TEXT, "line-break")) {
            s.push('\n');
        }
    }
    s
}

/// `office:text` of `content.xml`.
pub fn office_text<'a, 'i>(d: &'a Document<'i>) -> Node<'a, 'i> {
    d.descendants()
        .find(|n| n.has_tag_name((OFFICE, "text")))
        .expect("office:text")
}

/// `office:text` children after the title and subtitle.
pub fn content<'a, 'i>(d: &'a Document<'i>) -> Vec<Node<'a, 'i>> {
    elements(office_text(d))[2..].to_vec()
}

/// `text:style-name` of a paragraph, heading, span, link or list.
pub fn style_name<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    n.attribute((TEXT, "style-name"))
}

/// The style (`style:style`, a list style, a page layout, ...) named `name`
/// in a part.
pub fn style<'a, 'i>(d: &'a Document<'i>, name: &str) -> Node<'a, 'i> {
    d.descendants()
        .find(|n| n.attribute((STYLE, "name")) == Some(name))
        .unwrap_or_else(|| panic!("no style {name}"))
}

/// Attribute `ns:name` on one of a style's `*-properties` children.
pub fn prop<'a>(s: Node<'a, '_>, ns: &str, name: &str) -> Option<&'a str> {
    s.children()
        .filter(|c| c.is_element() && c.tag_name().name().ends_with("-properties"))
        .find_map(|c| c.attribute((ns, name)))
}

/// A length such as "16.000cm" as centimetres.
pub fn cm(len: &str) -> f64 {
    len.strip_suffix("cm")
        .unwrap_or_else(|| panic!("{len} is not in cm"))
        .parse()
        .unwrap()
}

/// How many `text:list` elements enclose a node (not counting the node).
pub fn list_depth(n: Node) -> usize {
    n.ancestors()
        .skip(1)
        .filter(|a| a.has_tag_name((TEXT, "list")))
        .count()
}
