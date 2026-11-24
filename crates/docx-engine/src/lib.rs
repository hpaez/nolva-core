//! Word (.docx) generation: an editable document rebuilt from the text
//! recognized in each page, one Word page per scanned page, optionally
//! followed by the page images.
//!
//! The package is written by hand (a few small XML parts in a zip) rather
//! than with a Word library: Nolva only needs paragraphs, page breaks and
//! pictures, and this keeps the dependency tree and binary small.

mod layout;
mod xml;

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use image_engine::{ImageError, upright_jpeg};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

pub use layout::DocxLine;

#[derive(Debug, thiserror::Error)]
pub enum DocxError {
    #[error("a document needs at least one page")]
    NoPages,

    #[error("could not read page image {}: {source}", path.display())]
    Image {
        path: PathBuf,
        #[source]
        source: ImageError,
    },

    #[error("could not write the document: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One scanned page: its recognized lines (empty if it has no text) and its
/// image, used when [`DocxOptions::include_images`] is set.
#[derive(Debug, Clone, PartialEq)]
pub struct DocxPage {
    pub image: PathBuf,
    /// Lines in reading order; a block's lines are consecutive.
    pub lines: Vec<DocxLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DocxOptions {
    /// Append each page's image, one per page, after the text.
    pub include_images: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocxSummary {
    pub page_count: u32,
    pub paragraph_count: u32,
    pub size_bytes: u64,
}

/// Writes a .docx for `pages` to `output`, atomically.
pub fn write_docx(
    title: &str,
    pages: &[DocxPage],
    options: &DocxOptions,
    output: &Path,
) -> Result<DocxSummary, DocxError> {
    if pages.is_empty() {
        return Err(DocxError::NoPages);
    }

    let partial = output.with_extension("docx.partial");
    let result = write_package(title, pages, options, &partial).and_then(|paragraph_count| {
        fs::rename(&partial, output)?;
        Ok(paragraph_count)
    });
    let paragraph_count = match result {
        Ok(count) => count,
        Err(e) => {
            let _ = fs::remove_file(&partial);
            return Err(e);
        }
    };

    Ok(DocxSummary {
        page_count: pages.len() as u32,
        paragraph_count,
        size_bytes: fs::metadata(output)?.len(),
    })
}

fn write_package(
    title: &str,
    pages: &[DocxPage],
    options: &DocxOptions,
    path: &Path,
) -> Result<u32, DocxError> {
    let mut body = xml::Body::default();
    let mut paragraph_count = 0;
    for (index, page) in pages.iter().enumerate() {
        let paragraphs = layout::paragraphs(&page.lines);
        paragraph_count += paragraphs.len() as u32;
        body.page(index > 0, &paragraphs);
    }

    let mut images = Vec::new();
    if options.include_images {
        for (index, page) in pages.iter().enumerate() {
            let jpeg = upright_jpeg(&page.image).map_err(|source| DocxError::Image {
                path: page.image.clone(),
                source,
            })?;
            body.picture(index + 1, jpeg.width, jpeg.height);
            images.push(jpeg.bytes);
        }
    }

    let mut zip = ZipWriter::new(BufWriter::new(File::create(path)?));
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    // JPEGs are already compressed.
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    let parts = [
        ("[Content_Types].xml", xml::content_types()),
        ("_rels/.rels", xml::package_rels()),
        ("docProps/core.xml", xml::core_properties(title)),
        ("word/styles.xml", xml::styles()),
        (
            "word/_rels/document.xml.rels",
            xml::document_rels(images.len()),
        ),
        ("word/document.xml", body.finish()),
    ];
    for (name, content) in parts {
        zip.start_file(name, deflated)?;
        zip.write_all(content.as_bytes())?;
    }
    for (index, image) in images.iter().enumerate() {
        zip.start_file(xml::image_part(index + 1), stored)?;
        zip.write_all(image)?;
    }
    let file = zip.finish()?.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(paragraph_count)
}
