//! Pure export of a knot page (a yrs document) to an in-memory model that the
//! DOCX and ODT writers consume.

pub mod assets;
pub mod media;
pub mod model;
pub mod tables;
pub mod walk;
pub mod xml;

pub use model::*;
pub use walk::{DiagramRef, Walked, blob_ids, diagrams, user_ids, walk};
