use std::path::Path;

use crate::clock::Clock;
use crate::error::DocumentError;
use crate::model::{Document, DocumentId, ImageFormat, NewPage, Page, PageId};
use crate::ports::{DocumentRepository, PageFileStore};

/// Maximum length of a document title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;

/// Document use cases, independent of storage technology and platform.
#[derive(Debug)]
pub struct DocumentService<R, F, C> {
    repository: R,
    files: F,
    clock: C,
}

impl<R, F, C> DocumentService<R, F, C>
where
    R: DocumentRepository,
    F: PageFileStore,
    C: Clock,
{
    pub fn new(repository: R, files: F, clock: C) -> Self {
        Self {
            repository,
            files,
            clock,
        }
    }

    pub fn files(&self) -> &F {
        &self.files
    }

    pub fn create_document(&self, title: &str) -> Result<Document, DocumentError> {
        let title = normalize_title(title)?;
        let now = self.clock.now_ms();
        let document = Document {
            id: DocumentId::new(),
            title,
            created_at_ms: now,
            updated_at_ms: now,
            page_count: 0,
        };
        self.repository.insert_document(&document)?;
        Ok(document)
    }

    pub fn list_documents(&self) -> Result<Vec<Document>, DocumentError> {
        Ok(self.repository.list_documents()?)
    }

    /// Imports an image file as the last page of a document.
    ///
    /// The source file is copied, never moved: the caller keeps ownership of
    /// it (it may be a camera temp file or a photo-library export).
    pub fn add_page(&self, document_id: DocumentId, source: &Path) -> Result<Page, DocumentError> {
        if self.repository.get_document(document_id)?.is_none() {
            return Err(DocumentError::DocumentNotFound(document_id));
        }

        let format = ImageFormat::from_path(source)?;
        if !source.is_file() {
            return Err(DocumentError::SourceImageNotFound(source.to_path_buf()));
        }

        let page_id = PageId::new();
        let original_path = self
            .files
            .import_original(document_id, page_id, format, source)?;

        let new_page = NewPage {
            id: page_id,
            document_id,
            original_path,
            created_at_ms: self.clock.now_ms(),
        };

        self.repository.append_page(&new_page).map_err(|err| {
            // Compensate so we don't leave orphaned files behind. If cleanup
            // also fails, a future orphan sweep catches it; the original
            // error is the one worth reporting.
            let _ = self.files.remove_page_files(document_id, page_id);
            err.into()
        })
    }

    pub fn list_pages(&self, document_id: DocumentId) -> Result<Vec<Page>, DocumentError> {
        if self.repository.get_document(document_id)?.is_none() {
            return Err(DocumentError::DocumentNotFound(document_id));
        }
        Ok(self.repository.list_pages(document_id)?)
    }
}

fn normalize_title(title: &str) -> Result<String, DocumentError> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(DocumentError::InvalidTitle {
            reason: "title is empty",
        });
    }
    if trimmed.chars().count() > MAX_TITLE_CHARS {
        return Err(DocumentError::InvalidTitle {
            reason: "title is too long",
        });
    }
    if trimmed.chars().any(char::is_control) {
        return Err(DocumentError::InvalidTitle {
            reason: "title contains control characters",
        });
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::error::PortError;

    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    #[derive(Default)]
    struct InMemoryRepo {
        docs: Mutex<HashMap<DocumentId, Document>>,
        pages: Mutex<Vec<Page>>,
        fail_append: bool,
    }

    impl DocumentRepository for InMemoryRepo {
        fn insert_document(&self, d: &Document) -> Result<(), PortError> {
            self.docs.lock().unwrap().insert(d.id, d.clone());
            Ok(())
        }
        fn get_document(&self, id: DocumentId) -> Result<Option<Document>, PortError> {
            Ok(self.docs.lock().unwrap().get(&id).cloned())
        }
        fn list_documents(&self) -> Result<Vec<Document>, PortError> {
            Ok(self.docs.lock().unwrap().values().cloned().collect())
        }
        fn append_page(&self, p: &NewPage) -> Result<Page, PortError> {
            if self.fail_append {
                return Err(PortError::new("append_page", "simulated failure"));
            }
            let mut docs = self.docs.lock().unwrap();
            let d = docs.get_mut(&p.document_id).unwrap();
            let page = Page {
                id: p.id,
                document_id: p.document_id,
                position: d.page_count,
                original_path: p.original_path.clone(),
                created_at_ms: p.created_at_ms,
            };
            d.page_count += 1;
            d.updated_at_ms = p.created_at_ms;
            self.pages.lock().unwrap().push(page.clone());
            Ok(page)
        }
        fn list_pages(&self, id: DocumentId) -> Result<Vec<Page>, PortError> {
            Ok(self
                .pages
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.document_id == id)
                .cloned()
                .collect())
        }
    }

    #[derive(Default)]
    struct FakeFiles {
        removed: Mutex<Vec<PageId>>,
    }

    impl PageFileStore for FakeFiles {
        fn import_original(
            &self,
            d: DocumentId,
            p: PageId,
            f: ImageFormat,
            _source: &Path,
        ) -> Result<PathBuf, PortError> {
            Ok(PathBuf::from(format!("{d}/{p}.{}", f.extension())))
        }
        fn remove_page_files(&self, _d: DocumentId, p: PageId) -> Result<(), PortError> {
            self.removed.lock().unwrap().push(p);
            Ok(())
        }
        fn resolve(&self, relative: &Path) -> PathBuf {
            relative.to_path_buf()
        }
    }

    fn service(repo: InMemoryRepo) -> DocumentService<InMemoryRepo, FakeFiles, FixedClock> {
        DocumentService::new(repo, FakeFiles::default(), FixedClock(1_000))
    }

    fn temp_image(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, b"fake").unwrap();
        (dir, path)
    }

    #[test]
    fn create_document_trims_title_and_sets_timestamps() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("  Factura luz  ").unwrap();
        assert_eq!(doc.title, "Factura luz");
        assert_eq!(doc.created_at_ms, 1_000);
        assert_eq!(doc.page_count, 0);
    }

    #[test]
    fn create_document_rejects_bad_titles() {
        let svc = service(InMemoryRepo::default());
        assert!(svc.create_document("   ").is_err());
        assert!(svc.create_document("a\u{0007}b").is_err());
        assert!(
            svc.create_document(&"x".repeat(MAX_TITLE_CHARS + 1))
                .is_err()
        );
        assert!(svc.create_document(&"á".repeat(MAX_TITLE_CHARS)).is_ok());
    }

    #[test]
    fn add_page_appends_in_order() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.JPG");

        let p0 = svc.add_page(doc.id, &img).unwrap();
        let p1 = svc.add_page(doc.id, &img).unwrap();

        assert_eq!((p0.position, p1.position), (0, 1));
        assert!(p0.original_path.to_string_lossy().ends_with(".jpg"));
        assert_eq!(svc.list_pages(doc.id).unwrap().len(), 2);
    }

    #[test]
    fn add_page_validates_inputs() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.png");

        assert!(matches!(
            svc.add_page(DocumentId::new(), &img),
            Err(DocumentError::DocumentNotFound(_))
        ));
        assert!(matches!(
            svc.add_page(doc.id, Path::new("/nope/missing.png")),
            Err(DocumentError::SourceImageNotFound(_))
        ));
        let (_d2, pdf) = temp_image("file.pdf");
        assert!(matches!(
            svc.add_page(doc.id, &pdf),
            Err(DocumentError::UnsupportedImageFormat { .. })
        ));
    }

    #[test]
    fn add_page_cleans_up_files_when_persistence_fails() {
        let repo = InMemoryRepo {
            fail_append: true,
            ..Default::default()
        };
        let svc = service(repo);
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.png");

        assert!(svc.add_page(doc.id, &img).is_err());
        assert_eq!(svc.files().removed.lock().unwrap().len(), 1);
    }
}
