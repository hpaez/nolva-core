use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use uuid::Uuid;

use crate::error::DocumentError;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new random identifier.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.as_hyphenated().fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = DocumentError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|_| DocumentError::InvalidId { value: s.to_owned() })
            }
        }
    };
}

id_type!(
    /// Stable identifier of a document.
    DocumentId
);
id_type!(
    /// Stable identifier of a page.
    PageId
);

id_type!(
    /// Identifier of a folder.
    FolderId
);

id_type!(
    /// Identifier of a saved signature.
    SignatureId
);

/// A scanned document: an ordered collection of pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub id: DocumentId,
    pub title: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub page_count: u32,
    /// Image of the first page (processed if it has one), relative to the
    /// storage root, for list thumbnails. `None` for empty documents. Derived
    /// by the repository on read; ignored on insert.
    pub cover_path: Option<PathBuf>,
    /// When the document was moved to the trash; `None` while it isn't.
    pub trashed_at_ms: Option<i64>,
    /// Folder it's in; `None` for the top level.
    pub folder_id: Option<FolderId>,
    /// Bytes its page images take (originals and processed). Filled in by
    /// the service when listing; 0 elsewhere.
    pub size_bytes: u64,
}

/// A folder of documents. Folders don't nest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub name: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Documents in it, not counting trashed ones. Derived on read.
    pub document_count: u32,
}

/// Page outline chosen for cropping: four corners in normalized coordinates
/// (`0.0..=1.0`) of the oriented original, clockwise from the top left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    corners: [(f32, f32); 4],
}

impl Crop {
    /// Validates that every coordinate is a finite number inside the image.
    /// Shape checks (convexity, size) belong to the image processor.
    pub fn new(corners: [(f32, f32); 4]) -> Result<Self, DocumentError> {
        let valid = corners.iter().all(|&(x, y)| {
            x.is_finite() && y.is_finite() && (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)
        });
        if valid {
            Ok(Self { corners })
        } else {
            Err(DocumentError::InvalidCorners {
                reason: "coordinates must be within 0.0..=1.0".into(),
            })
        }
    }

    /// Top-left, top-right, bottom-right, bottom-left.
    pub fn corners(&self) -> [(f32, f32); 4] {
        self.corners
    }
}

/// A single page of a document.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub id: PageId,
    pub document_id: DocumentId,
    /// Zero-based position within the document.
    pub position: u32,
    /// Path of the untouched original image, relative to the storage root.
    ///
    /// Relative on purpose: the absolute app container path changes between
    /// installs and OS updates (notably on iOS).
    pub original_path: PathBuf,
    /// Outline used for the processed image, if the page was cropped.
    pub crop: Option<Crop>,
    /// Filter applied to the processed image.
    pub filter: PageFilter,
    /// Turn applied after the crop.
    pub rotation: Rotation,
    /// Cropped, filtered and/or rotated image, relative to the storage root.
    /// `None` while the page has none of them; the original is never
    /// modified.
    pub processed_path: Option<PathBuf>,
    pub created_at_ms: i64,
}

/// Clockwise turn of a page, applied after its crop. Non-destructive like
/// the crop and filter: the original is never rotated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Rotation {
    #[default]
    None,
    Clockwise90,
    Clockwise180,
    Clockwise270,
}

impl Rotation {
    pub fn degrees(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Clockwise90 => 90,
            Self::Clockwise180 => 180,
            Self::Clockwise270 => 270,
        }
    }

    /// Accepts 0, 90, 180 or 270.
    pub fn from_degrees(degrees: u32) -> Result<Self, DocumentError> {
        match degrees {
            0 => Ok(Self::None),
            90 => Ok(Self::Clockwise90),
            180 => Ok(Self::Clockwise180),
            270 => Ok(Self::Clockwise270),
            _ => Err(DocumentError::InvalidRotation { degrees }),
        }
    }

    /// This rotation turned a further quarter clockwise (or counterclockwise).
    pub fn turned(self, clockwise: bool) -> Self {
        let step = if clockwise { 90 } else { 270 };
        // Always a multiple of 90 below 360, so this can't fail.
        Self::from_degrees((self.degrees() + step) % 360).unwrap_or_default()
    }
}

/// Look applied to a page on top of its crop. Filters are re-applied from the
/// original every time, so switching between them never degrades the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PageFilter {
    /// Colors as photographed.
    #[default]
    Original,
    Grayscale,
    /// Pure black ink on white paper: smallest files, best for text.
    BlackAndWhite,
    /// Evens out lighting and shadows, whitens the paper and deepens the
    /// ink while keeping colors.
    Enhanced,
}

/// A page about to be appended; the repository assigns its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPage {
    pub id: PageId,
    pub document_id: DocumentId,
    pub original_path: PathBuf,
    pub created_at_ms: i64,
}

/// Image formats accepted as page originals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Heic,
    Webp,
}

impl ImageFormat {
    /// Detects the format from the file extension (case-insensitive).
    pub fn from_path(path: &Path) -> Result<Self, DocumentError> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        match ext.as_str() {
            "jpg" | "jpeg" => Ok(Self::Jpeg),
            "png" => Ok(Self::Png),
            "heic" | "heif" => Ok(Self::Heic),
            "webp" => Ok(Self::Webp),
            _ => Err(DocumentError::UnsupportedImageFormat { extension: ext }),
        }
    }

    /// Canonical file extension used when storing the image.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Heic => "heic",
            Self::Webp => "webp",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_strings() {
        let id = DocumentId::new();
        let parsed: DocumentId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn invalid_id_is_rejected() {
        let err = "not-a-uuid".parse::<PageId>().unwrap_err();
        assert!(matches!(err, DocumentError::InvalidId { .. }));
    }

    #[test]
    fn image_format_detection_is_case_insensitive() {
        assert_eq!(
            ImageFormat::from_path(Path::new("a/B.JPEG")).unwrap(),
            ImageFormat::Jpeg
        );
        assert_eq!(
            ImageFormat::from_path(Path::new("x.HeIf")).unwrap(),
            ImageFormat::Heic
        );
    }

    #[test]
    fn unknown_or_missing_extension_is_rejected() {
        assert!(ImageFormat::from_path(Path::new("doc.pdf")).is_err());
        assert!(ImageFormat::from_path(Path::new("noext")).is_err());
    }
}
