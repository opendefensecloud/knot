//! `word/numbering.xml`: the list definitions and the `w:num` instances.
//!
//! Two abstract definitions, both nine levels deep: 0 for bullets and 1 for
//! decimals. Every list in the document gets its own `w:num` pointing at one
//! of them, so numbered lists restart where they should.

/// `w:abstractNumId` of the bullet definition.
pub(super) const BULLET_ABSTRACT: u32 = 0;
/// `w:abstractNumId` of the decimal definition.
pub(super) const DECIMAL_ABSTRACT: u32 = 1;
/// Levels per definition; the deepest `w:ilvl` is one less.
pub(super) const LEVELS: u32 = 9;
/// Indent step per level, in twips.
pub(super) const INDENT_TWIPS: u32 = 720;
/// How far the marker hangs left of the text, in twips.
const HANGING_TWIPS: u32 = 360;

const BULLETS: [&str; 3] = ["\u{2022}", "\u{25E6}", "\u{25AA}"];

const HEAD: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
);

/// The built-in numbering part: both `w:abstractNum`s and no `w:num`.
pub(super) fn builtin_xml() -> String {
    let mut s = String::from(HEAD);
    abstract_num(&mut s, BULLET_ABSTRACT, |level| {
        ("bullet", BULLETS[level as usize % BULLETS.len()].to_owned())
    });
    abstract_num(&mut s, DECIMAL_ABSTRACT, |level| {
        ("decimal", format!("%{}.", level + 1))
    });
    s.push_str("</w:numbering>");
    s
}

fn abstract_num(out: &mut String, id: u32, level: impl Fn(u32) -> (&'static str, String)) {
    out.push_str(&format!(
        r#"<w:abstractNum w:abstractNumId="{id}"><w:multiLevelType w:val="hybridMultilevel"/>"#
    ));
    for ilvl in 0..LEVELS {
        let (format, text) = level(ilvl);
        out.push_str(&format!(
            concat!(
                r#"<w:lvl w:ilvl="{ilvl}"><w:start w:val="1"/><w:numFmt w:val="{format}"/>"#,
                r#"<w:lvlText w:val="{text}"/><w:lvlJc w:val="left"/>"#,
                r#"<w:pPr><w:ind w:left="{left}" w:hanging="{hanging}"/></w:pPr></w:lvl>"#,
            ),
            ilvl = ilvl,
            format = format,
            text = text,
            left = INDENT_TWIPS * (ilvl + 1),
            hanging = HANGING_TWIPS,
        ));
    }
    out.push_str("</w:abstractNum>");
}

/// One `w:num` for a list instance. An ordered list restarts at `start` on
/// its own level, even when that is 1, so it never continues an earlier list.
pub(super) fn num(num_id: u32, ordered: bool, ilvl: u32, start: u32) -> String {
    if ordered {
        format!(
            concat!(
                r#"<w:num w:numId="{num_id}"><w:abstractNumId w:val="{DECIMAL_ABSTRACT}"/>"#,
                r#"<w:lvlOverride w:ilvl="{ilvl}"><w:startOverride w:val="{start}"/>"#,
                r#"</w:lvlOverride></w:num>"#,
            ),
            num_id = num_id,
            DECIMAL_ABSTRACT = DECIMAL_ABSTRACT,
            ilvl = ilvl,
            start = start,
        )
    } else {
        format!(r#"<w:num w:numId="{num_id}"><w:abstractNumId w:val="{BULLET_ABSTRACT}"/></w:num>"#)
    }
}

/// `numbering` with `nums` inserted before its closing tag, after every
/// `w:abstractNum`. `None` when the part has no closing tag to insert at.
pub(super) fn with_nums(numbering: &str, nums: &str) -> Option<String> {
    if nums.is_empty() {
        return Some(numbering.to_owned());
    }
    let at = numbering.rfind("</w:numbering>")?;
    let mut s = String::with_capacity(numbering.len() + nums.len());
    s.push_str(&numbering[..at]);
    s.push_str(nums);
    s.push_str(&numbering[at..]);
    Some(s)
}
