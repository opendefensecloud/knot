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
