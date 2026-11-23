use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{PortError, ProcessingError};
use crate::model::{Crop, Document, DocumentId, ImageFormat, NewPage, Page, PageFilter, PageId};
use crate::text::{PageText, TextMatch};

/// Persistence of document and page metadata.
pub trait DocumentRepository: Send + Sync {
    fn insert_document(&self, document: &Document) -> Result<(), PortError>;

    fn get_document(&self, id: DocumentId) -> Result<Option<Document>, PortError>;

    /// All documents, most recently updated first.
    fn list_documents(&self) -> Result<Vec<Document>, PortError>;

    /// Appends a page at the end of its document and bumps the document's
    /// `updated_at_ms`, atomically. The repository assigns the position, so
    /// concurrent appends cannot collide.
    fn append_page(&self, page: &NewPage) -> Result<Page, PortError>;

    /// Pages of a document ordered by position.
    fn list_pages(&self, document_id: DocumentId) -> Result<Vec<Page>, PortError>;

    fn get_page(&self, id: PageId) -> Result<Option<Page>, PortError>;

    /// Renames a document and bumps its `updated_at_ms`. Returns the updated
    /// document, or `None` if it doesn't exist.
    fn rename_document(
        &self,
        id: DocumentId,
        title: &str,
        updated_at_ms: i64,
    ) -> Result<Option<Document>, PortError>;

    /// Deletes a document and all its pages. Returns whether it existed.
    fn delete_document(&self, id: DocumentId) -> Result<bool, PortError>;

    /// Deletes a page, closes the gap in its document's positions and bumps
    /// the document's `updated_at_ms`, atomically. Returns whether it existed.
    fn delete_page(&self, id: PageId, updated_at_ms: i64) -> Result<bool, PortError>;

    /// Moves a page to `to_position` (clamped to the document), shifting the
    /// pages in between, and bumps the document's `updated_at_ms`, atomically.
    /// Returns the document's pages in their new order.
    fn move_page(
        &self,
        id: PageId,
        to_position: u32,
        updated_at_ms: i64,
    ) -> Result<Vec<Page>, PortError>;

    /// Records how a page is rendered (crop, filter and the resulting
    /// processed image, if any) and bumps its document's `updated_at_ms`,
    /// atomically. Returns the updated page.
    fn set_page_rendering(
        &self,
        id: PageId,
        crop: Option<&Crop>,
        filter: PageFilter,
        processed_path: Option<&Path>,
        updated_at_ms: i64,
    ) -> Result<Page, PortError>;

    /// Every stored file the database points to: the original and the
    /// processed image (if any) of every page, relative to the storage root.
    fn referenced_files(&self) -> Result<Vec<PathBuf>, PortError>;

    /// Stores the recognized text of a page, replacing any previous one, and
    /// indexes `plain_text` for search, atomically.
    fn save_page_text(&self, text: &PageText, plain_text: &str) -> Result<(), PortError>;

    /// The stored text of a page, current or not.
    fn get_page_text(&self, page_id: PageId) -> Result<Option<PageText>, PortError>;

    /// Up to `limit` pages with no current text (none stored, or recognized
    /// in an image the page no longer shows), most recently added first.
    fn pages_without_text(&self, limit: u32) -> Result<Vec<Page>, PortError>;

    /// Pages whose current text matches every word of `query` (each word
    /// also matches as a prefix; case and accents are ignored), best match
    /// first. `query` is user input, not search syntax.
    fn search_text(&self, query: &str, limit: u32) -> Result<Vec<TextMatch>, PortError>;
}

/// What a cleanup of unreferenced files removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanupSummary {
    pub files_removed: u32,
    pub bytes_freed: u64,
}

/// Storage of page image files.
pub trait PageFileStore: Send + Sync {
    /// Copies `source` into managed storage and returns the stored path,
    /// relative to the storage root.
    fn import_original(
        &self,
        document_id: DocumentId,
        page_id: PageId,
        format: ImageFormat,
        source: &Path,
    ) -> Result<PathBuf, PortError>;

    /// A fresh, unique path (relative to the storage root) for a new
    /// processed image of the page. Unique so a new crop never overwrites
    /// the file the database still points to.
    fn new_processed_path(&self, document_id: DocumentId, page_id: PageId) -> PathBuf;

    /// Removes a single stored file. Missing files are not an error.
    fn remove_file(&self, relative: &Path) -> Result<(), PortError>;

    /// Removes every file belonging to a document. Missing files are not an error.
    fn remove_document_files(&self, document_id: DocumentId) -> Result<(), PortError>;

    /// Removes every file belonging to a page. Missing files are not an error.
    fn remove_page_files(&self, document_id: DocumentId, page_id: PageId) -> Result<(), PortError>;

    /// Resolves a stored relative path to an absolute one.
    fn resolve(&self, relative: &Path) -> PathBuf;

    /// Deletes stored files that aren't in `referenced` (paths relative to
    /// the storage root, as [`DocumentRepository::referenced_files`] returns
    /// them), and the folders left empty. Anything modified within `grace`
    /// is kept: it may belong to an import or render still in progress,
    /// whose database row isn't written yet.
    fn remove_unreferenced(
        &self,
        referenced: &HashSet<PathBuf>,
        grace: Duration,
    ) -> Result<CleanupSummary, PortError>;
}

/// Corners found automatically on a page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CornerDetection {
    pub crop: Crop,
    /// `0.0..=1.0`; low values mean the user should check the corners.
    pub confidence: f32,
}

/// Image analysis and processing of page images.
pub trait PageProcessor: Send + Sync {
    /// Finds the page outline in the image at `original`, if any.
    fn detect_corners(&self, original: &Path) -> Result<Option<CornerDetection>, ProcessingError>;

    /// Writes `original`, straightened to `crop` when given and with
    /// `filter` applied, to `output`, atomically.
    fn render_page(
        &self,
        original: &Path,
        crop: Option<&Crop>,
        filter: PageFilter,
        output: &Path,
    ) -> Result<(), ProcessingError>;
}

/// Paper size for exported PDFs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperSize {
    /// Each page takes the proportions of its image.
    FitImage,
    A4,
    Letter,
    /// 8.5 × 14 in, the usual "oficio".
    Legal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfExportOptions {
    pub paper_size: PaperSize,
    /// White margin on fixed-size pages, in millimetres.
    pub margin_mm: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportSummary {
    pub page_count: u32,
    pub size_bytes: u64,
}

/// Turns page images into shareable files.
pub trait DocumentExporter: Send + Sync {
    /// Writes a PDF with one page per image, in order, to `output` (atomically).
    fn export_pdf(
        &self,
        title: &str,
        images: &[PathBuf],
        options: &PdfExportOptions,
        output: &Path,
    ) -> Result<ExportSummary, ProcessingError>;
}
