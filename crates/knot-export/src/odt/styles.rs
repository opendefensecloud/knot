//! The built-in `styles.xml`: fonts, the named styles, the outline and list
//! styles, the page layout and the master page with its footer. A company
//! template would replace this part (spec §10), so it reaches the writer only
//! through [`super::OdtDefaults`].
//!
//! Style names are LibreOffice's (`Heading_20_1` is "Heading 1"), so
//! LibreOffice, and a later template mapping, treats them as its own.

use super::{LIST_INDENT_CM, LIST_LEVELS};

const HEAD: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?>"#,
    r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
    r#"xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" "#,
    r#"xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" "#,
    r#"xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" "#,
    r#"xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" "#,
    r#"xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" "#,
    r#"office:version="1.3">"#,
    r#"<office:font-face-decls>"#,
    r#"<style:font-face style:name="Calibri" svg:font-family="Calibri" style:font-family-generic="swiss" style:font-pitch="variable"/>"#,
    r#"<style:font-face style:name="Consolas" svg:font-family="Consolas" style:font-family-generic="modern" style:font-pitch="fixed"/>"#,
    r#"</office:font-face-decls>"#,
);

/// The defaults and the title styles. Sizes and spacing follow the DOCX
/// styles (spec §7).
const TITLE_STYLES: &str = concat!(
    r#"<style:default-style style:family="paragraph">"#,
    r#"<style:paragraph-properties fo:margin-top="0cm" fo:margin-bottom="0.282cm" fo:line-height="115%" style:tab-stop-distance="1.27cm"/>"#,
    r#"<style:text-properties style:font-name="Calibri" fo:font-size="11pt" style:font-name-asian="Calibri" "#,
    r#"style:font-size-asian="11pt" style:font-name-complex="Calibri" style:font-size-complex="11pt"/>"#,
    r#"</style:default-style>"#,
    r#"<style:style style:name="Standard" style:family="paragraph" style:class="text"/>"#,
    r#"<style:style style:name="Title" style:display-name="Title" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:next-style-name="Standard" style:class="chapter">"#,
    r#"<style:paragraph-properties fo:margin-bottom="0.141cm" fo:line-height="100%"/>"#,
    r#"<style:text-properties fo:font-size="28pt" style:font-size-asian="28pt" style:font-size-complex="28pt"/>"#,
    r#"</style:style>"#,
    r#"<style:style style:name="Subtitle" style:display-name="Subtitle" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:next-style-name="Standard" style:class="chapter">"#,
    r#"<style:paragraph-properties fo:margin-bottom="0.635cm"/>"#,
    r##"<style:text-properties fo:color="#595959" fo:font-size="12pt" style:font-size-asian="12pt" style:font-size-complex="12pt"/>"##,
    r#"</style:style>"#,
);

/// The other paragraph styles, then the character styles.
const BODY_STYLES: &str = concat!(
    // The left margin is QUOTE_INDENT_CM in mod.rs.
    r#"<style:style style:name="Quotations" style:display-name="Quotations" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:class="html">"#,
    r#"<style:paragraph-properties fo:margin-left="1cm" fo:margin-right="0cm" fo:text-indent="0cm" "#,
    r#"fo:border-left="2.25pt solid #bfbfbf" fo:padding-left="0.282cm"/>"#,
    r##"<style:text-properties fo:color="#404040" fo:font-style="italic" style:font-style-asian="italic" style:font-style-complex="italic"/>"##,
    r#"</style:style>"#,
    r#"<style:style style:name="Preformatted_20_Text" style:display-name="Preformatted Text" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:class="html">"#,
    r##"<style:paragraph-properties fo:line-height="100%" fo:background-color="#f5f5f5"/>"##,
    r#"<style:text-properties style:font-name="Consolas" fo:font-size="10pt" style:font-name-asian="Consolas" "#,
    r#"style:font-size-asian="10pt" style:font-name-complex="Consolas" style:font-size-complex="10pt"/>"#,
    r#"</style:style>"#,
    r#"<style:style style:name="List_20_Paragraph" style:display-name="List Paragraph" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:class="list">"#,
    r#"<style:paragraph-properties style:contextual-spacing="true"/>"#,
    r#"</style:style>"#,
    r#"<style:style style:name="Table_20_Contents" style:display-name="Table Contents" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:class="extra">"#,
    r#"<style:paragraph-properties fo:margin-top="0.071cm" fo:margin-bottom="0.071cm" fo:line-height="100%"/>"#,
    r#"</style:style>"#,
    r#"<style:style style:name="Table_20_Heading" style:display-name="Table Heading" style:family="paragraph" "#,
    r#"style:parent-style-name="Table_20_Contents" style:class="extra">"#,
    r#"<style:paragraph-properties fo:text-align="start"/>"#,
    r#"<style:text-properties fo:font-weight="bold" style:font-weight-asian="bold" style:font-weight-complex="bold"/>"#,
    r#"</style:style>"#,
    r#"<style:style style:name="Caption" style:display-name="Caption" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:next-style-name="Standard" style:class="extra">"#,
    r##"<style:text-properties fo:color="#595959" fo:font-size="10pt" fo:font-style="italic" "##,
    r#"style:font-size-asian="10pt" style:font-style-asian="italic" style:font-size-complex="10pt" style:font-style-complex="italic"/>"#,
    r#"</style:style>"#,
    r#"<style:style style:name="Footer" style:display-name="Footer" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:class="extra">"#,
    r#"<style:paragraph-properties fo:margin-top="0cm" fo:margin-bottom="0cm" fo:line-height="100%" fo:text-align="center"/>"#,
    r##"<style:text-properties fo:color="#595959" fo:font-size="9pt" style:font-size-asian="9pt" style:font-size-complex="9pt"/>"##,
    r#"</style:style>"#,
    r#"<style:style style:name="Horizontal_20_Line" style:display-name="Horizontal Line" style:family="paragraph" "#,
    r#"style:parent-style-name="Standard" style:next-style-name="Standard" style:class="html">"#,
    r#"<style:paragraph-properties fo:margin-top="0.212cm" fo:margin-bottom="0.423cm" "#,
    r#"fo:border-bottom="0.75pt solid #bfbfbf" fo:padding-bottom="0.035cm"/>"#,
    r#"<style:text-properties fo:font-size="4pt" style:font-size-asian="4pt" style:font-size-complex="4pt"/>"#,
    r#"</style:style>"#,
    // Character styles.
    r#"<style:style style:name="Source_20_Text" style:display-name="Source Text" style:family="text">"#,
    r#"<style:text-properties style:font-name="Consolas" style:font-name-asian="Consolas" "#,
    r##"style:font-name-complex="Consolas" fo:background-color="#f2f2f2"/>"##,
    r#"</style:style>"#,
    r#"<style:style style:name="Internet_20_link" style:display-name="Internet link" style:family="text">"#,
    r##"<style:text-properties fo:color="#0563c1" style:text-underline-style="solid" "##,
    r#"style:text-underline-width="auto" style:text-underline-color="font-color"/>"#,
    r#"</style:style>"#,
);

/// Heading 1-6 as (font size, space before, space after).
const HEADING_LOOK: [(&str, &str, &str); 6] = [
    ("16pt", "0.635cm", "0.212cm"),
    ("14pt", "0.494cm", "0.176cm"),
    ("13pt", "0.423cm", "0.141cm"),
    ("12pt", "0.353cm", "0.141cm"),
    ("11pt", "0.353cm", "0.141cm"),
    ("11pt", "0.353cm", "0.141cm"),
];

/// A4 portrait with 2.5 cm margins. The footer sits in the bottom margin:
/// 1.25 cm from the page edge, 0.6 cm high, 0.65 cm below the text, so the
/// text ends 2.5 cm from the edge, as in the DOCX.
const PAGE: &str = concat!(
    r#"<office:automatic-styles><style:page-layout style:name="pm1">"#,
    r#"<style:page-layout-properties fo:page-width="21cm" fo:page-height="29.7cm" style:print-orientation="portrait" "#,
    r#"fo:margin-top="2.5cm" fo:margin-bottom="1.25cm" fo:margin-left="2.5cm" fo:margin-right="2.5cm"/>"#,
    r#"<style:header-style/>"#,
    r#"<style:footer-style><style:header-footer-properties fo:min-height="0.6cm" fo:margin-top="0.65cm"/></style:footer-style>"#,
    r#"</style:page-layout></office:automatic-styles>"#,
    r#"<office:master-styles><style:master-page style:name="Standard" style:page-layout-name="pm1">"#,
    r#"<style:footer><text:p text:style-name="Footer">Page "#,
    r#"<text:page-number text:select-page="current">1</text:page-number> of "#,
    r#"<text:page-count>1</text:page-count></text:p></style:footer>"#,
    r#"</style:master-page></office:master-styles>"#,
    r#"</office:document-styles>"#,
);

/// The built-in `styles.xml`.
pub(super) fn builtin_xml() -> String {
    const BULLETS: [&str; 3] = ["\u{2022}", "\u{25E6}", "\u{25AA}"];
    const NUMBER_END: &str = "</text:list-level-style-number>";
    let mut s = String::from(HEAD);
    s.push_str("<office:styles>");
    s.push_str(TITLE_STYLES);
    headings(&mut s);
    s.push_str(BODY_STYLES);
    outline_style(&mut s);
    list_style(&mut s, ("List_20_Bullet", "List Bullet"), true, |level| {
        let bullet = BULLETS[(level - 1) as usize % BULLETS.len()];
        (
            format!(
                r#"<text:list-level-style-bullet text:level="{level}" text:bullet-char="{bullet}">"#
            ),
            "</text:list-level-style-bullet>",
        )
    });
    list_style(
        &mut s,
        ("Numbering_20_123", "Numbering 123"),
        true,
        |level| {
            let open = format!(
                r#"<text:list-level-style-number text:level="{level}" style:num-suffix="." style:num-format="1">"#
            );
            (open, NUMBER_END)
        },
    );
    // No label: a checklist item's box is part of its text.
    list_style(&mut s, ("List_20_Check", "List Check"), false, |level| {
        let open =
            format!(r#"<text:list-level-style-number text:level="{level}" style:num-format="">"#);
        (open, NUMBER_END)
    });
    s.push_str("</office:styles>");
    s.push_str(PAGE);
    s
}

fn headings(out: &mut String) {
    for (i, (size, before, after)) in HEADING_LOOK.iter().enumerate() {
        let level = i + 1;
        let italic = if level == 6 {
            r#" fo:font-style="italic" style:font-style-asian="italic" style:font-style-complex="italic""#
        } else {
            ""
        };
        out.push_str(&format!(
            concat!(
                r#"<style:style style:name="Heading_20_{level}" style:display-name="Heading {level}" "#,
                r#"style:family="paragraph" style:parent-style-name="Standard" style:next-style-name="Standard" "#,
                r#"style:default-outline-level="{level}" style:class="text">"#,
                r#"<style:paragraph-properties fo:margin-top="{before}" fo:margin-bottom="{after}" "#,
                r#"fo:keep-with-next="always" fo:keep-together="always"/>"#,
                r#"<style:text-properties fo:font-size="{size}" fo:font-weight="bold" style:font-size-asian="{size}" "#,
                r#"style:font-weight-asian="bold" style:font-size-complex="{size}" style:font-weight-complex="bold"{italic}/>"#,
                r#"</style:style>"#,
            ),
            level = level,
            before = before,
            after = after,
            size = size,
            italic = italic,
        ));
    }
}

/// Headings carry outline levels but no numbers.
fn outline_style(out: &mut String) {
    out.push_str(r#"<text:outline-style style:name="Outline">"#);
    for level in 1..=LIST_LEVELS {
        out.push_str(&format!(
            concat!(
                r#"<text:outline-level-style text:level="{level}" style:num-format="">"#,
                r#"<style:list-level-properties text:list-level-position-and-space-mode="label-alignment">"#,
                r#"<style:list-level-label-alignment text:label-followed-by="nothing" fo:text-indent="0cm" fo:margin-left="0cm"/>"#,
                r#"</style:list-level-properties></text:outline-level-style>"#,
            ),
            level = level,
        ));
    }
    out.push_str("</text:outline-style>");
}

/// A list style `(name, display name)` with [`LIST_LEVELS`] levels, each
/// indented one [`LIST_INDENT_CM`] step further. `level` gives a level's
/// opening and closing tag. A label hangs half a step left of the text and a
/// tab follows it; without one (checklists) the text starts at the indent.
fn list_style(
    out: &mut String,
    (name, display): (&str, &str),
    labelled: bool,
    level: impl Fn(u32) -> (String, &'static str),
) {
    out.push_str(&format!(
        r#"<text:list-style style:name="{name}" style:display-name="{display}">"#
    ));
    let (followed_by, text_indent) = if labelled {
        ("listtab", format!("-{:.3}cm", LIST_INDENT_CM / 2.0))
    } else {
        ("nothing", "0cm".to_owned())
    };
    for n in 1..=LIST_LEVELS {
        let (open, close) = level(n);
        out.push_str(&open);
        out.push_str(&format!(
            concat!(
                r#"<style:list-level-properties text:list-level-position-and-space-mode="label-alignment">"#,
                r#"<style:list-level-label-alignment text:label-followed-by="{followed_by}" "#,
                r#"text:list-tab-stop-position="{indent:.3}cm" fo:text-indent="{text_indent}" fo:margin-left="{indent:.3}cm"/>"#,
                r#"</style:list-level-properties>"#,
            ),
            followed_by = followed_by,
            indent = LIST_INDENT_CM * f64::from(n),
            text_indent = text_indent,
        ));
        out.push_str(close);
    }
    out.push_str("</text:list-style>");
}
