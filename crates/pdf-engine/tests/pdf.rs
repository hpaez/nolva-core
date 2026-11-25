#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::codecs::jpeg::JpegEncoder;
use image::{GrayImage, Luma, Rgb, RgbImage};
use lopdf::{Document, LoadOptions, Object, ObjectId};
use pdf_engine::{Mark, PageSize, PdfError, PdfOptions, PdfPage, TextBox, write_pdf};

fn write_jpeg(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    let path = dir.join(name);
    RgbImage::from_pixel(width, height, Rgb([200, 60, 40]))
        .save(&path)
        .unwrap();
    path
}

/// A JPEG with an EXIF "rotate 90° clockwise" tag, like phone portraits.
fn write_rotated_jpeg(dir: &Path, width: u32, height: u32) -> PathBuf {
    let mut jpeg = Vec::new();
    JpegEncoder::new(Cursor::new(&mut jpeg))
        .encode_image(&RgbImage::from_pixel(width, height, Rgb([10, 120, 200])))
        .unwrap();
    #[rustfmt::skip]
    let tiff: [u8; 26] = [
        b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00,
        0x01, 0x00,
        0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x06, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];
    let mut app1 = vec![0xFF, 0xE1];
    app1.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);

    let path = dir.join("rotated.jpg");
    fs::write(&path, out).unwrap();
    path
}

fn media_box(doc: &Document, page: ObjectId) -> (f32, f32) {
    let dict = doc.get_dictionary(page).unwrap();
    let rect = dict.get(b"MediaBox").unwrap().as_array().unwrap();
    let num = |o: &Object| {
        o.as_float()
            .or_else(|_| o.as_i64().map(|v| v as f32))
            .unwrap()
    };
    (num(&rect[2]), num(&rect[3]))
}

/// Pages with just an image, no text.
fn image_pages(images: &[PathBuf]) -> Vec<PdfPage> {
    images
        .iter()
        .map(|image| PdfPage {
            image: image.clone(),
            text: Vec::new(),
            marks: Vec::new(),
        })
        .collect()
}

/// (width, height, filter) of each page's image, in page order.
fn page_images(doc: &Document) -> Vec<(i64, i64, String)> {
    doc.get_pages()
        .values()
        .map(|&page| {
            let image = doc
                .get_page_images(page)
                .unwrap()
                .into_iter()
                .next()
                .expect("page without image");
            let filter = image.filters.unwrap_or_default().join(",");
            (image.width, image.height, filter)
        })
        .collect()
}

#[test]
fn writes_one_page_per_image_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let images = vec![
        write_jpeg(dir.path(), "a.jpg", 600, 800),
        write_jpeg(dir.path(), "b.jpg", 800, 600),
        write_jpeg(dir.path(), "c.jpg", 300, 400),
    ];
    let output = dir.path().join("out.pdf");

    let summary = write_pdf(
        "Contrato",
        &image_pages(&images),
        &PdfOptions::default(),
        &output,
    )
    .unwrap();

    assert_eq!(summary.page_count, 3);
    assert_eq!(summary.size_bytes, fs::metadata(&output).unwrap().len());
    let doc = Document::load(&output).unwrap();
    assert_eq!(doc.get_pages().len(), 3);
    let pages: Vec<_> = doc.get_pages().values().copied().collect();
    // A4 portrait, A4 landscape, A4 portrait.
    let (w0, h0) = media_box(&doc, pages[0]);
    let (w1, h1) = media_box(&doc, pages[1]);
    assert!((w0 - 595.28).abs() < 0.1 && (h0 - 841.89).abs() < 0.1);
    assert!(w1 > h1);
    assert_eq!(
        page_images(&doc)
            .iter()
            .map(|(w, h, _)| (*w, *h))
            .collect::<Vec<_>>(),
        vec![(600, 800), (800, 600), (300, 400)]
    );
}

#[test]
fn embeds_upright_jpegs_without_recompressing() {
    let dir = tempfile::tempdir().unwrap();
    let jpeg = write_jpeg(dir.path(), "page.jpg", 640, 480);
    let output = dir.path().join("out.pdf");

    write_pdf(
        "x",
        &image_pages(std::slice::from_ref(&jpeg)),
        &PdfOptions::default(),
        &output,
    )
    .unwrap();

    let doc = Document::load(&output).unwrap();
    let page = *doc.get_pages().values().next().unwrap();
    let image = doc.get_page_images(page).unwrap().remove(0);
    assert_eq!(image.filters.unwrap(), vec!["DCTDecode".to_string()]);
    assert_eq!(
        image.content,
        fs::read(&jpeg).unwrap().as_slice(),
        "JPEG was re-encoded"
    );
}

#[test]
fn applies_exif_rotation_because_pdf_viewers_ignore_it() {
    let dir = tempfile::tempdir().unwrap();
    // Stored 400×200 (sensor), meant to be seen 200×400 (portrait).
    let photo = write_rotated_jpeg(dir.path(), 400, 200);
    let output = dir.path().join("out.pdf");

    write_pdf("x", &image_pages(&[photo]), &PdfOptions::default(), &output).unwrap();

    let doc = Document::load(&output).unwrap();
    assert_eq!(page_images(&doc), vec![(200, 400, "DCTDecode".to_string())]);
    let page = *doc.get_pages().values().next().unwrap();
    let (w, h) = media_box(&doc, page);
    assert!(h > w, "portrait photo should get a portrait page");
}

#[test]
fn converts_png_and_keeps_grayscale() {
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("scan.png");
    GrayImage::from_pixel(300, 400, Luma([180]))
        .save(&png)
        .unwrap();
    let output = dir.path().join("out.pdf");

    write_pdf("x", &image_pages(&[png]), &PdfOptions::default(), &output).unwrap();

    let doc = Document::load(&output).unwrap();
    let page = *doc.get_pages().values().next().unwrap();
    let image = doc.get_page_images(page).unwrap().remove(0);
    assert_eq!((image.width, image.height), (300, 400));
    assert_eq!(image.color_space.as_deref(), Some("DeviceGray"));
}

#[test]
fn fit_image_pages_follow_the_image_shape() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = write_jpeg(dir.path(), "receipt.jpg", 300, 1200);
    let output = dir.path().join("out.pdf");
    let options = PdfOptions {
        page_size: PageSize::FitImage,
        margin_mm: 0.0,
        page_numbers: false,
        password: None,
    };

    write_pdf("x", &image_pages(&[receipt]), &options, &output).unwrap();

    let doc = Document::load(&output).unwrap();
    let page = *doc.get_pages().values().next().unwrap();
    let (w, h) = media_box(&doc, page);
    assert!((h / w - 4.0).abs() < 0.01);
}

#[test]
fn sets_the_document_title() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");
    write_pdf(
        "Factura de luz",
        &image_pages(&[write_jpeg(dir.path(), "a.jpg", 10, 10)]),
        &PdfOptions::default(),
        &output,
    )
    .unwrap();

    let doc = Document::load(&output).unwrap();
    let info_ref = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
    let title = doc.get_dictionary(info_ref).unwrap().get(b"Title").unwrap();
    assert_eq!(lopdf::decode_text_string(title).unwrap(), "Factura de luz");
}

#[test]
fn rejects_empty_documents_and_leaves_no_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");

    assert!(matches!(
        write_pdf("x", &[], &PdfOptions::default(), &output),
        Err(PdfError::NoPages)
    ));

    let missing = dir.path().join("missing.jpg");
    assert!(
        write_pdf(
            "x",
            &image_pages(&[missing]),
            &PdfOptions::default(),
            &output
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn recognized_text_is_an_invisible_selectable_layer() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");
    let line = |text: &str, top: f32| TextBox {
        text: text.into(),
        left: 0.1,
        top,
        right: 0.9,
        bottom: top + 0.05,
    };
    let pages = vec![
        PdfPage {
            image: write_jpeg(dir.path(), "a.jpg", 600, 800),
            text: vec![line("Factura electrónica", 0.1), line("Total: $5.990", 0.2)],
            marks: Vec::new(),
        },
        PdfPage {
            image: write_jpeg(dir.path(), "b.jpg", 600, 800),
            text: Vec::new(),
            marks: Vec::new(),
        },
    ];

    write_pdf("Factura", &pages, &PdfOptions::default(), &output).unwrap();

    let doc = Document::load(&output).unwrap();
    let text = doc.extract_text(&[1]).unwrap();
    assert!(text.contains("Factura electrónica"), "{text:?}");
    assert!(text.contains("Total: $5.990"), "{text:?}");
    assert!(doc.extract_text(&[2]).unwrap().trim().is_empty());

    let ids: Vec<_> = doc.get_pages().values().copied().collect();
    let content = String::from_utf8_lossy(&doc.get_page_content(ids[0])).into_owned();
    assert!(
        content.contains("3 Tr"),
        "text must be invisible: {content}"
    );
    assert_eq!(doc.get_page_fonts(ids[0]).unwrap().len(), 1);
    assert!(doc.get_page_fonts(ids[1]).unwrap().is_empty());
    // The scan itself is untouched.
    assert_eq!(page_images(&doc).len(), 2);
}

#[test]
fn pdfs_without_text_have_no_font() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");
    let pages = image_pages(&[write_jpeg(dir.path(), "a.jpg", 60, 80)]);

    write_pdf("x", &pages, &PdfOptions::default(), &output).unwrap();

    let doc = Document::load(&output).unwrap();
    let has_font = doc
        .objects
        .values()
        .filter_map(|o| o.as_dict().ok())
        .any(|d| {
            d.get(b"Type")
                .and_then(|t| t.as_name())
                .is_ok_and(|n| n == b"Font")
        });
    assert!(!has_font);
}

#[test]
fn page_numbers_are_off_by_default_and_printed_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let images = [
        write_jpeg(dir.path(), "a.jpg", 600, 800),
        write_jpeg(dir.path(), "b.jpg", 600, 800),
    ];

    let plain = dir.path().join("plain.pdf");
    write_pdf("x", &image_pages(&images), &PdfOptions::default(), &plain).unwrap();
    let doc = Document::load(&plain).unwrap();
    assert!(doc.extract_text(&[1, 2]).unwrap().trim().is_empty());

    let numbered = dir.path().join("numbered.pdf");
    let options = PdfOptions {
        page_numbers: true,
        ..PdfOptions::default()
    };
    write_pdf("x", &image_pages(&images), &options, &numbered).unwrap();
    let doc = Document::load(&numbered).unwrap();
    assert_eq!(doc.extract_text(&[1]).unwrap().trim(), "1 / 2");
    assert_eq!(doc.extract_text(&[2]).unwrap().trim(), "2 / 2");
    let ids: Vec<_> = doc.get_pages().values().copied().collect();
    let content = String::from_utf8_lossy(&doc.get_page_content(ids[1])).into_owned();
    assert!(
        content.contains("0 Tr"),
        "numbers must be visible: {content}"
    );
}

#[test]
fn page_numbers_stay_visible_after_an_invisible_text_layer() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");
    let pages = vec![PdfPage {
        image: write_jpeg(dir.path(), "a.jpg", 600, 800),
        text: vec![TextBox {
            text: "Hola".into(),
            left: 0.1,
            top: 0.1,
            right: 0.5,
            bottom: 0.15,
        }],
        marks: Vec::new(),
    }];
    let options = PdfOptions {
        page_numbers: true,
        ..PdfOptions::default()
    };

    write_pdf("x", &pages, &options, &output).unwrap();

    let doc = Document::load(&output).unwrap();
    let id = *doc.get_pages().values().next().unwrap();
    let content = String::from_utf8_lossy(&doc.get_page_content(id)).into_owned();
    let invisible = content.find("3 Tr").unwrap();
    let visible = content.rfind("0 Tr").unwrap();
    assert!(visible > invisible, "{content}");
    assert_eq!(doc.get_page_fonts(id).unwrap().len(), 2);
}

fn protected(password: &str) -> PdfOptions {
    PdfOptions {
        password: Some(password.into()),
        ..PdfOptions::default()
    }
}

#[test]
fn password_protected_pdfs_open_only_with_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");
    let jpeg = write_jpeg(dir.path(), "a.jpg", 600, 800);
    let pages = vec![PdfPage {
        image: jpeg.clone(),
        text: vec![TextBox {
            text: "Total: $5.990".into(),
            left: 0.1,
            top: 0.1,
            right: 0.9,
            bottom: 0.15,
        }],
        marks: Vec::new(),
    }];
    let options = PdfOptions {
        page_numbers: true,
        ..protected("contraseña 123")
    };

    write_pdf("Factura ñandú", &pages, &options, &output).unwrap();

    let bytes = fs::read(&output).unwrap();
    // Nothing readable without the password: not the text, title or image.
    let raw = String::from_utf8_lossy(&bytes);
    assert!(
        !raw.contains("5.990") && !raw.contains("Factura"),
        "plain text leaked"
    );
    let jpeg_bytes = fs::read(&jpeg).unwrap();
    assert!(
        !bytes.windows(64).any(|w| w == &jpeg_bytes[..64]),
        "image stored in the clear"
    );

    let locked = Document::load(&output).unwrap();
    assert!(locked.is_encrypted());
    assert!(locked.authenticate_user_password("contraseña").is_err());
    assert!(locked.authenticate_user_password("").is_err());
    locked.authenticate_user_password("contraseña 123").unwrap();
    let with =
        |password: &str| Document::load_with_options(&output, LoadOptions::with_password(password));
    assert!(with("contraseña").is_err());
    let doc = with("contraseña 123").unwrap();

    let info_ref = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
    let title = doc.get_dictionary(info_ref).unwrap().get(b"Title").unwrap();
    assert_eq!(lopdf::decode_text_string(title).unwrap(), "Factura ñandú");
    let text = doc.extract_text(&[1]).unwrap();
    assert!(
        text.contains("Total: $5.990") && text.contains("1 / 1"),
        "{text:?}"
    );
    // The scan comes back byte for byte.
    let page = *doc.get_pages().values().next().unwrap();
    let image = doc.get_page_images(page).unwrap().remove(0);
    assert_eq!(image.content, &jpeg_bytes[..]);
}

#[test]
fn rejects_empty_or_too_long_passwords() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");
    let pages = image_pages(&[write_jpeg(dir.path(), "a.jpg", 10, 10)]);

    for password in [String::new(), "x".repeat(128)] {
        let err = write_pdf("x", &pages, &protected(&password), &output).unwrap_err();
        assert!(matches!(err, PdfError::InvalidPassword), "{err:?}");
        assert!(!output.exists());
    }
    write_pdf("x", &pages, &protected(&"x".repeat(127)), &output).unwrap();
}

#[test]
fn options_never_print_the_password() {
    let debug = format!("{:?}", protected("secreto"));
    assert!(
        !debug.contains("secreto") && debug.contains("***"),
        "{debug}"
    );
}

fn marks() -> Vec<Mark> {
    vec![
        Mark::Highlight {
            left: 0.1,
            top: 0.1,
            right: 0.9,
            bottom: 0.15,
            color: 0xFF_EB_3B,
        },
        Mark::Ink {
            strokes: vec![vec![(0.2, 0.8), (0.4, 0.75), (0.6, 0.82)], vec![(0.7, 0.8)]],
            color: 0x1A_4D_8F,
            width: 0.006,
        },
        Mark::Text {
            text: "Recibido conforme\nHugo, 25/11".into(),
            x: 0.1,
            y: 0.5,
            size: 0.03,
            color: 0xC6_28_28,
        },
    ]
}

#[test]
fn annotations_are_drawn_as_vectors_over_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");
    let jpeg = write_jpeg(dir.path(), "a.jpg", 600, 800);
    let pages = vec![
        PdfPage {
            image: jpeg.clone(),
            text: Vec::new(),
            marks: marks(),
        },
        PdfPage {
            image: jpeg.clone(),
            text: Vec::new(),
            marks: Vec::new(),
        },
    ];

    write_pdf("x", &pages, &PdfOptions::default(), &output).unwrap();

    let doc = Document::load(&output).unwrap();
    let ids: Vec<_> = doc.get_pages().values().copied().collect();
    let content = String::from_utf8_lossy(&doc.get_page_content(ids[0])).into_owned();
    assert!(
        content.contains(" l\n") && content.contains("S\n"),
        "strokes: {content}"
    );
    assert!(content.contains("/GsHighlight gs"), "highlight: {content}");
    let text = doc.extract_text(&[1]).unwrap();
    assert!(
        text.contains("Recibido conforme") && text.contains("Hugo, 25/11"),
        "{text:?}"
    );
    // The highlight multiplies, translucent, so the scan stays readable.
    let state = doc
        .objects
        .values()
        .filter_map(|o| o.as_dict().ok())
        .find(|d| d.has(b"BM"))
        .expect("highlight graphics state");
    assert_eq!(state.get(b"BM").unwrap().as_name().unwrap(), b"Multiply");
    // Unannotated pages carry nothing extra, and the scan is the same image.
    assert!(doc.get_page_fonts(ids[1]).unwrap().is_empty());
    assert_eq!(page_images(&doc).len(), 2);
}

#[test]
fn annotations_work_with_numbers_text_and_a_password() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pdf");
    let pages = vec![PdfPage {
        image: write_jpeg(dir.path(), "a.jpg", 600, 800),
        text: vec![TextBox {
            text: "Escaneado".into(),
            left: 0.1,
            top: 0.3,
            right: 0.5,
            bottom: 0.35,
        }],
        marks: marks(),
    }];
    let options = PdfOptions {
        page_numbers: true,
        ..protected("1234")
    };

    write_pdf("x", &pages, &options, &output).unwrap();

    let doc = Document::load_with_options(&output, LoadOptions::with_password("1234")).unwrap();
    let text = doc.extract_text(&[1]).unwrap();
    for expected in ["Escaneado", "Recibido conforme", "1 / 1"] {
        assert!(text.contains(expected), "{expected} missing: {text:?}");
    }
    let id = *doc.get_pages().values().next().unwrap();
    assert_eq!(
        doc.get_page_fonts(id).unwrap().len(),
        2,
        "text layer + visible"
    );
}
