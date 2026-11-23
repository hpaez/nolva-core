use std::path::PathBuf;

use crate::model::{DocumentId, PageId};

/// Failure reported by a port implementation (database, file system, ...).
///
/// The domain does not inspect the cause; it only propagates it.
#[derive(Debug, thiserror::Error)]
#[error("{context}: {source}")]
pub struct PortError {
    context: &'static str,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl PortError {
    pub fn new(
        context: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            context,
            source: source.into(),
        }
    }
}

/// Failure reported by a [`crate::PageProcessor`].
#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    #[error("unsupported image format")]
    UnsupportedFormat,

    #[error("invalid corners: {0}")]
    InvalidCorners(String),

    #[error(transparent)]
    Failed(#[from] PortError),
}

/// Errors produced by document use cases.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("invalid document title: {reason}")]
    InvalidTitle { reason: &'static str },

    #[error("invalid identifier: {value}")]
    InvalidId { value: String },

    #[error("document not found: {0}")]
    DocumentNotFound(DocumentId),

    #[error("page not found: {0}")]
    PageNotFound(PageId),

    #[error("document has no pages: {0}")]
    EmptyDocument(DocumentId),

    #[error("invalid corners: {reason}")]
    InvalidCorners { reason: String },

    #[error("source image not found: {}", .0.display())]
    SourceImageNotFound(PathBuf),

    #[error("invalid recognized text: {reason}")]
    InvalidText { reason: String },

    /// The page's image changed while its text was being recognized, so the
    /// result no longer applies. The page shows up again in
    /// `pages_needing_text`.
    #[error("page image changed during recognition: {0}")]
    PageImageChanged(PageId),

    #[error("unsupported image format: {extension}")]
    UnsupportedImageFormat { extension: String },

    #[error(transparent)]
    Storage(#[from] PortError),
}
