//! Image I/O and processing for Nolva.
//!
//! - [`load_oriented`]: decode a photo and apply its EXIF orientation, so
//!   pixels match what the user sees (and what corner coordinates refer to).
//! - [`correct_perspective`]: turn the quadrilateral outlining a page into
//!   an upright rectangle, like a flatbed scan.
//! - [`apply_filter`]: grayscale, black and white, or enhanced color.
//! - [`save_jpeg`]: encode atomically, never leaving a half-written file.

mod filters;
mod io;
mod perspective;
mod upright;

pub use filters::{Filter, apply_filter};
pub use io::{JPEG_QUALITY, load_oriented, save_jpeg};
pub use perspective::{MAX_OUTPUT_SIDE, correct_perspective, output_size};
pub use upright::{UprightJpeg, upright_jpeg};

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("image not found: {}", .0.display())]
    NotFound(PathBuf),

    #[error("unsupported image format: {}", .0.display())]
    UnsupportedFormat(PathBuf),

    #[error("invalid corners: {0}")]
    InvalidCorners(&'static str),

    #[error("could not decode image: {0}")]
    Decode(#[source] image::ImageError),

    #[error("could not encode image: {0}")]
    Encode(#[source] image::ImageError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
