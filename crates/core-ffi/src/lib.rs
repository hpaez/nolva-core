//! Public API of the Nolva core for the native apps, exported with UniFFI.
//!
//! Design rules for this surface:
//! - Pass identifiers and file paths across the boundary, never image bytes.
//! - Identifiers are strings (UUIDs) so they stay simple in Swift and Kotlin.
//! - Calls are synchronous and may block on I/O: native code must invoke them
//!   off the main thread (Dispatchers.IO / a background Task).
//! - Keep this crate thin: it only maps types and errors; logic lives in the
//!   engine crates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use document_engine::{
    Document, DocumentError, DocumentId, DocumentService, Page, PageFileStore, SystemClock,
};
use storage_engine::{DATABASE_FILE_NAME, LocalPageFileStore, SqliteDocumentRepository};

uniffi::setup_scaffolding!();

/// Version of the core library, for diagnostics and compatibility checks.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Configuration supplied by the host app at startup.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreConfig {
    /// Absolute path of a private, app-owned directory where the core keeps
    /// its database and files (e.g. `filesDir` on Android, Application
    /// Support on iOS). Created if missing.
    pub data_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DocumentInfo {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub page_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PageInfo {
    pub id: String,
    pub document_id: String,
    pub position: u32,
    /// Absolute path of the original image, resolved for the current install.
    pub original_path: String,
    pub created_at_ms: i64,
}

/// Errors surfaced to Swift (`throws`) and Kotlin (exceptions).
///
/// Messages are for logs and diagnostics, not for end users: map the variant
/// to localized UI text on each platform.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NolvaError {
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },
    #[error("not found: {message}")]
    NotFound { message: String },
    #[error("unsupported: {message}")]
    Unsupported { message: String },
    #[error("storage error: {message}")]
    Storage { message: String },
}

impl From<DocumentError> for NolvaError {
    fn from(err: DocumentError) -> Self {
        let message = err.to_string();
        match err {
            DocumentError::InvalidTitle { .. } | DocumentError::InvalidId { .. } => {
                Self::InvalidArgument { message }
            }
            DocumentError::DocumentNotFound(_) | DocumentError::SourceImageNotFound(_) => {
                Self::NotFound { message }
            }
            DocumentError::UnsupportedImageFormat { .. } => Self::Unsupported { message },
            DocumentError::Storage(_) => Self::Storage { message },
        }
    }
}

type Service = DocumentService<SqliteDocumentRepository, LocalPageFileStore, SystemClock>;

/// Entry point of the core. Create one per process and share it.
#[derive(Debug, uniffi::Object)]
pub struct NolvaCore {
    service: Service,
}

#[uniffi::export]
impl NolvaCore {
    #[uniffi::constructor]
    pub fn new(config: CoreConfig) -> Result<Arc<Self>, NolvaError> {
        let data_dir = PathBuf::from(&config.data_dir);
        if !data_dir.is_absolute() {
            return Err(NolvaError::InvalidArgument {
                message: "data_dir must be an absolute path".into(),
            });
        }
        let files = LocalPageFileStore::new(&data_dir).map_err(storage_error)?;
        let repository = SqliteDocumentRepository::open(&data_dir.join(DATABASE_FILE_NAME))
            .map_err(storage_error)?;
        Ok(Arc::new(Self {
            service: DocumentService::new(repository, files, SystemClock),
        }))
    }

    pub fn create_document(&self, title: String) -> Result<DocumentInfo, NolvaError> {
        Ok(self.service.create_document(&title)?.into())
    }

    pub fn list_documents(&self) -> Result<Vec<DocumentInfo>, NolvaError> {
        Ok(self
            .service
            .list_documents()?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Copies the image at `image_path` into Nolva's storage as the last page
    /// of the document. The source file is left untouched.
    pub fn add_page(
        &self,
        document_id: String,
        image_path: String,
    ) -> Result<PageInfo, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        let page = self.service.add_page(id, Path::new(&image_path))?;
        Ok(self.page_info(page))
    }

    pub fn list_pages(&self, document_id: String) -> Result<Vec<PageInfo>, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        Ok(self
            .service
            .list_pages(id)?
            .into_iter()
            .map(|p| self.page_info(p))
            .collect())
    }
}

impl NolvaCore {
    fn page_info(&self, page: Page) -> PageInfo {
        let absolute = self.service.files().resolve(&page.original_path);
        PageInfo {
            id: page.id.to_string(),
            document_id: page.document_id.to_string(),
            position: page.position,
            original_path: absolute.to_string_lossy().into_owned(),
            created_at_ms: page.created_at_ms,
        }
    }
}

impl From<Document> for DocumentInfo {
    fn from(d: Document) -> Self {
        Self {
            id: d.id.to_string(),
            title: d.title,
            created_at_ms: d.created_at_ms,
            updated_at_ms: d.updated_at_ms,
            page_count: d.page_count,
        }
    }
}

fn storage_error(err: document_engine::PortError) -> NolvaError {
    NolvaError::Storage {
        message: err.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn core(dir: &tempfile::TempDir) -> Arc<NolvaCore> {
        NolvaCore::new(CoreConfig {
            data_dir: dir.path().to_string_lossy().into_owned(),
        })
        .unwrap()
    }

    #[test]
    fn end_to_end_create_add_list() {
        let data = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let img = src_dir.path().join("shot.png");
        std::fs::write(&img, b"png").unwrap();

        let core = core(&data);
        let doc = core.create_document("Contrato".into()).unwrap();
        let page = core
            .add_page(doc.id.clone(), img.to_string_lossy().into_owned())
            .unwrap();

        assert!(Path::new(&page.original_path).is_absolute());
        assert!(Path::new(&page.original_path).exists());
        assert_eq!(core.list_pages(doc.id.clone()).unwrap(), vec![page]);

        // Data survives a restart of the core.
        drop(core);
        let reopened = super::tests::core(&data);
        let docs = reopened.list_documents().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].page_count, 1);
    }

    #[test]
    fn errors_map_to_ffi_variants() {
        let data = tempfile::tempdir().unwrap();
        let core = core(&data);

        assert!(matches!(
            core.create_document("  ".into()),
            Err(NolvaError::InvalidArgument { .. })
        ));
        assert!(matches!(
            core.list_pages("bad-id".into()),
            Err(NolvaError::InvalidArgument { .. })
        ));
        let doc = core.create_document("x".into()).unwrap();
        assert!(matches!(
            core.add_page(doc.id, "/missing/file.jpg".into()),
            Err(NolvaError::NotFound { .. })
        ));
    }

    #[test]
    fn relative_data_dir_is_rejected() {
        let result = NolvaCore::new(CoreConfig {
            data_dir: "relative/dir".into(),
        });
        assert!(matches!(result, Err(NolvaError::InvalidArgument { .. })));
    }
}
