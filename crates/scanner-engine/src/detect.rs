use image::imageops::{self, FilterType};
use image::{DynamicImage, GrayImage};
use imageproc::contours::{BorderType, find_contours};
use imageproc::distance_transform::Norm;
use imageproc::edges::canny;
use imageproc::filter::gaussian_blur_f32;
use imageproc::geometry::{approximate_polygon_dp, arc_length, convex_hull, min_area_rect};
use imageproc::morphology::dilate;
use imageproc::point::Point as PixelPoint;

use crate::Detection;
use crate::geometry::{Point, Quad};

/// Tuning knobs for [`detect_document_with`]. The defaults are tuned for
/// phone photos of paper documents.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectorConfig {
    /// Longest side, in pixels, of the downscaled copy that is analysed.
    /// Detection doesn't need full resolution and this bounds CPU time.
    pub analysis_size: u32,
    /// Smallest document accepted, as a fraction of the image area.
    pub min_area_ratio: f32,
    /// Gaussian blur applied before edge detection, to ignore paper texture
    /// and small print.
    pub blur_sigma: f32,
    /// Polygon simplification tolerance, as a fraction of the outline's perimeter.
    pub approx_epsilon: f64,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            analysis_size: 640,
            min_area_ratio: 0.15,
            blur_sigma: 2.0,
            approx_epsilon: 0.02,
        }
    }
}

/// Finds the document in `image` with the default configuration.
pub fn detect_document(image: &DynamicImage) -> Option<Detection> {
    detect_document_with(image, &DetectorConfig::default())
}

/// Finds the largest plausible document outline in `image`.
///
/// Returns `None` when nothing document-like is found; callers should then
/// fall back to [`Quad::FULL`] and let the user adjust the corners.
///
/// Pipeline: downscale → grayscale → blur → Canny edges → dilate (close
/// small gaps in the outline) → external contours → convex hull →
/// simplify to a polygon. A clean 4-sided polygon is a confident match; any
/// other large shape falls back to its minimum-area rectangle with lower
/// confidence.
pub fn detect_document_with(image: &DynamicImage, config: &DetectorConfig) -> Option<Detection> {
    let (width, height) = (image.width(), image.height());
    if width < 8 || height < 8 {
        return None;
    }

    let gray = downscale(&image.to_luma8(), config.analysis_size);
    let (w, h) = (gray.width() as f32, gray.height() as f32);

    let blurred = gaussian_blur_f32(&gray, config.blur_sigma);
    let (low, high) = canny_thresholds(&blurred);
    let edges = dilate(&canny(&blurred, low, high), Norm::LInf, 2);

    find_contours::<i32>(&edges)
        .into_iter()
        .filter(|c| c.border_type == BorderType::Outer && c.points.len() >= 4)
        .filter_map(|contour| {
            let hull = convex_hull(contour.points);
            let hull_area = polygon_area(&hull) / (w * h);
            if hull_area < config.min_area_ratio {
                return None;
            }

            let epsilon = config.approx_epsilon * arc_length(&hull, true);
            let polygon = approximate_polygon_dp(&hull, epsilon, true);
            let (corners, base_confidence) = match polygon.as_slice() {
                [a, b, c, d] => ([*a, *b, *c, *d], 1.0),
                _ => (min_area_rect(&hull), 0.5),
            };

            let quad = Quad::from_unordered(corners.map(|p| normalize(p, w, h)));
            if !quad.is_convex() {
                return None;
            }
            let area = quad.area();
            // Bigger outlines are more likely to be the page rather than
            // something printed on it.
            let confidence = (base_confidence * (area / 0.5).min(1.0)).clamp(0.0, 1.0);
            Some((area, Detection { quad, confidence }))
        })
        .max_by(|(a, _), (b, _)| a.total_cmp(b))
        .map(|(_, detection)| detection)
}

fn downscale(gray: &GrayImage, max_side: u32) -> GrayImage {
    let (w, h) = gray.dimensions();
    let longest = w.max(h);
    if longest <= max_side {
        return gray.clone();
    }
    let scale = max_side as f32 / longest as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    imageops::resize(gray, nw, nh, FilterType::Triangle)
}

/// Canny thresholds derived from the image's own gradient distribution.
///
/// Brightness-based thresholds confuse "bright" with "sharp" and miss white
/// paper on a light desk. Instead the high threshold follows the 90th
/// percentile of the Sobel gradient magnitude, bounded on both sides:
/// - the floor keeps flat, noisy images (no document) from producing edges;
/// - the ceiling stops dense high-contrast print (text, tables) from pushing
///   the threshold above the page outline, which is usually softer than the
///   ink on it.
const MIN_HIGH_THRESHOLD: f32 = 40.0;
const MAX_HIGH_THRESHOLD: f32 = 100.0;

fn canny_thresholds(gray: &GrayImage) -> (f32, f32) {
    let gradients = imageproc::gradients::sobel_gradients(gray);
    let mut magnitudes: Vec<u16> = gradients.pixels().map(|p| p.0[0]).collect();
    if magnitudes.is_empty() {
        return (20.0, 50.0);
    }
    let index = magnitudes.len() * 9 / 10;
    let (_, p90, _) = magnitudes.select_nth_unstable(index);
    let high = f32::from(*p90).clamp(MIN_HIGH_THRESHOLD, MAX_HIGH_THRESHOLD);
    (high * 0.4, high)
}

fn normalize(p: PixelPoint<i32>, w: f32, h: f32) -> Point {
    Point::new(
        (p.x as f32 / w).clamp(0.0, 1.0),
        (p.y as f32 / h).clamp(0.0, 1.0),
    )
}

fn polygon_area(points: &[PixelPoint<i32>]) -> f32 {
    if points.len() < 3 {
        return 0.0;
    }
    let twice: i64 = (0..points.len())
        .map(|i| {
            let (a, b) = (points[i], points[(i + 1) % points.len()]);
            i64::from(a.x) * i64::from(b.y) - i64::from(b.x) * i64::from(a.y)
        })
        .sum();
    (twice.abs() as f32) / 2.0
}
