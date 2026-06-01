//! Storage layer for knot — Postgres pool + storage traits.

pub mod doc_store;
pub mod pool;

pub use doc_store::{DocStore, DocStoreError};
pub use pool::{Pool, PoolError, connect};
