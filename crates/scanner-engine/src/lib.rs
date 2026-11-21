//! Document detection: finds the four corners of a sheet of paper in a photo.
//!
//! The detector is pure Rust (no OpenCV) and platform-agnostic. It works on
//! an already decoded, correctly oriented image; decoding and EXIF handling
//! live elsewhere.
//!
//! Corners are returned in **normalized coordinates** (`0.0..=1.0` relative
//! to the image width and height), so the apps can overlay them on a preview
//! of any size and the result is independent of the analysis resolution.

mod detect;
mod frame;
mod geometry;

pub use detect::{DetectorConfig, detect_document, detect_document_with};
pub use frame::{FrameError, LIVE_ANALYSIS_SIZE, LumaFrame, detect_document_in_frame};
pub use geometry::{Point, Quad};

/// A detected document outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detection {
    pub quad: Quad,
    /// How sure the detector is, in `0.0..=1.0`. Low values mean the outline
    /// is a rough guess (e.g. a bounding rectangle) and the user should check
    /// the corners before cropping.
    pub confidence: f32,
}
