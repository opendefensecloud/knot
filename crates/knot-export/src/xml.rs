//! XML escaping and the in-memory zip writer shared by the DOCX and ODT writers.

use std::io::{Cursor, Write};

use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::assets::ExportError;

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
            _ => out.push(c),
        }
    }
    out
}

/// Escape character data, dropping characters XML 1.0 forbids.
pub fn esc_text(s: &str) -> String {
    esc(s, false)
}

/// Escape an attribute value, dropping characters XML 1.0 forbids.
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
        Self {
            zip: ZipWriter::new(Cursor::new(Vec::new())),
        }
    }

    fn add(
        &mut self,
        name: &str,
        data: &[u8],
        method: CompressionMethod,
    ) -> Result<(), ExportError> {
        let opts = SimpleFileOptions::default().compression_method(method);
        self.zip
            .start_file(name, opts)
            .map_err(|e| ExportError::Zip(e.to_string()))?;
        self.zip
            .write_all(data)
            .map_err(|e| ExportError::Io(e.to_string()))
    }

    /// Store without compression (the ODT mimetype entry, and media files).
    pub fn stored(&mut self, name: &str, data: &[u8]) -> Result<(), ExportError> {
        self.add(name, data, CompressionMethod::Stored)
    }

    pub fn deflated(&mut self, name: &str, data: &[u8]) -> Result<(), ExportError> {
        self.add(name, data, CompressionMethod::Deflated)
    }

    pub fn finish(self) -> Result<Vec<u8>, ExportError> {
        self.zip
            .finish()
            .map(Cursor::into_inner)
            .map_err(|e| ExportError::Zip(e.to_string()))
    }
}
