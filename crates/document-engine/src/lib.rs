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
mod text;

pub use clock::{Clock, SystemClock};
pub use error::{DocumentError, PortError, ProcessingError};
pub use model::{Crop, Document, DocumentId, ImageFormat, NewPage, Page, PageFilter, PageId};
pub use ports::{
    CleanupSummary, CornerDetection, DocumentExporter, DocumentRepository, DocxExportOptions,
    ExportPage, ExportSummary, PageFileStore, PageProcessor, PaperSize, PdfExportOptions,
};
pub use service::{DocumentService, DocxExport, MAX_SEARCH_RESULTS, MAX_TITLE_CHARS, ORPHAN_GRACE};
pub use text::{
    MAX_LINES_PER_PAGE, MAX_TEXT_CHARS_PER_PAGE, PageText, TextBounds, TextLine, TextMatch,
};
