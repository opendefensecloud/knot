//! Pure export of a knot page (a yrs document) to an in-memory model that the
//! DOCX and ODT writers consume.

pub mod model;
pub mod tables;
pub mod walk;

pub use model::*;
pub use walk::{Walked, walk};
