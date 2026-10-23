//! Page export to DOCX and ODT: snapshot the live page, resolve what needs IO
//! (images, diagram pictures, mention names), and write the file in memory.
//! The pure work lives in `knot-export`; this module feeds it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use chrono::{DateTime, Utc};
use knot_crdt::DocHandle;
use knot_export::{
    Assets, DiagramRef, ExportDoc, Fallbacks, ImageAsset, Meta, picture_from_png, prepare_image,
};
use knot_storage::BlobStoreError;
use uuid::Uuid;

use crate::AppState;
use crate::routes::api::markdown::live_snapshot;

/// Image bytes one export may embed (40 MiB), counted after preparation.
pub const IMAGE_BUDGET_BYTES: i64 = 40 * 1024 * 1024;
/// Diagram pictures are rendered at twice their display size.
pub const DIAGRAM_SCALE: u32 = 2;
/// Exports one server process runs at a time.
pub const MAX_CONCURRENT_EXPORTS: usize = 2;
/// How long an export waits for a free slot before it is refused as busy.
pub const BUSY_WAIT: Duration = Duration::from_secs(5);
/// Longest file name before the extension, in code points.
const MAX_FILE_STEM_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Docx,
    Odt,
}

impl Format {
    /// `docx` or `odt`, exactly as they appear in the route.
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "docx" => Some(Self::Docx),
            "odt" => Some(Self::Odt),
            _ => None,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Self::Odt => "application/vnd.oasis.opendocument.text",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Odt => "odt",
        }
    }
}

/// A written export.
pub struct Built {
    pub bytes: Vec<u8>,
    pub fallbacks: Fallbacks,
    pub title: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportFailure {
    #[error("room: {0}")]
    Room(String),
    #[error("store: {0}")]
    Store(String),
    #[error("write: {0}")]
    Write(String),
}

fn store_err(e: impl std::fmt::Display) -> ExportFailure {
    ExportFailure::Store(e.to_string())
}

fn not_configured(what: &str) -> ExportFailure {
    ExportFailure::Store(format!("{what} not configured"))
}

/// The page's live state, copied into a transient doc by the same
/// [`live_snapshot`] the markdown export uses. The room actor only encodes
/// its state, so it keeps serving editors while the export walks the copy.
pub async fn snapshot(state: &AppState, doc_id: Uuid) -> Result<DocHandle, ExportFailure> {
    let (transient, _seq) = live_snapshot(state, doc_id)
        .await
        .map_err(|e| ExportFailure::Room(e.to_string()))?;
    Ok(transient)
}

/// [`resolve_assets_with_budget`] with the standard image budget.
pub async fn resolve_assets(
    state: &AppState,
    user_id: Uuid,
    doc: &ExportDoc,
    diagrams: HashMap<String, Vec<u8>>,
) -> Result<Assets, ExportFailure> {
    resolve_assets_with_budget(state, user_id, doc, diagrams, IMAGE_BUDGET_BYTES).await
}

/// Everything the writers need from outside the page:
/// - each knot image, if `user_id` may read the page it belongs to and it
///   fits in `budget`;
/// - a picture for each diagram on the page, from `diagrams` (keys that
///   aren't on the page are ignored);
/// - the current display name of each mentioned user.
pub async fn resolve_assets_with_budget(
    state: &AppState,
    user_id: Uuid,
    doc: &ExportDoc,
    mut diagrams: HashMap<String, Vec<u8>>,
    budget: i64,
) -> Result<Assets, ExportFailure> {
    let mut assets = Assets::default();

    let mut spent = 0;
    for id in knot_export::blob_ids(doc) {
        let image = resolve_image(state, user_id, &id, budget, &mut spent).await?;
        assets.images.insert(id, image);
    }

    for diagram in knot_export::diagrams(doc) {
        let (DiagramRef::Mermaid { key, .. } | DiagramRef::Board { key, .. }) = diagram;
        let picture = diagrams
            .remove(&key)
            .and_then(|png| picture_from_png(png, DIAGRAM_SCALE));
        if let Some(picture) = picture {
            assets.diagrams.insert(key, picture);
        }
    }

    for id in knot_export::user_ids(doc) {
        let users = state
            .users
            .as_ref()
            .ok_or_else(|| not_configured("users"))?;
        let Ok(mentioned) = Uuid::parse_str(&id) else {
            continue;
        };
        if let Some(user) = users.find_by_id(mentioned).await.map_err(store_err)? {
            assets.user_names.insert(id, user.display_name);
        }
    }
    Ok(assets)
}

/// One knot image. Access is checked on the blob's own page, like
/// `GET /api/blobs/{id}`, so an image copied from a page the reader can't
/// open stays out. `spent` is the running total of embedded bytes.
async fn resolve_image(
    state: &AppState,
    user_id: Uuid,
    id: &str,
    budget: i64,
    spent: &mut i64,
) -> Result<ImageAsset, ExportFailure> {
    let (Some(blob_meta), Some(acl), Some(blobs)) = (
        state.blob_meta.as_ref(),
        state.acl.as_ref(),
        state.blob_store.as_ref(),
    ) else {
        return Err(not_configured("blob storage"));
    };
    let Ok(blob_id) = Uuid::parse_str(id) else {
        return Ok(ImageAsset::Unavailable);
    };
    let Some(meta) = blob_meta.find(blob_id).await.map_err(store_err)? else {
        return Ok(ImageAsset::Unavailable);
    };
    let role = acl
        .effective_role(meta.workspace_id, meta.doc_id, user_id)
        .await
        .map_err(store_err)?;
    if role.is_none() {
        return Ok(ImageAsset::Unavailable);
    }
    // Decide on the stored size first, so an image past the budget is never
    // fetched.
    if spent.saturating_add(meta.byte_size) > budget {
        return Ok(ImageAsset::OverBudget);
    }
    let bytes = match blobs.get(blob_id).await {
        Ok(bytes) => bytes,
        Err(BlobStoreError::NotFound) => return Ok(ImageAsset::Unavailable),
        Err(e) => return Err(store_err(e)),
    };
    let picture = match tokio::task::spawn_blocking(move || prepare_image(&bytes)).await {
        Ok(Some(picture)) => picture,
        Ok(None) => return Ok(ImageAsset::Unavailable),
        Err(e) => {
            tracing::warn!(%blob_id, error = %e, "export image preparation failed");
            return Ok(ImageAsset::Unavailable);
        }
    };
    // Charge what gets embedded: a WebP converted to PNG can outgrow the
    // stored file.
    let total = spent.saturating_add(i64::try_from(picture.bytes.len()).unwrap_or(i64::MAX));
    if total > budget {
        return Ok(ImageAsset::OverBudget);
    }
    *spent = total;
    Ok(ImageAsset::Embedded(picture))
}

/// Export the page's live state as `format`. `diagram_pngs` holds the
/// browser's diagram pictures by key. The caller holds an export permit.
pub async fn build(
    state: &AppState,
    user_id: Uuid,
    doc_id: Uuid,
    format: Format,
    diagram_pngs: HashMap<String, Vec<u8>>,
) -> Result<Built, ExportFailure> {
    let handle = snapshot(state, doc_id).await?;
    let walked = tokio::task::spawn_blocking(move || knot_export::walk(handle.inner()))
        .await
        .map_err(|e| ExportFailure::Write(format!("walk: {e}")))?;
    let assets = resolve_assets(state, user_id, &walked.doc, diagram_pngs).await?;
    let docs = state.docs.as_ref().ok_or_else(|| not_configured("docs"))?;
    let title = docs
        .get(doc_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ExportFailure::Store(format!("doc {doc_id} not found")))?
        .title;
    let meta = Meta {
        title: title.clone(),
        exported_on: exported_on(Utc::now()),
        base_url: state.base_url.clone(),
    };
    let doc = walked.doc;
    let output = tokio::task::spawn_blocking(move || match format {
        Format::Docx => knot_export::write_docx(&doc, &meta, &assets),
        Format::Odt => knot_export::write_odt(&doc, &meta, &assets),
    })
    .await
    .map_err(|e| ExportFailure::Write(e.to_string()))?
    .map_err(|e| ExportFailure::Write(e.to_string()))?;
    let mut fallbacks = output.fallbacks;
    fallbacks.unknown_element = walked.unknown_elements.len() as u32;
    record_fallbacks(doc_id, &fallbacks);
    Ok(Built {
        bytes: output.bytes,
        fallbacks,
        title,
    })
}

/// Count and log each kind of content the export could not include as-is.
fn record_fallbacks(doc_id: Uuid, fallbacks: &Fallbacks) {
    for (kind, count) in [
        ("diagram", fallbacks.diagram),
        ("image_unavailable", fallbacks.image_unavailable),
        ("image_over_budget", fallbacks.image_over_budget),
        ("unknown_element", fallbacks.unknown_element),
    ] {
        if count > 0 {
            metrics::counter!("knot_export_fallback_total", "kind" => kind)
                .increment(u64::from(count));
            tracing::warn!(%doc_id, kind, count, "export fallback");
        }
    }
}

/// The download name for a page titled `title`. The browser applies the
/// same rule (`exportFileName`), so both must change together.
pub fn file_name(title: &str, format: Format) -> String {
    fn trim(s: &str) -> &str {
        s.trim_matches([' ', '.'])
    }
    let replaced: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    let capped: String = trim(&replaced).chars().take(MAX_FILE_STEM_CHARS).collect();
    let stem = match trim(&capped) {
        "" => "Untitled",
        stem => stem,
    };
    format!("{stem}.{}", format.extension())
}

/// An RFC 6266 attachment header: a quoted ASCII fallback (`_` for anything
/// else) for old clients, and the exact name as RFC 8187 `filename*`.
pub fn content_disposition(file_name: &str) -> String {
    let fallback: String = file_name
        .chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            ' '..='~' => c,
            _ => '_',
        })
        .collect();
    let mut encoded = String::new();
    for b in file_name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(char::from(b));
        } else {
            let _ = write!(encoded, "%{b:02X}");
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// The export date shown under the title, e.g. "23 Oct 2026". The DOCX
/// writer derives the document's dates from this exact form.
pub fn exported_on(now: DateTime<Utc>) -> String {
    now.format("%-d %b %Y").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn file_name_replaces_reserved_characters() {
        assert_eq!(
            file_name("Q4 / Plan: <draft>?", Format::Docx),
            "Q4 - Plan- -draft--.docx"
        );
    }

    #[test]
    fn file_name_falls_back_to_untitled() {
        assert_eq!(file_name(" .. ", Format::Odt), "Untitled.odt");
    }

    #[test]
    fn file_name_caps_code_points_and_replaces_controls() {
        let long = format!("{}😀😀", "a".repeat(119));
        assert_eq!(
            file_name(&long, Format::Docx),
            format!("{}😀.docx", "a".repeat(119))
        );
        assert_eq!(file_name("a\u{1}b\u{9f}c", Format::Odt), "a-b-c.odt");
        assert_eq!(file_name("  Notes. ", Format::Odt), "Notes.odt");
    }

    #[test]
    fn content_disposition_percent_encodes_utf8() {
        let header = content_disposition("Über.docx");
        assert!(header.starts_with("attachment; "), "{header}");
        assert!(header.contains(r#"filename="_ber.docx""#), "{header}");
        assert!(
            header.contains("filename*=UTF-8''%C3%9Cber.docx"),
            "{header}"
        );
    }

    #[test]
    fn content_disposition_keeps_the_ascii_fallback_quotable() {
        let header = content_disposition("a\"b\\c d.odt");
        assert!(header.contains(r#"filename="a_b_c d.odt""#), "{header}");
        assert!(
            header.contains("filename*=UTF-8''a%22b%5Cc%20d.odt"),
            "{header}"
        );
    }

    #[test]
    fn exported_on_is_day_month_year() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 10, 23, 23, 59, 0)
            .unwrap();
        assert_eq!(exported_on(now), "23 Oct 2026");
    }

    #[test]
    fn format_parses_its_own_extension_only() {
        for format in [Format::Docx, Format::Odt] {
            assert_eq!(Format::parse(format.extension()), Some(format));
        }
        assert_eq!(Format::parse("pdf"), None);
        assert_eq!(Format::parse("DOCX"), None);
        assert_eq!(
            Format::Docx.content_type(),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        assert_eq!(
            Format::Odt.content_type(),
            "application/vnd.oasis.opendocument.text"
        );
    }
}
