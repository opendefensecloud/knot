//! Inputs prepared by the caller (images, diagrams, user names) and the output
//! of an export.

use std::collections::HashMap;

/// Document metadata shown in the title block.
pub struct Meta {
    pub title: String,
    /// Already formatted, e.g. "23 Oct 2026".
    pub exported_on: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PicFormat {
    Png,
    Jpeg,
    Gif,
}

#[derive(Debug, Clone)]
pub struct Picture {
    pub format: PicFormat,
    pub bytes: Vec<u8>,
    pub width_px: u32,
    pub height_px: u32,
    /// 1 for images, 2 for diagrams (rendered at twice the display size).
    pub scale: u32,
}

#[derive(Debug, Clone)]
pub enum ImageAsset {
    Embedded(Picture),
    Unavailable,
    OverBudget,
}

#[derive(Default)]
pub struct Assets {
    /// Key: blob id. Absent means unavailable.
    pub images: HashMap<String, ImageAsset>,
    /// Key: diagram key. Absent means fallback.
    pub diagrams: HashMap<String, Picture>,
    /// User id to display name.
    pub user_names: HashMap<String, String>,
}

#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct Fallbacks {
    pub diagram: u32,
    pub image_unavailable: u32,
    pub image_over_budget: u32,
    pub unknown_element: u32,
}

pub struct Output {
    pub bytes: Vec<u8>,
    pub fallbacks: Fallbacks,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("zip: {0}")]
    Zip(String),
    #[error("io: {0}")]
    Io(String),
    /// The document's XML passed [`crate::xml::MAX_XML_BYTES`].
    #[error("document too large: over {} MiB of XML", crate::xml::MAX_XML_BYTES >> 20)]
    TooLarge,
}
