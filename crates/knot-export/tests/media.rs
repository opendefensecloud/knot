use knot_export::assets::{PicFormat, Picture};
use knot_export::media::{fit_cm, fit_emu, picture_from_png, prepare_image};
use knot_export::xml::{ZipOut, esc_attr, esc_text};

fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer
        .write_image_data(&vec![200u8; (w * h * 3) as usize])
        .unwrap();
    writer.finish().unwrap();
    out
}

fn pic(w: u32, h: u32, scale: u32) -> Picture {
    Picture {
        format: PicFormat::Png,
        bytes: Vec::new(),
        width_px: w,
        height_px: h,
        scale,
    }
}

#[test]
fn png_jpeg_gif_dimensions_are_read() {
    let p = prepare_image(&png_bytes(3, 2)).unwrap();
    assert_eq!(
        (p.format, p.width_px, p.height_px, p.scale),
        (PicFormat::Png, 3, 2, 1)
    );

    // SOI, SOF0 (height 2, width 3, 3 components), EOI.
    let jpeg = [
        0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x02, 0x00, 0x03, 0x03, 0x01, 0x22, 0x00,
        0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xFF, 0xD9,
    ];
    let p = prepare_image(&jpeg).unwrap();
    assert_eq!(
        (p.format, p.width_px, p.height_px, p.scale),
        (PicFormat::Jpeg, 3, 2, 1)
    );

    // GIF89a header, logical screen 3x2.
    let gif = [
        b'G', b'I', b'F', b'8', b'9', b'a', 3, 0, 2, 0, 0, 0, 0, 0x3B,
    ];
    let p = prepare_image(&gif).unwrap();
    assert_eq!(
        (p.format, p.width_px, p.height_px, p.scale),
        (PicFormat::Gif, 3, 2, 1)
    );
}

#[test]
fn webp_is_converted_to_png() {
    let webp = include_bytes!("fixtures/2x2.webp");
    let p = prepare_image(webp).unwrap();
    assert_eq!(p.format, PicFormat::Png);
    assert_eq!((p.width_px, p.height_px, p.scale), (2, 2, 1));
    assert!(p.bytes.starts_with(b"\x89PNG"));
    // The converted bytes decode as a 2x2 PNG.
    let dec = png::Decoder::new(std::io::Cursor::new(&p.bytes));
    let reader = dec.read_info().unwrap();
    assert_eq!((reader.info().width, reader.info().height), (2, 2));
}

#[test]
fn webp_over_pixel_cap_is_refused() {
    // RIFF/WEBP/VP8L header declaring 5000x5000, with no pixel data at all:
    // refusal must come from the header, before any decoding.
    let bits: u32 = 4999 | (4999 << 14);
    let mut chunk = vec![0x2F];
    chunk.extend_from_slice(&bits.to_le_bytes());
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&(4 + 8 + chunk.len() as u32).to_le_bytes());
    webp.extend_from_slice(b"WEBPVP8L");
    webp.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
    webp.extend_from_slice(&chunk);
    assert!(prepare_image(&webp).is_none());
}

#[test]
fn unknown_bytes_are_refused() {
    assert!(prepare_image(b"<svg/>").is_none());
    assert!(picture_from_png(b"<svg/>".to_vec(), 2).is_none());
}

#[test]
fn picture_from_png_keeps_scale() {
    let p = picture_from_png(png_bytes(4, 2), 2).unwrap();
    assert_eq!((p.width_px, p.height_px, p.scale), (4, 2, 2));
}

/// 24 cm in EMU: taller than any picture in `fit_scales_down_never_up`.
const TALL_EMU: u64 = 8_640_000;

#[test]
fn fit_scales_down_never_up() {
    assert_eq!(
        fit_emu(&pic(800, 400, 1), 5_760_720, TALL_EMU),
        (5_760_720, 2_880_360)
    );
    assert_eq!(
        fit_emu(&pic(100, 50, 1), 5_760_720, TALL_EMU),
        (952_500, 476_250)
    );
    assert_eq!(
        fit_emu(&pic(400, 200, 2), 5_760_720, TALL_EMU),
        (1_905_000, 952_500)
    );

    let (w, h) = fit_cm(&pic(1600, 800, 1), 16.0, 24.0);
    assert!((w - 16.0).abs() < 1e-9 && (h - 8.0).abs() < 1e-9);
    let (w, h) = fit_cm(&pic(96, 48, 1), 16.0, 24.0);
    assert!((w - 2.54).abs() < 1e-9 && (h - 1.27).abs() < 1e-9);
    let (w, _) = fit_cm(&pic(192, 96, 2), 16.0, 24.0);
    assert!((w - 2.54).abs() < 1e-9);
}

#[test]
fn fit_keeps_a_tall_picture_on_the_page() {
    // A phone screenshot: at the text width it would be 34.6 cm tall.
    let shot = pic(1170, 2532, 1);
    let (w, h) = fit_emu(&shot, 5_760_720, TALL_EMU);
    assert_eq!(h, TALL_EMU);
    assert_eq!(w, 1170 * TALL_EMU / 2532);
    let (w, h) = fit_cm(&shot, 16.0, 24.0);
    assert!((h - 24.0).abs() < 1e-9, "{h}");
    assert!((w - 24.0 * 1170.0 / 2532.0).abs() < 1e-9, "{w}");

    // Tall but narrow: only the height bound applies, and it never enlarges.
    let (w, h) = fit_emu(&pic(100, 4000, 1), 5_760_720, TALL_EMU);
    assert_eq!((w, h), (100 * TALL_EMU / 4000, TALL_EMU));
    assert_eq!(
        fit_emu(&pic(100, 900, 1), 5_760_720, TALL_EMU),
        (952_500, 8_572_500)
    );
}

#[test]
fn escaping_drops_invalid_chars() {
    assert_eq!(esc_text("a\u{1}<b>&\u{B}"), "a&lt;b&gt;&amp;");
    assert_eq!(esc_text("\"'\u{85}\u{FFFE}ok"), "\"'ok");
    assert_eq!(esc_attr("a\"b'c<&>\u{0}"), "a&quot;b&apos;c&lt;&amp;&gt;");
}

#[test]
fn attribute_escaping_keeps_tabs_and_line_breaks() {
    let s = "line 1\nline 2\r\n\tend";
    assert_eq!(esc_attr(s), "line 1&#10;line 2&#13;&#10;&#9;end");
    // A parser normalises literal whitespace in attributes to spaces; the
    // character references survive.
    let doc = format!("<a v=\"{}\"/>", esc_attr(s));
    let parsed = roxmltree::Document::parse(&doc).unwrap();
    assert_eq!(parsed.root_element().attribute("v"), Some(s));
    assert_eq!(esc_text("a\tb\nc"), "a\tb\nc");
}

#[test]
fn zip_first_entry_can_be_stored() {
    let mut z = ZipOut::new();
    z.stored("mimetype", b"application/x").unwrap();
    z.deflated("a.xml", b"<a/><a/><a/><a/><a/><a/><a/><a/>")
        .unwrap();
    let bytes = z.finish().unwrap();
    let mut ar = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let first = ar.by_index(0).unwrap();
    assert_eq!(first.name(), "mimetype");
    assert_eq!(first.compression(), zip::CompressionMethod::Stored);
    drop(first);
    let second = ar.by_index(1).unwrap();
    assert_eq!(second.name(), "a.xml");
    assert_eq!(second.compression(), zip::CompressionMethod::Deflated);
}
