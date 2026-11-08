//! Helpers for tests in this and other crates. Not part of the API.

use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt;

use sqlx::error::{DatabaseError, ErrorKind};

pub use crate::updates_store::CANCEL_ABANDONED_SQL;

/// A database error with SQLSTATE `sqlstate`, as Postgres would return it, for
/// test doubles: `UpdatesStoreError::Sqlx(db_error("57014"))`.
pub fn db_error(sqlstate: &'static str) -> sqlx::Error {
    sqlx::Error::Database(Box::new(FakeDbError { code: sqlstate }))
}

#[derive(Debug)]
struct FakeDbError {
    code: &'static str,
}

impl fmt::Display for FakeDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "test error with SQLSTATE {}", self.code)
    }
}

impl StdError for FakeDbError {}

impl DatabaseError for FakeDbError {
    fn message(&self) -> &str {
        "test error"
    }

    fn code(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(self.code))
    }

    fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
        self
    }

    fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
        self
    }

    fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
        self
    }

    fn kind(&self) -> ErrorKind {
        match self.code {
            "23505" => ErrorKind::UniqueViolation,
            "23503" => ErrorKind::ForeignKeyViolation,
            _ => ErrorKind::Other,
        }
    }
}
