//! Image sniffing, WebP conversion and size fitting.

use crate::assets::{PicFormat, Picture};

/// WebP pixel cap (4096 x 4096), checked before decoding.
const MAX_WEBP_PIXELS: u64 = 16_777_216;
const EMU_PER_PX: u64 = 9525;
const CM_PER_PX: f64 = 2.54 / 96.0;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
    let s = imagesize::blob_size(bytes).ok()?;
    let (w, h) = (u32::try_from(s.width).ok()?, u32::try_from(s.height).ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// Accept PNG, JPEG, GIF and WebP; WebP is converted to PNG. Anything else, or
/// any failure, yields `None`.
pub fn prepare_image(bytes: &[u8]) -> Option<Picture> {
    let format = if bytes.starts_with(PNG_MAGIC) {
        PicFormat::Png
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        PicFormat::Jpeg
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        PicFormat::Gif
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return prepare_webp(bytes);
    } else {
        return None;
    };
    let (width_px, height_px) = dims(bytes)?;
    Some(Picture {
        format,
        bytes: bytes.to_vec(),
        width_px,
        height_px,
        scale: 1,
    })
}

fn prepare_webp(bytes: &[u8]) -> Option<Picture> {
    let (w, h) = dims(bytes)?;
    if u64::from(w) * u64::from(h) > MAX_WEBP_PIXELS {
        return None;
    }
    let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    if dec.dimensions() != (w, h) {
        return None;
    }
    let has_alpha = dec.has_alpha();
    let mut buf = vec![0u8; dec.output_buffer_size()?];
    dec.read_image(&mut buf).ok()?;
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(if has_alpha {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&buf).ok()?;
        writer.finish().ok()?;
    }
    Some(Picture {
        format: PicFormat::Png,
        bytes: out,
        width_px: w,
        height_px: h,
        scale: 1,
    })
}

/// A diagram part: PNG only, with its dimensions checked.
pub fn picture_from_png(bytes: Vec<u8>, scale: u32) -> Option<Picture> {
    if scale == 0 || !bytes.starts_with(PNG_MAGIC) {
        return None;
    }
    let (width_px, height_px) = dims(&bytes)?;
    Some(Picture {
        format: PicFormat::Png,
        bytes,
        width_px,
        height_px,
        scale,
    })
}

/// Size in EMU: natural `px / scale` at 96 dpi, shrunk proportionally to
/// `max_width_emu`, never enlarged.
pub fn fit_emu(p: &Picture, max_width_emu: u64) -> (u64, u64) {
    let scale = u64::from(p.scale.max(1));
    let nat_w = u64::from(p.width_px) * EMU_PER_PX / scale;
    let nat_h = u64::from(p.height_px) * EMU_PER_PX / scale;
    if nat_w <= max_width_emu {
        return (nat_w, nat_h);
    }
    let h = u128::from(nat_h) * u128::from(max_width_emu) / u128::from(nat_w);
    (max_width_emu, h as u64)
}

/// Size in centimetres at 96 dpi, same rule as [`fit_emu`].
pub fn fit_cm(p: &Picture, max_width_cm: f64) -> (f64, f64) {
    let scale = f64::from(p.scale.max(1));
    let nat_w = f64::from(p.width_px) / scale * CM_PER_PX;
    let nat_h = f64::from(p.height_px) / scale * CM_PER_PX;
    if nat_w <= max_width_cm {
        return (nat_w, nat_h);
    }
    (max_width_cm, nat_h * max_width_cm / nat_w)
}
