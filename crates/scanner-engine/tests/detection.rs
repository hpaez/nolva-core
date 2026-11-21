//! End-to-end detection on synthetic photos: a light sheet with known
//! corners on a darker, noisy background.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use image::{DynamicImage, GrayImage, Luma};
use imageproc::drawing::draw_polygon_mut;
use imageproc::point::Point as PixelPoint;
use scanner_engine::{Point, Quad, detect_document};

const W: u32 = 1200;
const H: u32 = 1600;

/// Deterministic pseudo-random noise (no dev-dependency on `rand`).
fn add_noise(image: &mut GrayImage, amplitude: i16, seed: u32) {
    let mut state = seed;
    for pixel in image.pixels_mut() {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let noise = ((state >> 16) % (2 * amplitude as u32 + 1)) as i16 - amplitude;
        pixel.0[0] = (i16::from(pixel.0[0]) + noise).clamp(0, 255) as u8;
    }
}

/// Renders a photo-like image of a sheet with the given normalized corners.
fn photo_of_sheet(quad: Quad, background: u8, paper: u8) -> DynamicImage {
    let mut image = GrayImage::from_pixel(W, H, Luma([background]));
    let pixels: Vec<PixelPoint<i32>> = quad
        .corners()
        .iter()
        .map(|p| PixelPoint::new((p.x * W as f32) as i32, (p.y * H as f32) as i32))
        .collect();
    draw_polygon_mut(&mut image, &pixels, Luma([paper]));
    add_noise(&mut image, 12, 7);
    DynamicImage::ImageLuma8(image)
}

fn assert_close(found: Quad, expected: Quad, tolerance: f32) {
    for (f, e) in found.corners().iter().zip(expected.corners()) {
        assert!(
            f.distance(e) <= tolerance,
            "corner {f:?} is {:.3} away from expected {e:?}\nfound:    {found:?}\nexpected: {expected:?}",
            f.distance(e),
        );
    }
}

#[test]
fn finds_an_upright_sheet() {
    let expected = Quad {
        top_left: Point::new(0.15, 0.12),
        top_right: Point::new(0.85, 0.12),
        bottom_right: Point::new(0.85, 0.88),
        bottom_left: Point::new(0.15, 0.88),
    };

    let detection = detect_document(&photo_of_sheet(expected, 60, 225)).expect("sheet not found");

    assert_close(detection.quad, expected, 0.02);
    assert!(
        detection.confidence > 0.8,
        "confidence {}",
        detection.confidence
    );
}

#[test]
fn finds_a_sheet_photographed_at_an_angle() {
    // Perspective + rotation, like a hand-held shot from above and to the side.
    let expected = Quad {
        top_left: Point::new(0.22, 0.10),
        top_right: Point::new(0.83, 0.17),
        bottom_right: Point::new(0.90, 0.85),
        bottom_left: Point::new(0.08, 0.80),
    };

    let detection = detect_document(&photo_of_sheet(expected, 50, 215)).expect("sheet not found");

    assert_close(detection.quad, expected, 0.02);
}

#[test]
fn finds_a_low_contrast_sheet() {
    // White paper on a light desk: harder, but should still work.
    let expected = Quad {
        top_left: Point::new(0.2, 0.2),
        top_right: Point::new(0.8, 0.22),
        bottom_right: Point::new(0.78, 0.8),
        bottom_left: Point::new(0.18, 0.78),
    };

    let detection = detect_document(&photo_of_sheet(expected, 150, 215)).expect("sheet not found");

    assert_close(detection.quad, expected, 0.03);
}

#[test]
fn ignores_printed_text_inside_the_sheet() {
    let expected = Quad {
        top_left: Point::new(0.12, 0.10),
        top_right: Point::new(0.88, 0.13),
        bottom_right: Point::new(0.86, 0.90),
        bottom_left: Point::new(0.10, 0.87),
    };
    let mut photo = photo_of_sheet(expected, 55, 220).to_luma8();
    // Lines of "text": dark bars that create many strong inner edges.
    for line in 0..18 {
        let y = (0.2 * H as f32) as u32 + line * 60;
        for x in (0.22 * W as f32) as u32..(0.75 * W as f32) as u32 {
            for dy in 0..14 {
                photo.put_pixel(x, y + dy, Luma([30]));
            }
        }
    }

    let detection = detect_document(&DynamicImage::ImageLuma8(photo)).expect("sheet not found");

    assert_close(detection.quad, expected, 0.02);
}

#[test]
fn is_fast_enough_for_a_full_resolution_photo() {
    // 12 MP, like a phone's main camera. Detection must stay interactive
    // (well under a second) once compiled with optimizations.
    let quad = Quad {
        top_left: Point::new(0.15, 0.12),
        top_right: Point::new(0.85, 0.12),
        bottom_right: Point::new(0.85, 0.88),
        bottom_left: Point::new(0.15, 0.88),
    };
    let mut photo = GrayImage::from_pixel(3000, 4000, Luma([60]));
    let pixels: Vec<PixelPoint<i32>> = quad
        .corners()
        .iter()
        .map(|p| PixelPoint::new((p.x * 3000.0) as i32, (p.y * 4000.0) as i32))
        .collect();
    draw_polygon_mut(&mut photo, &pixels, Luma([225]));
    let photo = DynamicImage::ImageLuma8(photo);

    let start = std::time::Instant::now();
    let detection = detect_document(&photo);
    let elapsed = start.elapsed();

    assert!(detection.is_some());
    eprintln!("12 MP detection took {elapsed:?}");
    if !cfg!(debug_assertions) {
        assert!(elapsed.as_millis() < 1000, "too slow: {elapsed:?}");
    }
}

#[test]
fn returns_none_without_a_document() {
    let mut blank = GrayImage::from_pixel(W, H, Luma([120]));
    add_noise(&mut blank, 12, 3);

    assert_eq!(detect_document(&DynamicImage::ImageLuma8(blank)), None);
}

#[test]
fn ignores_objects_too_small_to_be_the_page() {
    let tiny = Quad {
        top_left: Point::new(0.45, 0.45),
        top_right: Point::new(0.55, 0.45),
        bottom_right: Point::new(0.55, 0.55),
        bottom_left: Point::new(0.45, 0.55),
    };

    assert_eq!(detect_document(&photo_of_sheet(tiny, 60, 225)), None);
}

#[test]
fn rejects_degenerate_images() {
    let tiny = DynamicImage::ImageLuma8(GrayImage::new(4, 4));
    assert_eq!(detect_document(&tiny), None);
}
