//! Format-neutral export model.

#[derive(Debug, Clone, PartialEq)]
pub struct ExportDoc {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    /// `level` is clamped to 1..=6.
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    Quote(Vec<Block>),
    /// A callout box: a blockquote with a known `kind`. `title` is its own
    /// title, already one trimmed line; none shows the kind's label.
    Callout {
        kind: CalloutKind,
        title: Option<String>,
        blocks: Vec<Block>,
    },
    Code {
        language: String,
        text: String,
    },
    Mermaid {
        key: String,
        source: String,
    },
    Rule,
    List(List),
    Table(Table),
    Image {
        source: ImageSource,
        alt: String,
    },
    Board {
        key: String,
        board_id: String,
        label: String,
    },
    Attachment {
        url: String,
        name: String,
        size: Option<u64>,
    },
    /// A table of contents: the page's headings down to `max_level`, filled
    /// in by the walk once every heading is known.
    Toc {
        max_level: u8,
        entries: Vec<TocEntry>,
    },
}

/// One line of a [`Block::Toc`]: `level` 0–4 below the shallowest heading
/// shown (deeper ones stay at 4), and the heading's text.
#[derive(Debug, Clone, PartialEq)]
pub struct TocEntry {
    pub level: u8,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Run(Run),
    Link { target: LinkTarget, runs: Vec<Run> },
    Break,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    pub marks: Marks,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Marks {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    Web(String),
    Doc(String),
    User(String),
    Plain,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImageSource {
    Blob(String),
    External(String),
    Unsupported,
}

#[derive(Debug, Clone, PartialEq)]
pub struct List {
    pub ordered: bool,
    pub start: u32,
    pub items: Vec<ListItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub shares: Vec<f64>,
    pub rows: Vec<Vec<Cell>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub col: usize,
    pub colspan: usize,
    pub rowspan: usize,
    pub header: bool,
    pub align: Align,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// The five kinds of callout box (GitHub's and GitLab's alerts). The same
/// list lives in `knot_markdown::callout`, which this crate does not depend
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl CalloutKind {
    /// The kind named `raw`, in any case; `None` for anything else.
    pub fn parse(raw: &str) -> Option<CalloutKind> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "note" => Some(CalloutKind::Note),
            "tip" => Some(CalloutKind::Tip),
            "important" => Some(CalloutKind::Important),
            "warning" => Some(CalloutKind::Warning),
            "caution" => Some(CalloutKind::Caution),
            _ => None,
        }
    }

    /// The title of a box without one of its own.
    pub fn label(self) -> &'static str {
        match self {
            CalloutKind::Note => "Note",
            CalloutKind::Tip => "Tip",
            CalloutKind::Important => "Important",
            CalloutKind::Warning => "Warning",
            CalloutKind::Caution => "Caution",
        }
    }

    /// The kind's colour (GitHub's light hue), as hex without `#`: border
    /// and title text.
    pub fn hue(self) -> &'static str {
        match self {
            CalloutKind::Note => "0969DA",
            CalloutKind::Tip => "1A7F37",
            CalloutKind::Important => "8250DF",
            CalloutKind::Warning => "9A6700",
            CalloutKind::Caution => "CF222E",
        }
    }

    /// The hue at 15% on white: the title line's shading.
    pub fn shade(self) -> &'static str {
        match self {
            CalloutKind::Note => "DAE8F9",
            CalloutKind::Tip => "DDECE1",
            CalloutKind::Important => "ECE5FA",
            CalloutKind::Warning => "F0E8D9",
            CalloutKind::Caution => "F8DEE0",
        }
    }

    /// A name for styles: `Note` … `Caution`.
    pub fn style_suffix(self) -> &'static str {
        self.label()
    }
}
