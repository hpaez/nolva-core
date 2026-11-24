#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use docx_engine::{DocxError, DocxLine, DocxOptions, DocxPage, write_docx};
use image::{Rgb, RgbImage};
use zip::ZipArchive;

fn jpeg(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    RgbImage::from_pixel(300, 400, Rgb([240, 240, 235]))
        .save(&path)
        .unwrap();
    path
}

fn line(block: u32, text: &str, top: f32) -> DocxLine {
    DocxLine {
        block,
        text: text.into(),
        left: 0.1,
        top,
        right: 0.9,
        bottom: top + 0.02,
    }
}

fn part(path: &Path, name: &str) -> Option<Vec<u8>> {
    let mut zip = ZipArchive::new(File::open(path).unwrap()).unwrap();
    let mut file = zip.by_name(name).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    Some(bytes)
}

fn text_part(path: &Path, name: &str) -> String {
    String::from_utf8(part(path, name).unwrap_or_else(|| panic!("missing {name}"))).unwrap()
}

#[test]
fn writes_an_editable_document_page_by_page() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("Contrato.docx");
    let pages = vec![
        DocxPage {
            image: jpeg(dir.path(), "a.jpg"),
            lines: vec![
                line(0, "Cláusula primera: el arrendatario", 0.1),
                line(0, "pagará <12> UF & gastos.", 0.13),
            ],
        },
        DocxPage {
            image: jpeg(dir.path(), "b.jpg"),
            lines: Vec::new(),
        },
        DocxPage {
            image: jpeg(dir.path(), "c.jpg"),
            lines: vec![line(0, "Firma", 0.8)],
        },
    ];

    let summary = write_docx("Contrato & anexo", &pages, &DocxOptions::default(), &output).unwrap();

    assert_eq!((summary.page_count, summary.paragraph_count), (3, 2));
    assert_eq!(summary.size_bytes, fs::metadata(&output).unwrap().len());
    let document = text_part(&output, "word/document.xml");
    assert!(
        document.contains("Cláusula primera: el arrendatario pagará &lt;12&gt; UF &amp; gastos."),
        "{document}"
    );
    assert_eq!(
        document.matches("<w:pageBreakBefore/>").count(),
        2,
        "one per page after the first"
    );
    assert!(!document.contains("<w:drawing>"));
    assert!(
        text_part(&output, "docProps/core.xml")
            .contains("<dc:title>Contrato &amp; anexo</dc:title>")
    );
    for name in [
        "[Content_Types].xml",
        "_rels/.rels",
        "word/styles.xml",
        "word/_rels/document.xml.rels",
    ] {
        assert!(part(&output, name).is_some(), "missing {name}");
    }
    assert!(part(&output, "word/media/page1.jpeg").is_none());
}

#[test]
fn can_append_the_page_images() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.docx");
    let a = jpeg(dir.path(), "a.jpg");
    let pages = vec![
        DocxPage {
            image: a.clone(),
            lines: vec![line(0, "Hola", 0.1)],
        },
        DocxPage {
            image: jpeg(dir.path(), "b.jpg"),
            lines: Vec::new(),
        },
    ];

    write_docx(
        "x",
        &pages,
        &DocxOptions {
            include_images: true,
        },
        &output,
    )
    .unwrap();

    let document = text_part(&output, "word/document.xml");
    assert_eq!(document.matches("<w:drawing>").count(), 2);
    assert!(document.contains(r#"r:embed="rIdPage2""#));
    assert!(
        text_part(&output, "word/_rels/document.xml.rels").contains(r#"Target="media/page2.jpeg""#)
    );
    // Upright JPEGs go in untouched.
    assert_eq!(
        part(&output, "word/media/page1.jpeg").unwrap(),
        fs::read(&a).unwrap()
    );
}

#[test]
fn failures_leave_no_files_behind() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.docx");

    assert!(matches!(
        write_docx("x", &[], &DocxOptions::default(), &output),
        Err(DocxError::NoPages)
    ));

    let missing = DocxPage {
        image: dir.path().join("missing.jpg"),
        lines: Vec::new(),
    };
    assert!(
        write_docx(
            "x",
            &[missing],
            &DocxOptions {
                include_images: true
            },
            &output
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}
