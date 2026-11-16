use std::path::{Path, PathBuf};

use crate::error::PortError;
use crate::model::{Document, DocumentId, ImageFormat, NewPage, Page, PageId};

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

    /// Removes every file belonging to a page. Missing files are not an error.
    fn remove_page_files(&self, document_id: DocumentId, page_id: PageId) -> Result<(), PortError>;

    /// Resolves a stored relative path to an absolute one.
    fn resolve(&self, relative: &Path) -> PathBuf;
}
