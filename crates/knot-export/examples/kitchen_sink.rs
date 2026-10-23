//! Writes `kitchen-sink.docx` and `kitchen-sink.odt` into a directory: one
//! page with every block and inline kind and every fallback, for opening in
//! Word, LibreOffice and friends by hand.
//!
//! `cargo run -p knot-export --example kitchen_sink -- <out_dir>`

use std::path::PathBuf;

use knot_export::assets::{Assets, ImageAsset, Meta, PicFormat, Picture};
use knot_export::{
    Align, Block, Cell, ExportDoc, ImageSource, Inline, LinkTarget, List, ListItem, Marks, Run,
    Table, write_docx, write_odt,
};

const PHOTO: &str = "11111111-1111-4111-8111-111111111111";
const WIDE: &str = "22222222-2222-4222-8222-222222222222";
const GONE: &str = "33333333-3333-4333-8333-333333333333";
const HUGE: &str = "44444444-4444-4444-8444-444444444444";
const DOC: &str = "55555555-5555-4555-8555-555555555555";
const USER: &str = "66666666-6666-4666-8666-666666666666";
const GHOST: &str = "77777777-7777-4777-8777-777777777777";
const BOARD: &str = "88888888-8888-4888-8888-888888888888";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir: PathBuf = std::env::args_os()
        .nth(1)
        .ok_or("usage: kitchen_sink <out_dir>")?
        .into();
    std::fs::create_dir_all(&dir)?;

    let (doc, assets) = sample();
    let meta = Meta {
        title: "Kitchen sink".into(),
        exported_on: "23 Oct 2026".into(),
        base_url: "https://knot.example".into(),
    };
    for (name, out) in [
        ("kitchen-sink.docx", write_docx(&doc, &meta, &assets)?),
        ("kitchen-sink.odt", write_odt(&doc, &meta, &assets)?),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, &out.bytes)?;
        println!(
            "{} ({} bytes, {:?})",
            path.display(),
            out.bytes.len(),
            out.fallbacks
        );
    }
    Ok(())
}

fn run(text: &str, marks: Marks) -> Run {
    Run {
        text: text.into(),
        marks,
    }
}

fn plain(text: &str) -> Inline {
    Inline::Run(run(text, Marks::default()))
}

fn marked(text: &str, set: impl FnOnce(&mut Marks)) -> Inline {
    let mut marks = Marks::default();
    set(&mut marks);
    Inline::Run(run(text, marks))
}

fn para(content: Vec<Inline>) -> Block {
    Block::Paragraph(content)
}

fn text_para(text: &str) -> Block {
    para(vec![plain(text)])
}

fn item(checked: Option<bool>, blocks: Vec<Block>) -> ListItem {
    ListItem { checked, blocks }
}

fn cell(col: usize, header: bool, align: Align, blocks: Vec<Block>) -> Cell {
    Cell {
        col,
        colspan: 1,
        rowspan: 1,
        header,
        align,
        blocks,
    }
}

/// A solid-colour PNG with a lighter diagonal, so scaling shows.
fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let on_diagonal = x * height / width.max(1) == y;
            let px = if on_diagonal { [255, 255, 255] } else { rgb };
            data.extend_from_slice(&px);
        }
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(&data).expect("png data");
    writer.finish().expect("png end");
    out
}

fn picture(width_px: u32, height_px: u32, scale: u32, rgb: [u8; 3]) -> Picture {
    Picture {
        format: PicFormat::Png,
        bytes: png(width_px, height_px, rgb),
        width_px,
        height_px,
        scale,
    }
}

fn sample() -> (ExportDoc, Assets) {
    let mut assets = Assets::default();
    assets.images.insert(
        PHOTO.into(),
        ImageAsset::Embedded(picture(320, 180, 1, [40, 110, 180])),
    );
    // Wider than the page, so it is shrunk to fit.
    assets.images.insert(
        WIDE.into(),
        ImageAsset::Embedded(picture(1600, 400, 1, [180, 90, 40])),
    );
    assets.images.insert(GONE.into(), ImageAsset::Unavailable);
    assets.images.insert(HUGE.into(), ImageAsset::OverBudget);
    // Diagrams are rendered at twice their display size.
    assets
        .diagrams
        .insert("mermaid-ok".into(), picture(640, 320, 2, [60, 140, 90]));
    assets.diagrams.insert(
        format!("board-{BOARD}"),
        picture(800, 400, 2, [120, 70, 150]),
    );
    assets.user_names.insert(USER.into(), "Ada Lovelace".into());

    let blocks = vec![
        Block::Heading {
            level: 1,
            content: vec![plain("Headings and text")],
        },
        Block::Heading {
            level: 2,
            content: vec![plain("Level two")],
        },
        Block::Heading {
            level: 3,
            content: vec![plain("Level three")],
        },
        Block::Heading {
            level: 6,
            content: vec![plain("Level six")],
        },
        para(vec![
            plain("Plain, "),
            marked("bold", |m| m.bold = true),
            plain(", "),
            marked("italic", |m| m.italic = true),
            plain(", "),
            marked("underline", |m| m.underline = true),
            plain(", "),
            marked("strike", |m| m.strike = true),
            plain(", "),
            marked("code", |m| m.code = true),
            plain(" and "),
            marked("all of them", |m| {
                m.bold = true;
                m.italic = true;
                m.underline = true;
                m.strike = true;
            }),
            plain("."),
            Inline::Break,
            plain(
                "After a line break:\ttab, and  two  spaces. <&> \"quotes\" \u{1F600} \u{05E9}\u{05DC}\u{05D5}\u{05DD}",
            ),
        ]),
        para(vec![
            plain("Links: "),
            Inline::Link {
                target: LinkTarget::Web("https://example.com/?a=1&b=2".into()),
                runs: vec![run("a web page", Marks::default())],
            },
            plain(", "),
            Inline::Link {
                target: LinkTarget::Doc(DOC.into()),
                runs: vec![run("another page", Marks::default())],
            },
            plain(", a mention of "),
            Inline::Link {
                target: LinkTarget::User(USER.into()),
                runs: vec![run("@ada", Marks::default())],
            },
            plain(", a mention of a deleted user "),
            Inline::Link {
                target: LinkTarget::User(GHOST.into()),
                runs: vec![run("@ghost", Marks::default())],
            },
            plain(" and "),
            Inline::Link {
                target: LinkTarget::Plain,
                runs: vec![run("a link that stays text", Marks::default())],
            },
            plain("."),
        ]),
        Block::Rule,
        Block::Quote(vec![
            text_para("A quotation."),
            Block::Quote(vec![text_para("A quotation inside it.")]),
            Block::List(List {
                ordered: false,
                start: 1,
                items: vec![item(None, vec![text_para("A list in a quote")])],
            }),
        ]),
        Block::Code {
            language: "rust".into(),
            text: "fn main() {\n    println!(\"hi\");\n\t// tab\n}".into(),
        },
        Block::Heading {
            level: 1,
            content: vec![plain("Lists")],
        },
        Block::List(List {
            ordered: false,
            start: 1,
            items: vec![
                item(
                    None,
                    vec![
                        text_para("Bullet one"),
                        Block::List(List {
                            ordered: true,
                            start: 3,
                            items: vec![
                                item(None, vec![text_para("Nested, starts at 3")]),
                                item(None, vec![text_para("Nested, four")]),
                            ],
                        }),
                    ],
                ),
                item(
                    None,
                    vec![text_para("Bullet two"), text_para("Second paragraph")],
                ),
            ],
        }),
        Block::List(List {
            ordered: true,
            start: 1,
            items: vec![
                item(None, vec![text_para("First")]),
                item(None, vec![text_para("Second")]),
            ],
        }),
        Block::List(List {
            ordered: false,
            start: 1,
            items: vec![
                item(Some(true), vec![text_para("Done")]),
                item(Some(false), vec![text_para("Still to do")]),
            ],
        }),
        Block::Heading {
            level: 1,
            content: vec![plain("Tables")],
        },
        Block::Table(Table {
            shares: vec![0.3, 0.4, 0.3],
            rows: vec![
                vec![
                    cell(0, true, Align::Left, vec![text_para("Left")]),
                    cell(1, true, Align::Center, vec![text_para("Centre")]),
                    cell(2, true, Align::Right, vec![text_para("Right")]),
                ],
                vec![
                    Cell {
                        colspan: 2,
                        ..cell(0, false, Align::Left, vec![text_para("Spans two columns")])
                    },
                    Cell {
                        rowspan: 2,
                        ..cell(2, false, Align::Right, vec![text_para("Spans two rows")])
                    },
                ],
                vec![
                    cell(0, false, Align::Left, vec![text_para("a")]),
                    cell(
                        1,
                        false,
                        Align::Center,
                        vec![
                            text_para("A nested table:"),
                            Block::Table(Table {
                                shares: vec![0.5, 0.5],
                                rows: vec![vec![
                                    cell(0, false, Align::Left, vec![text_para("x")]),
                                    cell(1, false, Align::Left, vec![text_para("y")]),
                                ]],
                            }),
                        ],
                    ),
                ],
                vec![cell(
                    0,
                    false,
                    Align::Left,
                    vec![Block::Image {
                        source: ImageSource::Blob(PHOTO.into()),
                        alt: "A picture inside a cell".into(),
                    }],
                )],
            ],
        }),
        Block::Heading {
            level: 1,
            content: vec![plain("Pictures")],
        },
        Block::Image {
            source: ImageSource::Blob(PHOTO.into()),
            alt: "A blue picture".into(),
        },
        Block::Image {
            source: ImageSource::Blob(WIDE.into()),
            alt: "A wide orange picture, shrunk to the page".into(),
        },
        // The same blob again shares one stored picture.
        Block::Image {
            source: ImageSource::Blob(PHOTO.into()),
            alt: "The blue picture again".into(),
        },
        Block::Image {
            source: ImageSource::External("https://img.example/remote.png".into()),
            alt: "An external image".into(),
        },
        Block::Mermaid {
            key: "mermaid-ok".into(),
            source: "graph TD\n  A --> B".into(),
        },
        Block::Board {
            key: format!("board-{BOARD}"),
            board_id: BOARD.into(),
            label: "Roadmap".into(),
        },
        Block::Attachment {
            url: "/api/blobs/99999999-9999-4999-8999-999999999999".into(),
            name: "budget.xlsx".into(),
            size: Some(49152),
        },
        Block::Attachment {
            url: "https://elsewhere.example/file.pdf".into(),
            name: "elsewhere.pdf".into(),
            size: None,
        },
        Block::Heading {
            level: 1,
            content: vec![plain("Fallbacks")],
        },
        Block::Mermaid {
            key: "mermaid-missing".into(),
            source: "graph LR\n  X --> Y".into(),
        },
        Block::Board {
            key: "board-missing".into(),
            board_id: "00000000-0000-4000-8000-000000000000".into(),
            label: "Sprint board".into(),
        },
        Block::Image {
            source: ImageSource::Blob(GONE.into()),
            alt: "Unavailable image".into(),
        },
        Block::Image {
            source: ImageSource::Blob(HUGE.into()),
            alt: "Image over the size budget".into(),
        },
        Block::Image {
            source: ImageSource::Unsupported,
            alt: "Local file".into(),
        },
    ];
    (ExportDoc { blocks }, assets)
}
