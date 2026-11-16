use std::path::PathBuf;

use crate::model::DocumentId;

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

/// Errors produced by document use cases.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("invalid document title: {reason}")]
    InvalidTitle { reason: &'static str },

    #[error("invalid identifier: {value}")]
    InvalidId { value: String },

    #[error("document not found: {0}")]
    DocumentNotFound(DocumentId),

    #[error("source image not found: {}", .0.display())]
    SourceImageNotFound(PathBuf),

    #[error("unsupported image format: {extension}")]
    UnsupportedImageFormat { extension: String },

    #[error(transparent)]
    Storage(#[from] PortError),
}
