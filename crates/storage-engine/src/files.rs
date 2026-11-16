use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use document_engine::{DocumentId, ImageFormat, PageFileStore, PageId, PortError};

/// [`PageFileStore`] on the local file system, rooted at the app's data dir.
///
/// Every path it builds comes from UUIDs and a fixed extension allowlist, so
/// caller input can never steer writes outside the root (no path traversal).
#[derive(Debug, Clone)]
pub struct LocalPageFileStore {
    root: PathBuf,
}

impl LocalPageFileStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, PortError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| PortError::new("create storage root", e))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn page_dir_relative(document_id: DocumentId, page_id: PageId) -> PathBuf {
        PathBuf::from("documents")
            .join(document_id.to_string())
            .join("pages")
            .join(page_id.to_string())
    }
}

impl PageFileStore for LocalPageFileStore {
    fn import_original(
        &self,
        document_id: DocumentId,
        page_id: PageId,
        format: ImageFormat,
        source: &Path,
    ) -> Result<PathBuf, PortError> {
        let dir_rel = Self::page_dir_relative(document_id, page_id);
        let dir_abs = self.root.join(&dir_rel);
        fs::create_dir_all(&dir_abs).map_err(|e| PortError::new("create page directory", e))?;

        let file_name = format!("original.{}", format.extension());
        let final_abs = dir_abs.join(&file_name);
        let partial_abs = dir_abs.join(format!("{file_name}.partial"));

        // Copy to a temporary name then rename, so a crash mid-copy never
        // leaves a truncated file under the final name.
        let result = fs::copy(source, &partial_abs)
            .and_then(|_| fs::File::open(&partial_abs)?.sync_all())
            .and_then(|()| fs::rename(&partial_abs, &final_abs));
        if let Err(e) = result {
            let _ = fs::remove_file(&partial_abs);
            return Err(PortError::new("import page image", e));
        }

        Ok(dir_rel.join(file_name))
    }

    fn remove_page_files(&self, document_id: DocumentId, page_id: PageId) -> Result<(), PortError> {
        let dir = self
            .root
            .join(Self::page_dir_relative(document_id, page_id));
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PortError::new("remove page files", e)),
        }
    }

    fn resolve(&self, relative: &Path) -> PathBuf {
        self.root.join(relative)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn imports_copy_and_removes_page_files() {
        let root = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("cam.jpeg");
        fs::write(&src, b"pixels").unwrap();

        let store = LocalPageFileStore::new(root.path()).unwrap();
        let (d, p) = (DocumentId::new(), PageId::new());
        let rel = store
            .import_original(d, p, ImageFormat::Jpeg, &src)
            .unwrap();

        assert!(rel.is_relative());
        assert!(rel.ends_with("original.jpg"));
        assert_eq!(fs::read(store.resolve(&rel)).unwrap(), b"pixels");
        assert!(src.exists(), "source must be copied, not moved");

        store.remove_page_files(d, p).unwrap();
        assert!(!store.resolve(&rel).exists());
        // Idempotent.
        store.remove_page_files(d, p).unwrap();
    }

    #[test]
    fn failed_import_leaves_no_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalPageFileStore::new(root.path()).unwrap();
        let (d, p) = (DocumentId::new(), PageId::new());

        let missing = root.path().join("missing.png");
        assert!(
            store
                .import_original(d, p, ImageFormat::Png, &missing)
                .is_err()
        );

        let page_dir = root
            .path()
            .join(LocalPageFileStore::page_dir_relative(d, p));
        assert_eq!(fs::read_dir(page_dir).unwrap().count(), 0);
    }
}
