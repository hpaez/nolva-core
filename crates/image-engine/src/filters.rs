//! Page filters: plain grayscale, black and white, and an "enhanced" color
//! mode that removes uneven lighting.
//!
//! Black and white and enhanced both start by estimating the paper: what
//! each point of the page would look like with the ink taken away. Dividing
//! by that estimate turns shadows, gradients and tinted light into flat white
//! paper, which is what makes a phone photo look like a flatbed scan.

use image::imageops::{self, FilterType};
use image::{DynamicImage, GrayImage, Luma, Rgb, RgbImage};
use imageproc::contrast::otsu_level;
use imageproc::filter::gaussian_blur_f32;
use imageproc::morphology::{Mask, grayscale_dilate};

/// A filter applied to a page after cropping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Filter {
    Grayscale,
    /// Pure black ink on white paper.
    BlackAndWhite,
    /// Flat white paper and deeper ink, keeping colors.
    Enhanced,
}

/// Returns `image` with `filter` applied. Grayscale and black and white
/// return single-channel images, which also encode to smaller JPEGs.
pub fn apply_filter(image: &DynamicImage, filter: Filter) -> DynamicImage {
    match filter {
        Filter::Grayscale => DynamicImage::ImageLuma8(image.to_luma8()),
        Filter::BlackAndWhite => DynamicImage::ImageLuma8(black_and_white(&image.to_luma8())),
        Filter::Enhanced => DynamicImage::ImageRgb8(enhanced(&image.to_rgb8())),
    }
}

/// Longest side of the copy the paper is estimated on. Small enough to be
/// cheap on 12 MP photos, large enough to follow shadows and folds.
const PAPER_SIDE: u32 = 256;

/// Radius, in pixels of the small copy, of the window that erases ink. At
/// [`PAPER_SIDE`] a body-text stroke is about one pixel, so 3 also clears
/// headings and bold type.
const INK_RADIUS: u8 = 3;

/// Blur that turns the erased page into a smooth lighting field.
const PAPER_BLUR_SIGMA: f32 = 3.0;

/// In enhanced mode, anything at least this close to the paper's brightness
/// becomes pure white. The paper estimate runs slightly bright inside sharp
/// shadows (the blur borrows from lit paper nearby); without this they
/// would leave a faint gray halo.
const WHITE_POINT: f32 = 0.9;

/// Darkening applied after the white point in enhanced mode: paper stays
/// white while mid-tones such as faded ink move towards black.
const INK_GAMMA: f32 = 1.6;

/// Bounds for the black and white threshold, as a fraction of the paper's
/// brightness scaled to `0..=255`. Otsu alone would split a blank page's
/// paper noise in two; the upper bound keeps blank paper white, the lower
/// one keeps light pencil from vanishing on dense pages.
const MIN_INK_LEVEL: u8 = 140;
const MAX_INK_LEVEL: u8 = 215;

/// Size of the small copy the paper is estimated on.
fn paper_size(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= PAPER_SIDE {
        return (width, height);
    }
    let scale = PAPER_SIDE as f32 / longest as f32;
    let scaled = |side: u32| ((side as f32 * scale).round() as u32).max(1);
    (scaled(width), scaled(height))
}

/// Estimates the paper's brightness at every pixel of the already shrunk
/// `channel`: a local maximum (dilation) erases dark strokes thinner than the
/// window, then a blur smooths what is left into a gentle lighting field.
fn small_paper(channel: &GrayImage) -> GrayImage {
    let erased = grayscale_dilate(channel, &Mask::square(INK_RADIUS));
    gaussian_blur_f32(&erased, PAPER_BLUR_SIGMA)
}

/// Paper brightness at every pixel of `luma`.
fn paper(luma: &GrayImage) -> GrayImage {
    let (width, height) = luma.dimensions();
    let (w, h) = paper_size(width, height);
    let small = imageops::resize(luma, w, h, FilterType::Triangle);
    imageops::resize(&small_paper(&small), width, height, FilterType::Triangle)
}

/// Paper color at every pixel of `rgb`, one estimate per channel so tinted
/// light (warm bulbs, yellowed paper) also ends up neutral white. The photo
/// is shrunk once and the estimate enlarged once, whatever the channels.
fn paper_rgb(rgb: &RgbImage) -> RgbImage {
    let (width, height) = rgb.dimensions();
    let (w, h) = paper_size(width, height);
    let small = imageops::resize(rgb, w, h, FilterType::Triangle);
    let channels: [GrayImage; 3] = std::array::from_fn(|c| {
        let channel = GrayImage::from_raw(w, h, small.pixels().map(|p| p.0[c]).collect())
            .unwrap_or_else(|| GrayImage::new(w, h));
        small_paper(&channel)
    });
    let merged = RgbImage::from_fn(w, h, |x, y| {
        Rgb(std::array::from_fn(|c| channels[c].get_pixel(x, y).0[0]))
    });
    imageops::resize(&merged, width, height, FilterType::Triangle)
}

/// `value` relative to the paper behind it, scaled to `0..=255`.
fn normalize(value: u8, paper: u8) -> u8 {
    let paper = u32::from(paper.max(1));
    (u32::from(value) * 255 / paper).min(255) as u8
}

fn black_and_white(luma: &GrayImage) -> GrayImage {
    let background = paper(luma);
    let mut normalized = luma.clone();
    for (pixel, paper) in normalized.pixels_mut().zip(background.pixels()) {
        pixel.0[0] = normalize(pixel.0[0], paper.0[0]);
    }
    let level = otsu_level(&normalized).clamp(MIN_INK_LEVEL, MAX_INK_LEVEL);
    for pixel in normalized.pixels_mut() {
        *pixel = Luma([if pixel.0[0] <= level { 0 } else { 255 }]);
    }
    normalized
}

fn enhanced(rgb: &RgbImage) -> RgbImage {
    let background = paper_rgb(rgb);

    // out = 255 · min(value / paper / white point, 1)^γ for every
    // (paper, value) pair, computed once instead of per pixel.
    let mut curve = vec![0u8; 256 * 256];
    for paper in 0..256usize {
        for value in 0..256usize {
            let n = (f32::from(normalize(value as u8, paper as u8)) / 255.0 / WHITE_POINT).min(1.0);
            curve[paper * 256 + value] = (255.0 * n.powf(INK_GAMMA)).round() as u8;
        }
    }

    let mut out = rgb.clone();
    for (value, paper) in out.iter_mut().zip(background.iter()) {
        *value = curve[usize::from(*paper) * 256 + usize::from(*value)];
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const W: u32 = 600;
    const H: u32 = 400;

    /// Paper lit from the left: brightness falls from 250 to 110 across the
    /// page. Ink is 35% of the local paper brightness, in horizontal lines.
    fn lit_paper(x: u32) -> f32 {
        250.0 - 140.0 * x as f32 / (W - 1) as f32
    }

    fn is_ink(x: u32, y: u32) -> bool {
        (40..W - 40).contains(&x) && y % 40 < 4 && y > 20
    }

    fn photo(ink: [f32; 3]) -> RgbImage {
        RgbImage::from_fn(W, H, |x, y| {
            let light = lit_paper(x);
            Rgb(if is_ink(x, y) {
                ink.map(|c| (light * c) as u8)
            } else {
                [light as u8; 3]
            })
        })
    }

    #[test]
    fn black_and_white_flattens_shadows() {
        let image = DynamicImage::ImageRgb8(photo([0.35; 3]));
        let out = apply_filter(&image, Filter::BlackAndWhite).to_luma8();

        let (mut wrong_paper, mut wrong_ink) = (0, 0);
        for (x, y, p) in out.enumerate_pixels() {
            match (is_ink(x, y), p.0[0]) {
                (false, 0) => wrong_paper += 1,
                (true, 255) => wrong_ink += 1,
                _ => {}
            }
        }
        // Allow the soft edge of each stroke; the dark side of the page
        // must not turn black.
        assert!(
            wrong_paper < (W * H / 100) as usize,
            "paper turned black: {wrong_paper}"
        );
        assert!(
            wrong_ink < (W * H / 200) as usize,
            "ink turned white: {wrong_ink}"
        );
        assert!(out.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255));
    }

    #[test]
    fn black_and_white_keeps_a_blank_page_white() {
        let blank = GrayImage::from_fn(W, H, |x, y| Luma([235 + ((x * 7 + y * 13) % 9) as u8]));
        let out = apply_filter(&DynamicImage::ImageLuma8(blank), Filter::BlackAndWhite).to_luma8();
        assert!(out.pixels().all(|p| p.0[0] == 255));
    }

    #[test]
    fn enhanced_whitens_paper_and_keeps_ink_color() {
        // Blue ballpoint under warm light: paper is yellowish.
        let warm = RgbImage::from_fn(W, H, |x, y| {
            let light = lit_paper(x);
            let paper = [light, light * 0.95, light * 0.75];
            Rgb(if is_ink(x, y) {
                [paper[0] * 0.25, paper[1] * 0.3, paper[2] * 0.8].map(|c| c as u8)
            } else {
                paper.map(|c| c as u8)
            })
        });
        let out = apply_filter(&DynamicImage::ImageRgb8(warm), Filter::Enhanced).to_rgb8();

        for x in [60, W / 2, W - 60] {
            let paper = out.get_pixel(x, 30).0;
            assert!(paper.iter().all(|&c| c >= 235), "paper at x={x}: {paper:?}");
            let ink = out.get_pixel(x, 41).0;
            assert!(
                ink[2] > ink[0] + 60,
                "ink should stay blue at x={x}: {ink:?}"
            );
            assert!(ink[0] < 100, "ink should be dark at x={x}: {ink:?}");
        }
    }

    #[test]
    fn grayscale_keeps_size_and_drops_color() {
        let image = DynamicImage::ImageRgb8(photo([0.35; 3]));
        let out = apply_filter(&image, Filter::Grayscale);
        assert!(matches!(out, DynamicImage::ImageLuma8(_)));
        assert_eq!((out.width(), out.height()), (W, H));
    }

    #[test]
    fn tiny_images_are_handled() {
        for (w, h) in [(1, 1), (3, 700), (700, 2)] {
            let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, Rgb([200, 190, 180])));
            for filter in [Filter::Grayscale, Filter::BlackAndWhite, Filter::Enhanced] {
                let out = apply_filter(&image, filter);
                assert_eq!((out.width(), out.height()), (w, h));
            }
        }
    }
}
