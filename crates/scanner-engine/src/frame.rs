//! Live detection on camera preview frames.

use image::{DynamicImage, GrayImage, imageops};

use crate::Detection;
use crate::detect::{DetectorConfig, detect_document_with};

/// Analysis size for live frames: about half of the photo setting, which
/// keeps detection to a few milliseconds per frame while still following
/// the sheet's edges closely enough to draw a guide.
pub const LIVE_ANALYSIS_SIZE: u32 = 320;

/// A grayscale camera frame as delivered by camera APIs: the luminance (Y)
/// plane, possibly padded at the end of each row, and the clockwise
/// rotation that makes it upright.
#[derive(Debug, Clone, Copy)]
pub struct LumaFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes from the start of one row to the next; at least `width`.
    pub row_stride: u32,
    pub pixels: &'a [u8],
    /// 0, 90, 180 or 270.
    pub rotation_degrees: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame is empty")]
    Empty,
    #[error("row stride {stride} is smaller than the width {width}")]
    StrideTooSmall { stride: u32, width: u32 },
    #[error("frame needs {needed} bytes but has {actual}")]
    TooShort { needed: usize, actual: usize },
    #[error("rotation must be 0, 90, 180 or 270, got {0}")]
    InvalidRotation(u32),
}

/// Finds the document in a camera frame. Corners are normalized to the
/// **upright** frame (rotation applied), matching what the preview shows.
pub fn detect_document_in_frame(frame: &LumaFrame<'_>) -> Result<Option<Detection>, FrameError> {
    let upright = upright_luma(frame)?;
    let config = DetectorConfig {
        analysis_size: LIVE_ANALYSIS_SIZE,
        ..DetectorConfig::default()
    };
    Ok(detect_document_with(
        &DynamicImage::ImageLuma8(upright),
        &config,
    ))
}

/// Copies the frame's visible pixels (dropping row padding) and rotates them
/// upright.
fn upright_luma(frame: &LumaFrame<'_>) -> Result<GrayImage, FrameError> {
    let LumaFrame {
        width,
        height,
        row_stride,
        pixels,
        rotation_degrees,
    } = *frame;
    if width == 0 || height == 0 {
        return Err(FrameError::Empty);
    }
    if row_stride < width {
        return Err(FrameError::StrideTooSmall {
            stride: row_stride,
            width,
        });
    }
    if !matches!(rotation_degrees, 0 | 90 | 180 | 270) {
        return Err(FrameError::InvalidRotation(rotation_degrees));
    }
    let (w, stride) = (width as usize, row_stride as usize);
    // The last row may be unpadded. Checked: on 32-bit devices absurd
    // dimensions could wrap around and pass the length check.
    let needed = stride
        .checked_mul(height as usize - 1)
        .and_then(|n| n.checked_add(w))
        .unwrap_or(usize::MAX);
    if pixels.len() < needed {
        return Err(FrameError::TooShort {
            needed,
            actual: pixels.len(),
        });
    }

    let mut packed = Vec::with_capacity(w * height as usize);
    for row in pixels.chunks(stride).take(height as usize) {
        packed.extend_from_slice(&row[..w]);
    }
    let image = GrayImage::from_raw(width, height, packed).ok_or(FrameError::TooShort {
        needed,
        actual: pixels.len(),
    })?;
    Ok(match rotation_degrees {
        90 => imageops::rotate90(&image),
        180 => imageops::rotate180(&image),
        270 => imageops::rotate270(&image),
        _ => image,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {

    #[test]
    fn absurd_dimensions_are_too_short_not_a_crash() {
        let pixels = [0u8; 16];
        let frame = LumaFrame {
            width: 4,
            height: u32::MAX,
            row_stride: u32::MAX,
            pixels: &pixels,
            rotation_degrees: 0,
        };
        assert!(matches!(
            detect_document_in_frame(&frame),
            Err(FrameError::TooShort { .. })
        ));
    }

    use super::*;
    use image::Luma;
    use imageproc::drawing::draw_polygon_mut;
    use imageproc::point::Point as PixelPoint;

    /// A 640×480 landscape sensor frame (as phones deliver them) showing a
    /// light sheet whose top edge, once rotated upright, is at the top.
    /// Rows are padded to 704 bytes, like real camera buffers.
    fn sensor_frame() -> (Vec<u8>, u32) {
        let (w, h, stride) = (640u32, 480u32, 704u32);
        let mut img = GrayImage::from_pixel(w, h, Luma([40]));
        // In sensor coordinates (rotated 90° counter-clockwise from upright).
        let corners =
            [(80, 400), (90, 60), (560, 50), (570, 420)].map(|(x, y)| PixelPoint::new(x, y));
        draw_polygon_mut(&mut img, &corners, Luma([220]));
        let mut padded = vec![0u8; (stride * h) as usize];
        for (y, row) in img.rows().enumerate() {
            for (x, p) in row.enumerate() {
                padded[y * stride as usize + x] = p.0[0];
            }
        }
        (padded, stride)
    }

    #[test]
    fn detects_the_sheet_in_upright_coordinates() {
        let (pixels, stride) = sensor_frame();
        let frame = LumaFrame {
            width: 640,
            height: 480,
            row_stride: stride,
            pixels: &pixels,
            rotation_degrees: 90,
        };

        let detection = detect_document_in_frame(&frame)
            .unwrap()
            .expect("sheet not found");
        assert!(
            detection.confidence > 0.8,
            "confidence {}",
            detection.confidence
        );
        // Upright frame is 480×640; sensor (80,400) → upright (480-1-400, 80).
        let tl = detection.quad.top_left;
        assert!(
            (tl.x - 80.0 / 480.0).abs() < 0.03 && (tl.y - 80.0 / 640.0).abs() < 0.03,
            "{tl:?}"
        );
        let br = detection.quad.bottom_right;
        assert!(
            (br.x - 430.0 / 480.0).abs() < 0.03 && (br.y - 570.0 / 640.0).abs() < 0.03,
            "{br:?}"
        );
    }

    #[test]
    fn empty_scenes_have_no_detection() {
        let pixels = vec![90u8; 320 * 240];
        let frame = LumaFrame {
            width: 320,
            height: 240,
            row_stride: 320,
            pixels: &pixels,
            rotation_degrees: 0,
        };
        assert_eq!(detect_document_in_frame(&frame).unwrap(), None);
    }

    #[test]
    fn rejects_malformed_frames() {
        let pixels = vec![0u8; 100];
        let frame = |w, h, stride, rotation| LumaFrame {
            width: w,
            height: h,
            row_stride: stride,
            pixels: &pixels,
            rotation_degrees: rotation,
        };
        assert_eq!(
            detect_document_in_frame(&frame(0, 10, 10, 0)),
            Err(FrameError::Empty)
        );
        assert_eq!(
            detect_document_in_frame(&frame(10, 10, 8, 0)),
            Err(FrameError::StrideTooSmall {
                stride: 8,
                width: 10
            })
        );
        assert_eq!(
            detect_document_in_frame(&frame(10, 11, 10, 0)),
            Err(FrameError::TooShort {
                needed: 110,
                actual: 100
            })
        );
        assert_eq!(
            detect_document_in_frame(&frame(10, 10, 10, 45)),
            Err(FrameError::InvalidRotation(45))
        );
        // An unpadded last row is fine.
        assert!(detect_document_in_frame(&frame(9, 10, 10, 0)).is_ok());
    }
}
