#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
use image_engine::{
    ImageError, MAX_OUTPUT_SIDE, correct_perspective, load_oriented, output_size, save_jpeg,
};
use imageproc::drawing::draw_polygon_mut;
use imageproc::point::Point as PixelPoint;
use scanner_engine::{Point, Quad};

const RED: Rgb<u8> = Rgb([220, 30, 30]);
const BLUE: Rgb<u8> = Rgb([30, 30, 220]);

fn px(p: Point, w: u32, h: u32) -> PixelPoint<i32> {
    PixelPoint::new(
        (p.x * w as f32).round() as i32,
        (p.y * h as f32).round() as i32,
    )
}

fn midpoint(a: Point, b: Point) -> Point {
    Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)
}

/// A photo of a tilted "page" whose left half is red and right half blue.
fn photo_with_split_page(quad: Quad, w: u32, h: u32) -> DynamicImage {
    let mut image = RgbImage::from_pixel(w, h, Rgb([90, 90, 90]));
    let top_mid = midpoint(quad.top_left, quad.top_right);
    let bottom_mid = midpoint(quad.bottom_left, quad.bottom_right);
    let left = [quad.top_left, top_mid, bottom_mid, quad.bottom_left].map(|p| px(p, w, h));
    let right = [top_mid, quad.top_right, quad.bottom_right, bottom_mid].map(|p| px(p, w, h));
    draw_polygon_mut(&mut image, &left, RED);
    draw_polygon_mut(&mut image, &right, BLUE);
    DynamicImage::ImageRgb8(image)
}

fn is_close(actual: Rgb<u8>, expected: Rgb<u8>) -> bool {
    actual
        .0
        .iter()
        .zip(expected.0)
        .all(|(a, e)| a.abs_diff(e) < 40)
}

#[test]
fn straightens_a_tilted_page_without_mirroring() {
    let quad = Quad {
        top_left: Point::new(0.20, 0.10),
        top_right: Point::new(0.85, 0.18),
        bottom_right: Point::new(0.80, 0.90),
        bottom_left: Point::new(0.12, 0.82),
    };
    let photo = photo_with_split_page(quad, 1000, 1200);

    let page = correct_perspective(&photo, &quad).unwrap().to_rgb8();
    let (w, h) = page.dimensions();

    // Left of the vertical centre line is red, right of it blue, top to bottom.
    for y in [h / 10, h / 2, h * 9 / 10] {
        assert!(
            is_close(*page.get_pixel(w / 4, y), RED),
            "left side at y={y} isn't red"
        );
        assert!(
            is_close(*page.get_pixel(w * 3 / 4, y), BLUE),
            "right side at y={y} isn't blue"
        );
    }
    // No background bleeding in at the corners: the crop is tight.
    for (x, y) in [(3, 3), (w - 4, 3), (w - 4, h - 4), (3, h - 4)] {
        let p = *page.get_pixel(x, y);
        assert!(
            is_close(p, RED) || is_close(p, BLUE),
            "corner ({x},{y}) is {p:?}"
        );
    }
}

#[test]
fn output_keeps_the_page_proportions() {
    let a4_like = Quad {
        top_left: Point::new(0.1, 0.1),
        top_right: Point::new(0.6, 0.1),
        bottom_right: Point::new(0.6, 0.8),
        bottom_left: Point::new(0.1, 0.8),
    };
    // 0.5 × 1000 = 500 px wide, 0.7 × 1000 = 700 px tall.
    assert_eq!(output_size(&a4_like, 1000, 1000), (500, 700));
}

#[test]
fn output_is_capped_for_huge_photos() {
    let (w, h) = output_size(&Quad::FULL, 12_000, 9_000);
    assert_eq!(w, MAX_OUTPUT_SIDE);
    assert_eq!(h, 3072);
}

#[test]
fn rejects_non_convex_corners() {
    let bow_tie = Quad {
        top_left: Point::new(0.1, 0.1),
        top_right: Point::new(0.9, 0.9),
        bottom_right: Point::new(0.9, 0.1),
        bottom_left: Point::new(0.1, 0.9),
    };
    let photo = DynamicImage::new_rgb8(100, 100);
    assert!(matches!(
        correct_perspective(&photo, &bow_tie),
        Err(ImageError::InvalidCorners(_))
    ));
}

#[test]
fn saves_and_reloads_jpeg_without_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("page.jpg");
    let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 48, RED));

    save_jpeg(&image, &path).unwrap();
    let reloaded = load_oriented(&path).unwrap();

    assert_eq!(reloaded.dimensions(), (64, 48));
    assert!(is_close(
        reloaded.to_rgb8().get_pixel(32, 24).to_owned(),
        RED
    ));
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "temp file left behind"
    );
}

/// Builds a JPEG whose EXIF says "rotate 90° clockwise" (orientation 6),
/// which is how many phone cameras store portrait photos.
fn jpeg_with_exif_orientation(width: u32, height: u32, orientation: u8) -> Vec<u8> {
    let mut jpeg = Vec::new();
    JpegEncoder::new(Cursor::new(&mut jpeg))
        .encode_image(&RgbImage::from_pixel(width, height, BLUE))
        .unwrap();

    #[rustfmt::skip]
    let tiff: [u8; 26] = [
        b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // little-endian header, IFD at 8
        0x01, 0x00,                                     // 1 entry
        0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, // tag 0x0112 Orientation, SHORT, count 1
        orientation, 0x00, 0x00, 0x00,                  // value
        0x00, 0x00, 0x00, 0x00,                         // no next IFD
    ];
    let mut app1 = vec![0xFF, 0xE1];
    let length = (2 + 6 + tiff.len()) as u16;
    app1.extend_from_slice(&length.to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);

    // Insert right after the SOI marker (FF D8).
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[test]
fn applies_exif_orientation_like_phone_cameras() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("portrait.jpg");
    // Stored landscape (sensor orientation), meant to be viewed portrait.
    fs::write(&path, jpeg_with_exif_orientation(40, 20, 6)).unwrap();

    let image = load_oriented(&path).unwrap();

    assert_eq!(image.dimensions(), (20, 40));
}

#[test]
fn reports_missing_and_unsupported_files() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        load_oriented(&dir.path().join("missing.jpg")),
        Err(ImageError::NotFound(_))
    ));

    let text = dir.path().join("notes.txt");
    fs::write(&text, "not an image").unwrap();
    assert!(matches!(
        load_oriented(&text),
        Err(ImageError::UnsupportedFormat(_))
    ));
}

#[test]
fn detect_then_correct_end_to_end() {
    // The real pipeline: no corners given, the detector finds them.
    let quad = Quad {
        top_left: Point::new(0.18, 0.12),
        top_right: Point::new(0.84, 0.16),
        bottom_right: Point::new(0.88, 0.86),
        bottom_left: Point::new(0.10, 0.82),
    };
    let photo = photo_with_split_page(quad, 1200, 1600);

    let detected = scanner_engine::detect_document(&photo).expect("page not found");
    let page = correct_perspective(&photo, &detected.quad)
        .unwrap()
        .to_rgb8();
    let (w, h) = page.dimensions();

    assert!(is_close(*page.get_pixel(w / 4, h / 2), RED));
    assert!(is_close(*page.get_pixel(w * 3 / 4, h / 2), BLUE));
}
