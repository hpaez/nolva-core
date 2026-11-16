//! Local persistence for Nolva.
//!
//! - [`SqliteDocumentRepository`]: document/page metadata in SQLite.
//! - [`LocalPageFileStore`]: page images on the file system.
//!
//! Layout under the storage root:
//!
//! ```text
//! <root>/
//!   nolva.sqlite3
//!   documents/<document-id>/pages/<page-id>/original.<ext>
//! ```

mod files;
mod migrations;
mod sqlite;

pub use files::LocalPageFileStore;
pub use sqlite::SqliteDocumentRepository;

/// File name of the metadata database inside the storage root.
pub const DATABASE_FILE_NAME: &str = "nolva.sqlite3";
