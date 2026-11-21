//! Domain model and use cases for Nolva documents.
//!
//! This crate is platform-agnostic and has no I/O of its own: persistence and
//! file handling and image processing are expressed as ports
//! ([`DocumentRepository`], [`PageFileStore`], [`PageProcessor`]) that other
//! crates implement.

mod clock;
mod error;
mod model;
mod ports;
mod service;

pub use clock::{Clock, SystemClock};
pub use error::{DocumentError, PortError, ProcessingError};
pub use model::{Crop, Document, DocumentId, ImageFormat, NewPage, Page, PageId};
pub use ports::{
    CornerDetection, DocumentExporter, DocumentRepository, ExportSummary, PageFileStore,
    PageProcessor, PaperSize, PdfExportOptions,
};
pub use service::{DocumentService, MAX_TITLE_CHARS};
