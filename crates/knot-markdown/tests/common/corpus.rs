//! The differential corpus (spec "Compatibility guarantees", 1):
//! - `generated()`: deterministic combinations of every node and mark,
//!   containers nested up to four deep, with every attribute and mark both in
//!   the editor's and in the importer's representation;
//! - `random(seed, n)`: seeded random documents that mix representations
//!   within one document and include damaged and unknown content;
//! - `markdown_inputs()`: every fixture and probe file.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use knot_crdt::DocHandle;
use yrs::Any;

use super::{N, Run, build, el, ela, hard_break, p, run, text};

pub struct Case {
    pub name: String,
    pub doc: DocHandle,
}

/// How stored attribute and mark values are represented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// As the editor (y-prosemirror) stores them: numbers and booleans, `{}`
    /// for a mark without attributes, links with `target`, `rel`, `class` and
    /// `title`, table cells with `colspan`, `rowspan` and `colwidth`.
    Editor,
    /// As the Markdown importer stores them: strings, and `true` for a mark
    /// without attributes. Attributes the importer never writes are strings
    /// too, so every attribute exists in both representations.
    Import,
}

impl Shape {
    pub const ALL: [Shape; 2] = [Shape::Editor, Shape::Import];

    pub fn name(self) -> &'static str {
        match self {
            Shape::Editor => "editor",
            Shape::Import => "import",
        }
    }

    /// An integer attribute (`level`, `start`, `colspan`, `size`).
    pub fn int(self, n: i64) -> Any {
        match self {
            Shape::Editor => Any::Number(n as f64),
            Shape::Import => Any::from(n.to_string()),
        }
    }

    /// A boolean attribute (`checked`).
    pub fn boolean(self, b: bool) -> Any {
        match self {
            Shape::Editor => Any::Bool(b),
            Shape::Import => Any::from(if b { "true" } else { "false" }),
        }
    }

    /// The value of a mark without attributes.
    pub fn flag(self) -> Any {
        match self {
            Shape::Editor => Any::Map(Arc::new(HashMap::new())),
            Shape::Import => Any::Bool(true),
        }
    }

    pub fn link(self, href: &str, title: Option<&str>) -> Any {
        let mut m = HashMap::new();
        m.insert("href".to_string(), Any::from(href));
        match self {
            Shape::Editor => {
                m.insert("target".into(), Any::from("_blank"));
                m.insert("rel".into(), Any::from("noopener noreferrer"));
                m.insert("class".into(), Any::Null);
                m.insert("title".into(), title.map_or(Any::Null, Any::from));
            }
            Shape::Import => {
                if let Some(t) = title {
                    m.insert("title".into(), Any::from(t));
                }
            }
        }
        Any::Map(Arc::new(m))
    }

    /// A cell's column width in pixels (prosemirror-tables stores one width
    /// per spanned column).
    pub fn colwidth(self, px: i64) -> Any {
        match self {
            Shape::Editor => Any::Array(Arc::from(vec![Any::Number(px as f64)])),
            Shape::Import => Any::from(px.to_string()),
        }
    }
}

const FLAG_MARKS: [&str; 5] = ["bold", "italic", "code", "strike", "underline"];

const BOARD_ID: &str = "11111111-2222-3333-4444-555555555555";
const BLOB_URL: &str = "/api/blobs/22222222-3333-4444-5555-666666666666";
const DOC_URL: &str = "knot://doc/33333333-4444-5555-6666-777777777777";
const USER_URL: &str = "knot://user/44444444-5555-6666-7777-888888888888";
const TIME_URL: &str = "knot://time/2026-06-04T17:00:00Z";
const IMAGE_URL: &str = "https://example.com/a.png";

fn image(src: &str, alt: Option<&str>, title: Option<&str>) -> N {
    let mut attrs = vec![("src", Any::from(src))];
    if let Some(a) = alt {
        attrs.push(("alt", Any::from(a)));
    }
    if let Some(t) = title {
        attrs.push(("title", Any::from(t)));
    }
    ela("image", attrs, Vec::new())
}

fn board(label: Option<&str>) -> N {
    let mut attrs = vec![("board_id", Any::from(BOARD_ID))];
    if let Some(l) = label {
        attrs.push(("label", Any::from(l)));
    }
    ela("excalidraw_board", attrs, Vec::new())
}

fn rule() -> N {
    el("horizontal_rule", Vec::new())
}

fn item(children: Vec<N>) -> N {
    el("list_item", children)
}

fn bullets(items: Vec<N>) -> N {
    el("bullet_list", items)
}

fn quote(children: Vec<N>) -> N {
    el("blockquote", children)
}

fn row(cells: Vec<N>) -> N {
    el("table_row", cells)
}

fn marked(s: &str, marks: Vec<(&'static str, Any)>) -> N {
    N::Text(vec![run(s, marks)])
}

/// The containers `generated()` nests and combines.
#[derive(Clone, Copy)]
enum Container {
    Quote,
    Bullet,
    Task,
    Ordered,
    OrderedTask,
    Cell,
}

const CONTAINERS: [Container; 6] = [
    Container::Quote,
    Container::Bullet,
    Container::Task,
    Container::Ordered,
    Container::OrderedTask,
    Container::Cell,
];

impl Container {
    fn name(self) -> &'static str {
        match self {
            Container::Quote => "quote",
            Container::Bullet => "bullet",
            Container::Task => "task",
            Container::Ordered => "ordered",
            Container::OrderedTask => "ordered-task",
            Container::Cell => "cell",
        }
    }
}

/// Builds `generated()`'s documents for one representation.
struct Gen {
    shape: Shape,
}

impl Gen {
    fn heading(&self, level: i64, inlines: Vec<N>) -> N {
        ela("heading", vec![("level", self.shape.int(level))], inlines)
    }

    fn ordered(&self, start: i64, items: Vec<N>) -> N {
        ela(
            "ordered_list",
            vec![("start", self.shape.int(start))],
            items,
        )
    }

    fn task(&self, checked: bool, children: Vec<N>) -> N {
        ela(
            "list_item",
            vec![("checked", self.shape.boolean(checked))],
            children,
        )
    }

    fn code(&self, language: Option<&str>, body: &str) -> N {
        let attrs = language
            .map(|l| vec![("language", Any::from(l))])
            .unwrap_or_default();
        let children = if body.is_empty() {
            Vec::new()
        } else {
            vec![text(body)]
        };
        ela("code_block", attrs, children)
    }

    fn attachment(&self, name: Option<&str>) -> N {
        let mut attrs = vec![("url", Any::from(BLOB_URL))];
        if let Some(n) = name {
            attrs.push(("name", Any::from(n)));
            attrs.push(("size", self.shape.int(2048)));
            attrs.push(("contentType", Any::from("application/pdf")));
        }
        ela("attachment", attrs, Vec::new())
    }

    fn cell(
        &self,
        tag: &'static str,
        align: Option<&str>,
        colwidth: Option<i64>,
        children: Vec<N>,
    ) -> N {
        let mut attrs = vec![
            ("colspan", self.shape.int(1)),
            ("rowspan", self.shape.int(1)),
        ];
        if let Some(px) = colwidth {
            attrs.push(("colwidth", self.shape.colwidth(px)));
        }
        if let Some(a) = align {
            attrs.push(("align", Any::from(a)));
        }
        ela(tag, attrs, children)
    }

    /// A header row and one body row holding `body`.
    fn table_around(&self, body: Vec<N>) -> N {
        el(
            "table",
            vec![
                row(vec![self.cell("table_header", None, None, vec![p("h")])]),
                row(vec![self.cell("table_cell", None, None, body)]),
            ],
        )
    }

    /// Paragraph content: plain, every mark alone, in text, in pairs and all
    /// together, links of every kind, and hard breaks.
    fn inlines(&self) -> Vec<(String, Vec<N>)> {
        let s = self.shape;
        let mut v: Vec<(String, Vec<N>)> = vec![
            ("plain".into(), vec![text("hello world")]),
            ("empty".into(), Vec::new()),
            ("unicode".into(), vec![text("Grüße – 日本 😀")]),
            (
                "specials".into(),
                vec![text("a * b _c_ `d` [e] # f | g \\ h <i> 1. j")],
            ),
            (
                "hard-break".into(),
                vec![text("a"), hard_break(), text("b")],
            ),
            ("hard-break-last".into(), vec![text("a"), hard_break()]),
            ("hard-break-first".into(), vec![hard_break(), text("b")]),
            (
                "marked-hard-break".into(),
                vec![
                    marked("a", vec![("bold", s.flag())]),
                    hard_break(),
                    marked("b", vec![("bold", s.flag())]),
                ],
            ),
            (
                "adjacent-marks".into(),
                vec![N::Text(vec![
                    run("a", vec![("bold", s.flag())]),
                    run("b", vec![("italic", s.flag())]),
                ])],
            ),
            ("two-texts".into(), vec![text("a"), text("b")]),
        ];
        for m in FLAG_MARKS {
            v.push((m.into(), vec![marked("x", vec![(m, s.flag())])]));
            v.push((
                format!("{m}-in-text"),
                vec![N::Text(vec![
                    run("a ", Vec::new()),
                    run("x", vec![(m, s.flag())]),
                    run(" b", Vec::new()),
                ])],
            ));
        }
        for (name, label, href, title) in [
            ("link", "x", "https://example.com", None),
            ("link-title", "x", "https://example.com/a", Some("A title")),
            ("doc-link", "Page", DOC_URL, None),
            ("blob-link", "file", BLOB_URL, None),
            ("mention", "@Alice", USER_URL, None),
            ("datetime", "Jun 4", TIME_URL, None),
        ] {
            v.push((
                name.into(),
                vec![marked(label, vec![("link", s.link(href, title))])],
            ));
        }
        let all: Vec<(&'static str, Any)> = FLAG_MARKS
            .iter()
            .map(|m| (*m, s.flag()))
            .chain([("link", s.link("https://example.com", None))])
            .collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                v.push((
                    format!("{}+{}", a.0, b.0),
                    vec![marked("x", vec![a.clone(), b.clone()])],
                ));
            }
        }
        v.push(("all-marks".into(), vec![marked("x", all)]));
        v
    }

    /// Every leaf block, and tables in every layout.
    fn leaves(&self) -> Vec<(String, N)> {
        let mut v: Vec<(String, N)> = (1..=6)
            .map(|level| {
                (
                    format!("heading-{level}"),
                    self.heading(level, vec![text("title")]),
                )
            })
            .collect();
        let th = |align, kids| self.cell("table_header", align, None, kids);
        let td = |align, kids| self.cell("table_cell", align, None, kids);
        v.extend([
            ("code".into(), self.code(None, "x")),
            (
                "code-rust".into(),
                self.code(Some("rust"), "fn main() {\n    let x = 1;\n}"),
            ),
            ("code-empty".into(), self.code(None, "")),
            ("code-blank-line".into(), self.code(Some("text"), "a\n\nb")),
            ("rule".into(), rule()),
            ("image".into(), image(IMAGE_URL, None, None)),
            ("image-alt".into(), image(IMAGE_URL, Some("alt text"), None)),
            (
                "image-title".into(),
                image(IMAGE_URL, Some("alt"), Some("A \"quoted\" title")),
            ),
            ("image-blob".into(), image(BLOB_URL, Some("pic"), None)),
            ("attachment".into(), self.attachment(Some("report.pdf"))),
            ("attachment-unnamed".into(), self.attachment(None)),
            ("board-label".into(), board(Some("Architecture"))),
            ("board-default-label".into(), board(Some("Diagram"))),
            ("board".into(), board(None)),
            (
                "table".into(),
                el(
                    "table",
                    vec![
                        row(vec![th(None, vec![p("h1")]), th(None, vec![p("h2")])]),
                        row(vec![td(None, vec![p("c1")]), td(None, vec![p("c2")])]),
                    ],
                ),
            ),
            (
                "table-aligned".into(),
                el(
                    "table",
                    vec![
                        row(vec![
                            th(Some("left"), vec![p("l")]),
                            th(Some("center"), vec![p("c")]),
                            th(Some("right"), vec![p("r")]),
                            th(None, vec![p("n")]),
                        ]),
                        row(vec![
                            td(Some("left"), vec![p("1")]),
                            td(Some("center"), vec![p("2")]),
                            td(Some("right"), vec![p("3")]),
                            td(None, vec![p("4")]),
                        ]),
                    ],
                ),
            ),
            (
                "table-no-header".into(),
                el(
                    "table",
                    vec![
                        row(vec![td(None, vec![p("a")]), td(None, vec![p("b")])]),
                        row(vec![td(None, vec![p("c")]), td(None, vec![p("d")])]),
                    ],
                ),
            ),
            (
                "table-header-column".into(),
                el(
                    "table",
                    vec![
                        row(vec![th(None, vec![p("k")]), td(None, vec![p("v")])]),
                        row(vec![th(None, vec![p("k2")]), td(None, vec![p("v2")])]),
                    ],
                ),
            ),
            (
                "table-multi-block-cell".into(),
                self.table_around(vec![p("a"), p("b")]),
            ),
            ("table-pipe".into(), self.table_around(vec![p("a | b")])),
            (
                "table-marks".into(),
                self.table_around(vec![el(
                    "paragraph",
                    vec![N::Text(vec![
                        run("bold", vec![("bold", self.shape.flag())]),
                        run(" and ", Vec::new()),
                        run(
                            "link",
                            vec![("link", self.shape.link("https://example.com", None))],
                        ),
                    ])],
                )]),
            ),
            (
                "table-colwidth".into(),
                el(
                    "table",
                    vec![
                        row(vec![
                            self.cell("table_header", None, Some(120), vec![p("h1")]),
                            self.cell("table_header", None, Some(80), vec![p("h2")]),
                        ]),
                        row(vec![
                            self.cell("table_cell", None, Some(120), vec![p("c1")]),
                            self.cell("table_cell", None, Some(80), vec![p("c2")]),
                        ]),
                    ],
                ),
            ),
            (
                // Exactly what the importer writes: no span or width attributes.
                "table-bare".into(),
                el(
                    "table",
                    vec![
                        row(vec![el("table_header", vec![p("h")])]),
                        row(vec![el("table_cell", vec![p("c")])]),
                    ],
                ),
            ),
        ]);
        v
    }

    /// The blocks combined in pairs, at the top level and in containers.
    fn pair_blocks(&self) -> Vec<(&'static str, N)> {
        vec![
            ("p", p("para")),
            ("empty-p", el("paragraph", Vec::new())),
            ("heading", self.heading(2, vec![text("head")])),
            ("code", self.code(Some("sh"), "ls")),
            ("rule", rule()),
            ("image", image(IMAGE_URL, Some("img"), None)),
            ("attachment", self.attachment(Some("file.pdf"))),
            ("board", board(Some("Board"))),
            ("table", self.table_around(vec![p("cell")])),
            ("bullets", bullets(vec![item(vec![p("b")])])),
            ("ordered-3", self.ordered(3, vec![item(vec![p("o")])])),
            ("tasks", bullets(vec![self.task(true, vec![p("t")])])),
            ("quote", quote(vec![p("q")])),
        ]
    }

    /// `container` holding `children` directly (a list holds them in its
    /// only item).
    fn wrap(&self, container: Container, depth: usize, children: Vec<N>) -> N {
        match container {
            Container::Quote => quote(children),
            Container::Bullet => bullets(vec![item(children)]),
            Container::Task => bullets(vec![self.task(depth.is_multiple_of(2), children)]),
            Container::Ordered => self.ordered([1, 3, 10, 2][depth % 4], vec![item(children)]),
            Container::OrderedTask => {
                self.ordered(1, vec![self.task(!depth.is_multiple_of(2), children)])
            }
            Container::Cell => self.table_around(children),
        }
    }

    /// `container` around `inner` as the editor nests content: a list item
    /// starts with its own paragraph and has a sibling item after it.
    fn nest(&self, container: Container, depth: usize, inner: Vec<N>) -> N {
        let first = |kids: Vec<N>| {
            let mut all = vec![p(&format!("item {depth}"))];
            all.extend(kids);
            all
        };
        let next = || item(vec![p(&format!("next {depth}"))]);
        match container {
            Container::Quote => quote(inner),
            Container::Bullet => bullets(vec![item(first(inner)), next()]),
            Container::Task => bullets(vec![
                self.task(depth.is_multiple_of(2), first(inner)),
                self.task(!depth.is_multiple_of(2), vec![p(&format!("next {depth}"))]),
            ]),
            Container::Ordered => {
                self.ordered([1, 3, 10, 2][depth % 4], vec![item(first(inner)), next()])
            }
            Container::OrderedTask => self.ordered(
                1,
                vec![self.task(!depth.is_multiple_of(2), first(inner)), next()],
            ),
            Container::Cell => self.table_around(inner),
        }
    }

    /// List and quote shapes the combinations don't reach: several items,
    /// empty items, wide ordered markers, and the quote shapes that
    /// round-trip today (P2).
    fn lists(&self) -> Vec<(&'static str, Vec<N>)> {
        let empty = || el("paragraph", Vec::new());
        vec![
            (
                "bullets-3",
                vec![bullets(vec![
                    item(vec![p("a")]),
                    item(vec![p("b")]),
                    item(vec![p("c")]),
                ])],
            ),
            (
                "empty-item-middle",
                vec![bullets(vec![
                    item(vec![p("a")]),
                    item(vec![empty()]),
                    item(vec![p("c")]),
                ])],
            ),
            ("empty-bullet", vec![bullets(vec![item(vec![empty()])])]),
            (
                "empty-task-unchecked",
                vec![bullets(vec![self.task(false, vec![empty()])])],
            ),
            (
                "empty-task-checked",
                vec![bullets(vec![self.task(true, vec![empty()])])],
            ),
            (
                "empty-ordered-2",
                vec![self.ordered(2, vec![item(vec![empty()])])],
            ),
            (
                "tasks-mixed",
                vec![bullets(vec![
                    self.task(false, vec![p("a")]),
                    self.task(true, vec![p("b")]),
                    item(vec![p("c")]),
                ])],
            ),
            (
                "ordered-9-10",
                vec![self.ordered(
                    9,
                    vec![
                        item(vec![p("a")]),
                        item(vec![p("b"), bullets(vec![item(vec![p("c")])])]),
                    ],
                )],
            ),
            (
                "ordered-99",
                vec![self.ordered(99, vec![item(vec![p("a")]), item(vec![p("b")])])],
            ),
            (
                "ordered-tasks",
                vec![self.ordered(
                    1,
                    vec![
                        self.task(true, vec![p("done")]),
                        self.task(false, vec![p("todo")]),
                    ],
                )],
            ),
            (
                "item-only-nested-list",
                vec![bullets(vec![item(vec![bullets(vec![item(vec![p(
                    "inner",
                )])])])])],
            ),
            (
                "two-lists",
                vec![
                    bullets(vec![item(vec![p("a")])]),
                    bullets(vec![item(vec![p("b")])]),
                ],
            ),
            (
                "bullets-then-ordered",
                vec![
                    bullets(vec![item(vec![p("a")])]),
                    self.ordered(1, vec![item(vec![p("b")])]),
                ],
            ),
            (
                "item-two-paragraphs-then-item",
                vec![bullets(vec![
                    item(vec![p("a"), p("b")]),
                    item(vec![p("c")]),
                ])],
            ),
            (
                "tasks-4-deep",
                vec![bullets(vec![self.task(
                    false,
                    vec![
                        p("1"),
                        bullets(vec![self.task(
                            true,
                            vec![
                                p("2"),
                                bullets(vec![self.task(
                                    false,
                                    vec![p("3"), bullets(vec![self.task(true, vec![p("4")])])],
                                )]),
                            ],
                        )]),
                    ],
                )])],
            ),
            ("quote-two-paragraphs", vec![quote(vec![p("a"), p("b")])]),
            (
                "quote-paragraph-image",
                vec![quote(vec![p("a"), image(IMAGE_URL, Some("x"), None)])],
            ),
            (
                "quote-paragraph-board",
                vec![quote(vec![p("a"), board(None)])],
            ),
            (
                "quote-paragraph-attachment",
                vec![quote(vec![p("a"), self.attachment(Some("f.pdf"))])],
            ),
            ("quote-empty-paragraph", vec![quote(vec![empty()])]),
            (
                "quote-in-quote",
                vec![quote(vec![p("a"), quote(vec![p("b")]), p("c")])],
            ),
        ]
    }

    /// Shapes the old importer stored (spec A1, A7, A10), stray inline
    /// content in every block container, and nodes the serializer doesn't
    /// know.
    fn damaged(&self) -> Vec<(&'static str, Vec<N>)> {
        let in_item = |kids: Vec<N>| vec![bullets(vec![item(kids)])];
        let bold = |s: &str| marked(s, vec![("bold", self.shape.flag())]);
        vec![
            (
                "item-hard-break",
                in_item(vec![p("first"), hard_break(), p("second")]),
            ),
            (
                "item-soft-break",
                in_item(vec![p("line one"), text(" "), p("continued")]),
            ),
            (
                "item-soft-break-bold",
                in_item(vec![
                    el("paragraph", vec![bold("bold")]),
                    bold(" "),
                    el("paragraph", vec![bold("more")]),
                ]),
            ),
            (
                "nested-checklist-after-restore",
                vec![bullets(vec![
                    self.task(true, vec![p("parent"), text(" "), p("- [ ] child")]),
                ])],
            ),
            (
                "item-hard-break-chain",
                in_item(vec![p("a"), hard_break(), p("b"), hard_break(), p("c")]),
            ),
            ("item-leading-text", in_item(vec![text("a"), p("b")])),
            ("item-text-only", in_item(vec![text("a")])),
            (
                "empty-item",
                vec![bullets(vec![
                    item(vec![p("a")]),
                    item(Vec::new()),
                    item(vec![p("c")]),
                ])],
            ),
            (
                "item-heading-first",
                in_item(vec![self.heading(1, vec![text("h")])]),
            ),
            (
                "ordered-item-hard-break",
                vec![self.ordered(1, vec![item(vec![p("one"), hard_break(), p("two")])])],
            ),
            (
                "quoted-item-hard-break",
                vec![quote(vec![bullets(vec![item(vec![
                    p("a"),
                    hard_break(),
                    p("b"),
                ])])])],
            ),
            (
                "nested-item-hard-break",
                in_item(vec![
                    p("a"),
                    bullets(vec![item(vec![p("b"), hard_break(), p("c")])]),
                ]),
            ),
            ("top-level-text", vec![text("stray")]),
            ("top-level-hard-break", vec![p("a"), hard_break(), p("b")]),
            ("quote-text", vec![quote(vec![p("a"), text(" "), p("b")])]),
            ("cell-text", vec![self.table_around(vec![text("x")])]),
            (
                "unknown-block",
                vec![p("before"), el("callout", vec![p("hi")]), p("after")],
            ),
            (
                "unknown-inline",
                vec![el(
                    "paragraph",
                    vec![text("a"), el("mention_chip", Vec::new()), text("b")],
                )],
            ),
            (
                "unknown-in-item",
                in_item(vec![p("a"), el("details", vec![p("hidden")])]),
            ),
        ]
    }

    fn cases(&self) -> Vec<(String, Vec<N>)> {
        let mut out: Vec<(String, Vec<N>)> = Vec::new();
        for (name, inlines) in self.inlines() {
            out.push((
                format!("heading/{name}"),
                vec![self.heading(2, inlines.clone())],
            ));
            out.push((format!("paragraph/{name}"), vec![el("paragraph", inlines)]));
        }
        for (name, block) in self.leaves() {
            out.push((format!("block/{name}"), vec![block]));
        }
        let pairs = self.pair_blocks();
        for (a_name, a) in &pairs {
            for (b_name, b) in &pairs {
                out.push((
                    format!("pair/{a_name}+{b_name}"),
                    vec![a.clone(), b.clone()],
                ));
                for c in CONTAINERS {
                    out.push((
                        format!("in-{}/{a_name}+{b_name}", c.name()),
                        vec![self.wrap(c, 0, vec![a.clone(), b.clone()])],
                    ));
                }
            }
            for c in CONTAINERS {
                out.push((
                    format!("in-{}/{a_name}", c.name()),
                    vec![self.wrap(c, 0, vec![a.clone()])],
                ));
                out.push((
                    format!("in-{}/p+{a_name}+p", c.name()),
                    vec![self.wrap(c, 0, vec![p("before"), a.clone(), p("after")])],
                ));
            }
        }
        let inners: [(&str, Vec<N>); 2] = [
            (
                "marked",
                vec![el(
                    "paragraph",
                    vec![N::Text(vec![
                        run("deep ", Vec::new()),
                        run("bold", vec![("bold", self.shape.flag())]),
                        run(" ", Vec::new()),
                        run(
                            "link",
                            vec![("link", self.shape.link("https://example.com", None))],
                        ),
                    ])],
                )],
            ),
            ("code", vec![p("x"), self.code(Some("sh"), "a\nb")]),
        ];
        for len in 1..=4 {
            for chain in chains(len) {
                let label: Vec<&str> = chain.iter().map(|c| c.name()).collect();
                for (inner_name, inner) in &inners {
                    let mut blocks = inner.clone();
                    for (depth, c) in chain.iter().enumerate().rev() {
                        blocks = vec![self.nest(*c, depth, blocks)];
                    }
                    out.push((format!("nest/{}/{inner_name}", label.join(">")), blocks));
                }
            }
        }
        for (name, blocks) in self.lists() {
            out.push((format!("lists/{name}"), blocks));
        }
        for (name, blocks) in self.damaged() {
            out.push((format!("damaged/{name}"), blocks));
        }
        let mut sink: Vec<N> = pairs.into_iter().map(|(_, b)| b).collect();
        sink.extend(self.leaves().into_iter().map(|(_, b)| b));
        out.push(("kitchen-sink".into(), sink));
        out
    }
}

/// Every sequence of `len` containers, outermost first.
fn chains(len: usize) -> Vec<Vec<Container>> {
    let mut out = vec![Vec::new()];
    for _ in 0..len {
        out = out
            .into_iter()
            .flat_map(|chain| {
                CONTAINERS.iter().map(move |c| {
                    let mut next = chain.clone();
                    next.push(*c);
                    next
                })
            })
            .collect();
    }
    out
}

/// Deterministic combinations of every node and mark, containers nested up to
/// four deep, once in the editor's and once in the importer's representation.
pub fn generated() -> Vec<Case> {
    let mut out = Vec::new();
    for shape in Shape::ALL {
        for (name, blocks) in (Gen { shape }).cases() {
            out.push(Case {
                name: format!("{}/{name}", shape.name()),
                doc: build(&blocks),
            });
        }
    }
    out
}

/// xorshift64 (13, 7, 17), seeded through splitmix64 so that small,
/// adjacent seeds diverge at once.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self(if z == 0 { 1 } else { z })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

const MAX_DEPTH: usize = 4;

const WORDS: &[&str] = &[
    "alpha",
    "beta gamma",
    "x",
    "Grüße",
    "日本",
    "😀",
    "1. item",
    "* star",
    "[ ] box",
    "# hash",
    "a | b",
    "`tick`",
    "> quote",
    "under_score",
    "back\\slash",
    "<u>",
    " lead",
    "trail ",
    "-",
    "a  b",
];

const HREFS: &[&str] = &[
    "https://example.com",
    "https://example.com/a?b=c",
    DOC_URL,
    BLOB_URL,
    USER_URL,
    TIME_URL,
];

const CODE: &[&str] = &[
    "x",
    "fn main() {}",
    "a\nb",
    "  indented",
    "",
    "a\n\nb",
    "```",
];

/// Builds `random()`'s documents. Every attribute and mark picks its
/// representation on its own, so one document mixes both.
struct Random {
    rng: XorShift,
}

impl Random {
    fn shape(&mut self) -> Shape {
        if self.rng.one_in(2) {
            Shape::Editor
        } else {
            Shape::Import
        }
    }

    fn run(&mut self) -> Run {
        let word = self.rng.pick(WORDS);
        let mut marks = Vec::new();
        for m in FLAG_MARKS {
            if self.rng.one_in(6) {
                let value = self.shape().flag();
                marks.push((m, value));
            }
        }
        if self.rng.one_in(8) {
            let href = self.rng.pick(HREFS);
            let title = self.rng.one_in(4).then_some("T");
            let value = self.shape().link(href, title);
            marks.push(("link", value));
        }
        run(word, marks)
    }

    /// Inline content: runs grouped into one `XmlText` between hard breaks
    /// (as the editor stores them), sometimes split into two, and rarely an
    /// unknown inline node.
    fn inlines(&mut self) -> Vec<N> {
        let mut out = Vec::new();
        let mut runs = Vec::new();
        for _ in 0..self.rng.below(5) {
            if self.rng.one_in(6) {
                if !runs.is_empty() {
                    out.push(N::Text(std::mem::take(&mut runs)));
                }
                if self.rng.one_in(40) {
                    out.push(el("mention_chip", Vec::new()));
                } else {
                    out.push(hard_break());
                }
            } else {
                runs.push(self.run());
                if self.rng.one_in(30) {
                    out.push(N::Text(std::mem::take(&mut runs)));
                }
            }
        }
        if !runs.is_empty() {
            out.push(N::Text(runs));
        }
        out
    }

    /// Sometimes a stray text run or hard break among a block container's
    /// children: the shape the old importer stored.
    fn maybe_stray(&mut self, mut blocks: Vec<N>) -> Vec<N> {
        if self.rng.one_in(25) {
            let stray = if self.rng.one_in(2) {
                hard_break()
            } else {
                N::Text(vec![self.run()])
            };
            let at = self.rng.below(blocks.len() + 1);
            blocks.insert(at, stray);
        }
        blocks
    }

    fn blocks(&mut self, depth: usize) -> Vec<N> {
        let n = 1 + self.rng.below(3);
        let blocks = (0..n).map(|_| self.block(depth)).collect();
        self.maybe_stray(blocks)
    }

    fn block(&mut self, depth: usize) -> N {
        let kinds = if depth < MAX_DEPTH { 16 } else { 9 };
        match self.rng.below(kinds) {
            0..=2 => el("paragraph", self.inlines()),
            3 => {
                let level = self.shape().int(1 + self.rng.below(6) as i64);
                ela("heading", vec![("level", level)], self.inlines())
            }
            4 => {
                let attrs = if self.rng.one_in(2) {
                    vec![(
                        "language",
                        Any::from(self.rng.pick(&["rust", "sh", "text"])),
                    )]
                } else {
                    Vec::new()
                };
                let body = self.rng.pick(CODE);
                let children = if body.is_empty() {
                    Vec::new()
                } else {
                    vec![text(body)]
                };
                ela("code_block", attrs, children)
            }
            5 => rule(),
            6 => {
                let alt = self.rng.one_in(2).then_some("alt");
                let title = self.rng.one_in(3).then_some("Title");
                image(self.rng.pick(&[IMAGE_URL, BLOB_URL]), alt, title)
            }
            7 => {
                let shape = self.shape();
                let name = self.rng.one_in(4).then_some("notes.txt");
                (Gen { shape }).attachment(name)
            }
            8 => board(self.rng.pick(&[None, Some("Diagram"), Some("Flow")])),
            9 | 10 => quote(self.blocks(depth + 1)),
            11 | 12 => bullets(self.items(depth)),
            13 => {
                let start = self.shape().int(self.rng.pick(&[1, 2, 3, 9, 10, 99]));
                ela("ordered_list", vec![("start", start)], self.items(depth))
            }
            14 => self.table(depth),
            _ => {
                if self.rng.one_in(4) {
                    el("callout", vec![p("note")])
                } else {
                    el("paragraph", self.inlines())
                }
            }
        }
    }

    fn items(&mut self, depth: usize) -> Vec<N> {
        let n = 1 + self.rng.below(3);
        (0..n).map(|_| self.item(depth)).collect()
    }

    fn item(&mut self, depth: usize) -> N {
        let mut attrs = Vec::new();
        if self.rng.one_in(3) {
            let checked = self.rng.one_in(2);
            attrs.push(("checked", self.shape().boolean(checked)));
        }
        let mut children = Vec::new();
        // Rarely an item without its leading paragraph, as the old importer
        // stored empty items and items starting with another block.
        if !self.rng.one_in(25) {
            children.push(el("paragraph", self.inlines()));
        }
        for _ in 0..self.rng.below(3) {
            children.push(self.block(depth + 1));
        }
        let children = self.maybe_stray(children);
        ela("list_item", attrs, children)
    }

    fn table(&mut self, depth: usize) -> N {
        let cols = 1 + self.rng.below(3);
        let rows = 1 + self.rng.below(3);
        let header = self.rng.one_in(2);
        let aligns: Vec<Option<&str>> = (0..cols)
            .map(|_| {
                self.rng
                    .pick(&[None, None, Some("left"), Some("center"), Some("right")])
            })
            .collect();
        let rows = (0..rows)
            .map(|r| {
                let tag = if r == 0 && header {
                    "table_header"
                } else {
                    "table_cell"
                };
                let cells = aligns.iter().map(|a| self.cell(tag, *a, depth)).collect();
                row(cells)
            })
            .collect();
        el("table", rows)
    }

    fn cell(&mut self, tag: &'static str, align: Option<&str>, depth: usize) -> N {
        let mut attrs = Vec::new();
        if self.rng.one_in(2) {
            attrs.push(("colspan", self.shape().int(1)));
            attrs.push(("rowspan", self.shape().int(1)));
        }
        if self.rng.one_in(4) {
            let px = 80 + 20 * self.rng.below(5) as i64;
            attrs.push(("colwidth", self.shape().colwidth(px)));
        }
        if let Some(a) = align {
            attrs.push(("align", Any::from(a)));
        }
        let mut children = vec![el("paragraph", self.inlines())];
        if self.rng.one_in(4) {
            children.push(self.block(depth + 1));
        }
        let children = self.maybe_stray(children);
        ela(tag, attrs, children)
    }
}

/// `n` random documents from `seed`, containers nested up to four deep.
pub fn random(seed: u64, n: usize) -> Vec<Case> {
    let mut g = Random {
        rng: XorShift::new(seed),
    };
    (0..n)
        .map(|i| {
            let blocks = g.blocks(0);
            Case {
                name: format!("random/{seed}/{i}"),
                doc: build(&blocks),
            }
        })
        .collect()
}

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// The `.md` files in `dir`, sorted by name.
pub fn md_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    files.sort();
    files
}

/// Every fixture and probe file as `(name, Markdown)`, the name being the
/// path under `tests/`.
pub fn markdown_inputs() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for dir in ["fixtures", "probes"] {
        let files = md_files(&tests_dir().join(dir));
        assert!(!files.is_empty(), "no Markdown files in tests/{dir}");
        for path in files {
            let name = format!(
                "{dir}/{}",
                path.file_name().expect("file name").to_string_lossy()
            );
            let md = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            out.push((name, md));
        }
    }
    out
}
