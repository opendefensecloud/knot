//! Pure export of a knot page (a yrs document) to an in-memory model that the
//! DOCX and ODT writers consume.

pub mod assets;
pub mod docx;
pub mod media;
pub mod model;
pub mod odt;
pub mod tables;
pub mod walk;
pub mod xml;

pub use assets::{Assets, ExportError, Fallbacks, ImageAsset, Meta, Output, PicFormat, Picture};
pub use docx::write_docx;
pub use media::{picture_from_png, prepare_image};
pub use model::*;
pub use odt::write_odt;
pub use walk::{
    DiagramRef, MAX_DEPTH, MAX_SPAN, MAX_TABLE_COLUMNS, MAX_TABLE_GRID_CELLS, Walked, blob_ids,
    diagrams, user_ids, walk,
};
