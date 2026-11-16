//! Domain model and use cases for Nolva documents.
//!
//! This crate is platform-agnostic and has no I/O of its own: persistence and
//! file handling are expressed as ports ([`DocumentRepository`],
//! [`PageFileStore`]) that other crates implement.

mod clock;
mod error;
mod model;
mod ports;
mod service;

pub use clock::{Clock, SystemClock};
pub use error::{DocumentError, PortError};
pub use model::{Document, DocumentId, ImageFormat, NewPage, Page, PageId};
pub use ports::{DocumentRepository, PageFileStore};
pub use service::{DocumentService, MAX_TITLE_CHARS};
