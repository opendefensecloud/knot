//! XML escaping and the in-memory zip writer shared by the DOCX and ODT writers.

use std::io::{Cursor, Write};

use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::assets::ExportError;

/// Most XML one document may write (64 MiB). The walker's limits keep real
/// pages far below it; a writer that passes it stops and fails with
/// [`ExportError::TooLarge`] rather than run the server out of memory.
pub const MAX_XML_BYTES: usize = 64 * 1024 * 1024;

/// Room in the zip buffer beyond its entries' data: headers, the central
/// directory, and deflate's overhead on data that doesn't compress.
const ZIP_SLACK_BYTES: usize = 64 * 1024;

fn invalid_xml_char(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{8}' | '\u{B}' | '\u{C}' | '\u{E}'..='\u{1F}'
        | '\u{7F}'..='\u{9F}' | '\u{FFFE}' | '\u{FFFF}')
}

fn esc(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if invalid_xml_char(c) {
            continue;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            '\'' if attr => out.push_str("&apos;"),
            // Attribute-value normalisation would turn these into spaces.
            '\t' if attr => out.push_str("&#9;"),
            '\n' if attr => out.push_str("&#10;"),
            '\r' if attr => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape character data, dropping characters XML 1.0 forbids.
pub fn esc_text(s: &str) -> String {
    esc(s, false)
}

/// Escape an attribute value, dropping characters XML 1.0 forbids. Tabs and
/// line breaks become character references so they survive parsing.
pub fn esc_attr(s: &str) -> String {
    esc(s, true)
}

/// A zip archive built in memory. Entries are written in call order.
pub struct ZipOut {
    zip: ZipWriter<Cursor<Vec<u8>>>,
}

impl Default for ZipOut {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipOut {
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// A zip whose entries hold about `data_bytes` in all. The buffer is
    /// sized for them up front, so a large package is never copied while
    /// it grows.
    pub fn with_capacity(data_bytes: usize) -> Self {
        let buf = Vec::with_capacity(data_bytes.saturating_add(ZIP_SLACK_BYTES));
        Self {
            zip: ZipWriter::new(Cursor::new(buf)),
        }
    }

    fn add(
        &mut self,
        name: &str,
        pieces: &[&[u8]],
        method: CompressionMethod,
    ) -> Result<(), ExportError> {
        let opts = SimpleFileOptions::default().compression_method(method);
        self.zip
            .start_file(name, opts)
            .map_err(|e| ExportError::Zip(e.to_string()))?;
        for piece in pieces {
            self.zip
                .write_all(piece)
                .map_err(|e| ExportError::Io(e.to_string()))?;
        }
        Ok(())
    }

    /// Store without compression (the ODT mimetype entry, and media files).
    pub fn stored(&mut self, name: &str, data: &[u8]) -> Result<(), ExportError> {
        self.add(name, &[data], CompressionMethod::Stored)
    }

    pub fn deflated(&mut self, name: &str, data: &[u8]) -> Result<(), ExportError> {
        self.add(name, &[data], CompressionMethod::Deflated)
    }

    /// One deflated entry holding `pieces` one after another, so a part
    /// built in pieces is never copied into one buffer first.
    pub fn deflated_pieces(&mut self, name: &str, pieces: &[&[u8]]) -> Result<(), ExportError> {
        self.add(name, pieces, CompressionMethod::Deflated)
    }

    pub fn finish(self) -> Result<Vec<u8>, ExportError> {
        self.zip
            .finish()
            .map(Cursor::into_inner)
            .map_err(|e| ExportError::Zip(e.to_string()))
    }
}
