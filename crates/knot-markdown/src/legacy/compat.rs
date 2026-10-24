//! Classifiers shared by the differential compatibility harness
//! (`tests/compat.rs`) and the real-data check (spec "Compatibility
//! guarantees").

use std::fmt::Write as _;
use std::ops::Range;

use knot_crdt::{DocHandle, YrsEngine};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use yrs::types::Attrs;
use yrs::types::text::YChange;
use yrs::{Any, Out, ReadTxn, Text, Transact, Xml, XmlFragment, XmlOut};

use super::{parse, serialise};

/// A named bug class: its name, and whether a document or input is in it.
type Class<T> = (&'static str, fn(&T) -> bool);

/// Named export bug classes. A healthy document in one of them may export
/// differently from the legacy serializer (serializer rule 1); every other
/// healthy document must export byte for byte as before. Each entry is a bug
/// from the spec, and the first entry that matches names the class.
const EXPORT_CLASSES: &[Class<DocHandle>] = &[];

/// Named import bug classes. A healthy Markdown input in one of them may
/// import differently from the legacy importer (importer rule).
///
/// The two classes of text the writer can't express yet (a follow-up) come
/// first, so `import_class` names them whenever they apply: no round trip of
/// such an input can be stable.
const IMPORT_CLASSES: &[Class<str>] = &[
    ("markdown_escaping", markdown_escaping),
    ("leading_hard_break", leading_hard_break),
    (
        "item_first_child_not_paragraph",
        item_first_child_not_paragraph,
    ),
    ("rule_in_container", rule_in_container),
    ("html_between_item_lines", html_between_item_lines),
    (
        "image_after_block_in_tight_item",
        image_after_block_in_tight_item,
    ),
];

/// The export bug class `doc` falls in, if any.
pub fn export_class(doc: &DocHandle) -> Option<&'static str> {
    EXPORT_CLASSES
        .iter()
        .find(|(_, is_in)| is_in(doc))
        .map(|(name, _)| *name)
}

/// The import bug class `md` falls in, if any.
pub fn import_class(md: &str) -> Option<&'static str> {
    IMPORT_CLASSES
        .iter()
        .find(|(_, is_in)| is_in(md))
        .map(|(name, _)| *name)
}

/// The pulldown-cmark events of `md` with their source ranges, read with
/// the importers' options.
fn events(md: &str) -> impl Iterator<Item = (Event<'_>, Range<usize>)> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);
    Parser::new_ext(md, opts).into_offset_iter()
}

/// Text from a backslash escape (`\*`) or a character reference (`&amp;`)
/// outside code. The writer doesn't escape text, which is a non-goal and a
/// follow-up issue, so such text can come back as Markdown syntax. An
/// escaped `|` in a table cell is not in the class: the writer escapes it.
fn markdown_escaping(md: &str) -> bool {
    let mut in_code_block = false;
    let mut in_cell = false;
    events(md).any(|(event, range)| {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Start(Tag::TableCell) => in_cell = true,
            Event::End(TagEnd::TableCell) => in_cell = false,
            Event::Text(text) if !in_code_block => {
                let reference = md[range.clone()] != *text;
                let escape = md[..range.start].ends_with('\\')
                    && text.starts_with(|c: char| c.is_ascii_punctuation())
                    && !(in_cell && text.starts_with('|'));
                return reference || escape;
            }
            _ => {}
        }
        false
    })
}

/// A hard break before any other inline content of its paragraph, heading
/// or tight list item (`- \` then `b`). The writer's `"  \n"` at the start
/// of a line is not a hard break: the same follow-up as `markdown_escaping`.
/// Neither image alt text nor inline HTML counts as content: the importer
/// drops an image that shares its paragraph, and ignores inline HTML.
fn leading_hard_break(md: &str) -> bool {
    let mut at_start = false;
    let mut image_depth = 0usize;
    events(md).any(|(event, _)| {
        match event {
            Event::Start(Tag::Image { .. }) => image_depth += 1,
            Event::End(TagEnd::Image) => image_depth -= 1,
            _ if image_depth > 0 => {}
            Event::HardBreak => return at_start,
            Event::Text(_) | Event::Code(_) | Event::SoftBreak => at_start = false,
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. })
            | Event::End(
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link,
            )
            | Event::InlineHtml(_)
            | Event::TaskListMarker(_) => {}
            // A block starts or ends: the next inline content starts a
            // paragraph.
            _ => at_start = true,
        }
        false
    })
}

/// A list item the legacy importer stored without its leading paragraph:
/// an empty item (`-`), one starting with another block (`- # h`), or one
/// holding only an image or board, which the legacy importer dropped. The
/// new importer puts an empty paragraph first (A7).
fn item_first_child_not_paragraph(md: &str) -> bool {
    fn has_bad_item<T: ReadTxn>(txn: &T, node: &XmlOut) -> bool {
        let XmlOut::Element(el) = node else {
            return false;
        };
        let children: Vec<XmlOut> = (0..el.len(txn)).filter_map(|i| el.get(txn, i)).collect();
        (el.tag().as_ref() == "list_item"
            && children.first().and_then(tag_of).as_deref() != Some("paragraph"))
            || children.iter().any(|child| has_bad_item(txn, child))
    }
    let Ok((doc, _)) = parse(md) else {
        return false;
    };
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    txn.get_xml_fragment("default").is_some_and(|frag| {
        (0..frag.len(&txn))
            .filter_map(|i| frag.get(&txn, i))
            .any(|child| has_bad_item(&txn, &child))
    })
}

/// A rule inside a list item or blockquote, which the legacy importer moved
/// to the top level, after the enclosing container (A9).
fn rule_in_container(md: &str) -> bool {
    let mut depth = 0usize;
    events(md).any(|(event, _)| {
        match event {
            Event::Start(Tag::Item | Tag::BlockQuote(_)) => depth += 1,
            Event::End(TagEnd::Item | TagEnd::BlockQuote(_)) => depth -= 1,
            Event::Rule => return depth > 0,
            _ => {}
        }
        false
    })
}

/// Inline content, an HTML block, then more inline content, all directly in
/// one tight list item (`- a`, `  <!-- c -->`, `  b`). The legacy importer
/// appended the later line to the first paragraph, giving `"ab"` (A3); the
/// new importer keeps two paragraphs.
fn html_between_item_lines(md: &str) -> bool {
    /// What was last seen directly in an open list item.
    #[derive(Clone, Copy, PartialEq)]
    enum Seen {
        Nothing,
        Inline,
        HtmlAfterInline,
        Block,
    }
    // One entry per open block, innermost last: `Some` for a list item.
    let mut open: Vec<Option<Seen>> = Vec::new();
    events(md).any(|(event, _)| {
        let inline = matches!(
            event,
            Event::Text(_)
                | Event::Code(_)
                | Event::InlineHtml(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::Start(
                    Tag::Emphasis
                        | Tag::Strong
                        | Tag::Strikethrough
                        | Tag::Link { .. }
                        | Tag::Image { .. }
                )
        );
        let item = open.last_mut().and_then(Option::as_mut);
        match event {
            _ if inline => {
                if let Some(seen) = item {
                    if *seen == Seen::HtmlAfterInline {
                        return true;
                    }
                    *seen = Seen::Inline;
                }
            }
            Event::Start(Tag::Item) => open.push(Some(Seen::Nothing)),
            Event::Start(tag) => {
                if let Some(seen) = item {
                    let html_after_inline = matches!(tag, Tag::HtmlBlock)
                        && matches!(*seen, Seen::Inline | Seen::HtmlAfterInline);
                    *seen = if html_after_inline {
                        Seen::HtmlAfterInline
                    } else {
                        Seen::Block
                    };
                }
                open.push(None);
            }
            Event::End(
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::Image,
            ) => {}
            Event::End(_) => {
                open.pop();
            }
            Event::Rule => {
                if let Some(seen) = item {
                    *seen = Seen::Block;
                }
            }
            _ => {}
        }
        false
    })
}

/// A lone image or board as a run of its own directly in a tight list item,
/// after another block of the item (`- a`, `  # h`, `  ![x](y.png)`): the
/// run has exactly one image and no text, code or line break outside its alt
/// text. The legacy importer dropped it, because promotion needs a paragraph
/// on the stack (A2); the new importer keeps it as an `image` or
/// `excalidraw_board` block. A run that also has text imported the same way
/// before (the image is dropped from a paragraph with text), and so does one
/// with no image or several (it yields nothing).
fn image_after_block_in_tight_item(md: &str) -> bool {
    /// An open list item: whether one of its blocks was seen, and the run
    /// being read directly in it.
    #[derive(Default)]
    struct Item {
        after_block: bool,
        images: usize,
        image_depth: usize,
        content: bool,
    }
    impl Item {
        /// Ends the run directly in this item: whether it was a lone image
        /// after a block.
        fn end_run(&mut self) -> bool {
            let lone_image = self.after_block && self.images == 1 && !self.content;
            *self = Self {
                after_block: true,
                ..Self::default()
            };
            lone_image
        }
    }
    // One entry per open block, innermost last: `Some` for a list item.
    let mut open: Vec<Option<Item>> = Vec::new();
    events(md).any(|(event, _)| {
        let item = open.last_mut().and_then(Option::as_mut);
        match event {
            Event::Start(Tag::Image { .. }) => {
                if let Some(item) = item {
                    item.images += 1;
                    item.image_depth += 1;
                }
            }
            Event::End(TagEnd::Image) => {
                if let Some(item) = item {
                    item.image_depth = item.image_depth.saturating_sub(1);
                }
            }
            // The importer writes alt text nowhere, but code and line breaks
            // even inside an image.
            Event::Text(_) => {
                if let Some(item) = item
                    && item.image_depth == 0
                {
                    item.content = true;
                }
            }
            Event::Code(_) | Event::SoftBreak | Event::HardBreak => {
                if let Some(item) = item {
                    item.content = true;
                }
            }
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. })
            | Event::End(
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link,
            )
            | Event::InlineHtml(_)
            | Event::TaskListMarker(_) => {}
            // A block of the item starts, or a leaf block sits in it: the
            // run before it ends.
            Event::Start(tag) => {
                if let Some(item) = item
                    && item.end_run()
                {
                    return true;
                }
                open.push(matches!(tag, Tag::Item).then(Item::default));
            }
            Event::Rule | Event::Html(_) => {
                if let Some(item) = item
                    && item.end_run()
                {
                    return true;
                }
            }
            // A block ends; at the end of an item, so does its last run.
            Event::End(_) => {
                if let Some(Some(mut item)) = open.pop()
                    && item.end_run()
                {
                    return true;
                }
            }
            _ => {}
        }
        false
    })
}

/// The legacy export of `doc` when `doc` is healthy: the legacy export
/// succeeds and is a fixed point, `serialise(parse(m)) == m`, both legacy.
pub fn healthy(doc: &DocHandle) -> Option<String> {
    let md = serialise(&YrsEngine, doc).ok()?;
    let (again, _) = parse(&md).ok()?;
    (serialise(&YrsEngine, &again).ok()? == md).then_some(md)
}

/// The block group of `tools/schema.json`.
const BLOCKS: &[&str] = &[
    "paragraph",
    "heading",
    "blockquote",
    "code_block",
    "horizontal_rule",
    "bullet_list",
    "ordered_list",
    "excalidraw_board",
    "image",
    "attachment",
    "table",
];

/// What a node may contain, per `tools/schema.json`.
enum Content {
    /// `block*`, `block+` (`min: 1`) or `paragraph block*`.
    Blocks { min: usize, paragraph_first: bool },
    /// One or more of these nodes.
    OneOrMore(&'static [&'static str]),
    /// `inline*`: text and hard breaks.
    Inline,
    /// `text*`.
    Text,
    /// Nothing.
    Empty,
}

fn content(tag: &str) -> Option<Content> {
    Some(match tag {
        "paragraph" | "heading" => Content::Inline,
        "blockquote" | "table_cell" | "table_header" => Content::Blocks {
            min: 1,
            paragraph_first: false,
        },
        "list_item" => Content::Blocks {
            min: 1,
            paragraph_first: true,
        },
        "code_block" => Content::Text,
        "bullet_list" | "ordered_list" => Content::OneOrMore(&["list_item"]),
        "table" => Content::OneOrMore(&["table_row"]),
        "table_row" => Content::OneOrMore(&["table_cell", "table_header"]),
        "horizontal_rule" | "excalidraw_board" | "image" | "attachment" | "hard_break" => {
            Content::Empty
        }
        _ => return None,
    })
}

/// Violations of the content rules of `tools/schema.json`, one message per
/// offending node, such as
/// `doc > bullet_list[0] > list_item[0]: hard_break[1] not allowed in list_item`.
///
/// It checks `list_item: paragraph block*`, `table_cell`/`table_header:
/// block+`, `code_block: text*`, that no text or hard break sits directly
/// in a block container, and the remaining content rules and node names.
/// The top-level fragment is `block*`: empty Markdown imports to an empty
/// document, and the editor fills it on load.
pub fn schema_violations(doc: &yrs::Doc) -> Vec<String> {
    let txn = doc.transact();
    let mut out = Vec::new();
    if let Some(frag) = txn.get_xml_fragment("default") {
        let children: Vec<XmlOut> = (0..frag.len(&txn))
            .filter_map(|i| frag.get(&txn, i))
            .collect();
        let doc_content = Content::Blocks {
            min: 0,
            paragraph_first: false,
        };
        check(&txn, "doc", &doc_content, &children, "doc", &mut out);
    }
    out
}

fn tag_of(node: &XmlOut) -> Option<String> {
    match node {
        XmlOut::Element(el) => Some(el.tag().to_string()),
        _ => None,
    }
}

fn check<T: ReadTxn>(
    txn: &T,
    tag: &str,
    rule: &Content,
    children: &[XmlOut],
    path: &str,
    out: &mut Vec<String>,
) {
    // Whether `child` may sit in this node. Unknown elements pass here and
    // are reported once, as unknown, below.
    let allowed = |child: &XmlOut| -> bool {
        let child_tag = tag_of(child);
        if let Some(t) = &child_tag
            && content(t).is_none()
        {
            return true;
        }
        match rule {
            Content::Blocks { .. } => child_tag.is_some_and(|t| BLOCKS.contains(&t.as_str())),
            Content::OneOrMore(names) => child_tag.is_some_and(|t| names.contains(&t.as_str())),
            Content::Inline => match child {
                XmlOut::Text(_) => true,
                XmlOut::Element(_) => child_tag.as_deref() == Some("hard_break"),
                XmlOut::Fragment(_) => false,
            },
            Content::Text => matches!(child, XmlOut::Text(_)),
            Content::Empty => false,
        }
    };

    let min = match rule {
        Content::Blocks { min, .. } => *min,
        Content::OneOrMore(_) => 1,
        _ => 0,
    };
    if children.len() < min {
        out.push(format!("{path}: empty {tag}"));
    }
    if let Content::Blocks {
        paragraph_first: true,
        ..
    } = rule
        && !children.is_empty()
        && tag_of(&children[0]).as_deref() != Some("paragraph")
    {
        out.push(format!("{path}: {tag} doesn't start with a paragraph"));
    }
    for (i, child) in children.iter().enumerate() {
        if !allowed(child) {
            let what = match child {
                XmlOut::Element(el) => el.tag().to_string(),
                XmlOut::Text(_) => "text".to_string(),
                XmlOut::Fragment(_) => "fragment".to_string(),
            };
            out.push(format!("{path}: {what}[{i}] not allowed in {tag}"));
        }
    }

    for (i, child) in children.iter().enumerate() {
        let XmlOut::Element(el) = child else {
            continue;
        };
        let child_tag = el.tag().to_string();
        let child_path = format!("{path} > {child_tag}[{i}]");
        match content(&child_tag) {
            None => out.push(format!("{child_path}: unknown node {child_tag}")),
            Some(child_rule) => {
                let grandchildren: Vec<XmlOut> =
                    (0..el.len(txn)).filter_map(|j| el.get(txn, j)).collect();
                check(
                    txn,
                    &child_tag,
                    &child_rule,
                    &grandchildren,
                    &child_path,
                    out,
                );
            }
        }
    }
}

/// A canonical dump of a document, for comparing imports (importer rule):
/// - one line per element: its tag and its attributes sorted by name, with
///   values as strings, so `2` and `"2"` agree;
/// - one line per text run: the text, then its marks sorted by name, where
///   `{}` and `true` both mean "set" and other values are written with their
///   keys sorted. Adjacent runs of one `XmlText` with equal marks are one
///   line, and empty runs are left out.
pub fn normalised(doc: &yrs::Doc) -> String {
    let txn = doc.transact();
    let mut out = String::new();
    if let Some(frag) = txn.get_xml_fragment("default") {
        for i in 0..frag.len(&txn) {
            if let Some(child) = frag.get(&txn, i) {
                dump(&txn, &child, 0, &mut out);
            }
        }
    }
    out
}

fn dump<T: ReadTxn>(txn: &T, node: &XmlOut, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match node {
        XmlOut::Element(el) => {
            let mut attrs: Vec<(String, String)> = el
                .attributes(txn)
                .map(|(k, v)| (k.to_string(), out_string(txn, v)))
                .collect();
            attrs.sort();
            let _ = write!(out, "{pad}<{}", el.tag());
            for (k, v) in attrs {
                let _ = write!(out, " {k}={v:?}");
            }
            out.push_str(">\n");
            for i in 0..el.len(txn) {
                if let Some(child) = el.get(txn, i) {
                    dump(txn, &child, depth + 1, out);
                }
            }
        }
        XmlOut::Text(t) => {
            // (is text, content, marks); embeds never merge with text.
            let mut runs: Vec<(bool, String, Vec<String>)> = Vec::new();
            for chunk in t.diff(txn, YChange::identity) {
                let marks = chunk
                    .attributes
                    .as_deref()
                    .map(marks_of)
                    .unwrap_or_default();
                let (is_text, piece) = match chunk.insert {
                    Out::Any(Any::String(s)) => (true, s.to_string()),
                    other => (false, out_string(txn, other)),
                };
                match runs.last_mut() {
                    Some((true, text, m)) if is_text && *m == marks => text.push_str(&piece),
                    _ => runs.push((is_text, piece, marks)),
                }
            }
            for (is_text, piece, marks) in runs {
                if is_text && piece.is_empty() {
                    continue;
                }
                if is_text {
                    let _ = write!(out, "{pad}{piece:?}");
                } else {
                    let _ = write!(out, "{pad}embed {piece:?}");
                }
                for m in marks {
                    let _ = write!(out, " {m}");
                }
                out.push('\n');
            }
        }
        XmlOut::Fragment(f) => {
            let _ = writeln!(out, "{pad}<#fragment>");
            for i in 0..f.len(txn) {
                if let Some(child) = f.get(txn, i) {
                    dump(txn, &child, depth + 1, out);
                }
            }
        }
    }
}

fn marks_of(attrs: &Attrs) -> Vec<String> {
    let mut marks: Vec<String> = attrs
        .iter()
        .filter_map(|(k, v)| match v {
            Any::Null | Any::Undefined => None,
            Any::Bool(true) => Some(k.to_string()),
            Any::Map(m) if m.is_empty() => Some(k.to_string()),
            other => Some(format!("{k}={}", any_string(other))),
        })
        .collect();
    marks.sort();
    marks
}

fn out_string<T: ReadTxn>(txn: &T, value: Out) -> String {
    match value {
        Out::Any(a) => any_string(&a),
        other => other.to_string(txn),
    }
}

/// `Any` as a string: scalars as `Display` writes them, maps with their keys
/// sorted.
fn any_string(value: &Any) -> String {
    match value {
        Any::Map(m) => {
            let mut entries: Vec<(&String, &Any)> = m.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| format!("{k}: {:?}", any_string(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Any::Array(items) => {
            let inner: Vec<String> = items
                .iter()
                .map(|v| format!("{:?}", any_string(v)))
                .collect();
            format!("[{}]", inner.join(", "))
        }
        other => other.to_string(),
    }
}
