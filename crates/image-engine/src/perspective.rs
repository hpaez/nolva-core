use image::{DynamicImage, Rgb, RgbImage};
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use scanner_engine::{Point, Quad};

use crate::ImageError;

/// Longest side of a corrected page. Bounds memory and file size; 4096 px is
/// ~350 dpi for an A4 page, plenty for reading and OCR.
pub const MAX_OUTPUT_SIDE: u32 = 4096;

/// Size of the corrected page for `quad` (normalized) on an image of
/// `width` × `height` pixels.
///
/// Uses the longer of each pair of opposite edges, so the output keeps the
/// page's real proportions instead of the photo's perspective-shrunk ones,
/// then scales down to fit [`MAX_OUTPUT_SIDE`].
pub fn output_size(quad: &Quad, width: u32, height: u32) -> (u32, u32) {
    let px = |p: Point| Point::new(p.x * width as f32, p.y * height as f32);
    let [tl, tr, br, bl] = quad.corners().map(px);
    let out_w = tl.distance(tr).max(bl.distance(br));
    let out_h = tl.distance(bl).max(tr.distance(br));

    let scale = (MAX_OUTPUT_SIDE as f32 / out_w.max(out_h)).min(1.0);
    (
        ((out_w * scale).round() as u32).max(1),
        ((out_h * scale).round() as u32).max(1),
    )
}

/// Warps the region of `image` outlined by `quad` (normalized corners) into
/// an upright rectangle.
pub fn correct_perspective(image: &DynamicImage, quad: &Quad) -> Result<DynamicImage, ImageError> {
    if !quad.is_convex() {
        return Err(ImageError::InvalidCorners(
            "corners must form a convex quadrilateral",
        ));
    }
    let (width, height) = (image.width(), image.height());
    let (out_w, out_h) = output_size(quad, width, height);
    if out_w < 2 || out_h < 2 {
        return Err(ImageError::InvalidCorners("outlined area is too small"));
    }

    let to_px = |p: Point| (p.x * width as f32, p.y * height as f32);
    let source = quad.corners().map(to_px);
    let (w, h) = (out_w as f32, out_h as f32);
    let target = [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)];

    // imageproc's projection maps source coordinates to output coordinates.
    let projection = Projection::from_control_points(source, target)
        .ok_or(ImageError::InvalidCorners("corners are degenerate"))?;

    let rgb = image.to_rgb8();
    let mut out = RgbImage::new(out_w, out_h);
    // Anything outside the photo (corners dragged past its edge) becomes white paper.
    warp_into(
        &rgb,
        projection,
        Interpolation::Bilinear,
        Border::Constant(Rgb([255, 255, 255])),
        &mut out,
    );
    Ok(DynamicImage::ImageRgb8(out))
}
