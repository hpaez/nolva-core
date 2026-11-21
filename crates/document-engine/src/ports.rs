use std::path::{Path, PathBuf};

use crate::error::{PortError, ProcessingError};
use crate::model::{Crop, Document, DocumentId, ImageFormat, NewPage, Page, PageId};

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

    /// Records a page's crop and processed image, and bumps its document's
    /// `updated_at_ms`, atomically. Returns the updated page.
    fn set_page_crop(
        &self,
        id: PageId,
        crop: &Crop,
        processed_path: &Path,
        updated_at_ms: i64,
    ) -> Result<Page, PortError>;
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

    /// Removes every file belonging to a page. Missing files are not an error.
    fn remove_page_files(&self, document_id: DocumentId, page_id: PageId) -> Result<(), PortError>;

    /// Resolves a stored relative path to an absolute one.
    fn resolve(&self, relative: &Path) -> PathBuf;
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

    /// Writes the perspective-corrected region of `original` outlined by
    /// `crop` to `output`, atomically.
    fn render_crop(
        &self,
        original: &Path,
        crop: &Crop,
        output: &Path,
    ) -> Result<(), ProcessingError>;
}
