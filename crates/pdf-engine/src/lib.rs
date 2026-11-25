//! PDF generation: one page per image, in order, optionally with the
//! page's recognized text as an invisible, selectable layer (see [`TextBox`])
//! and, if asked, page numbers (see [`PdfOptions::page_numbers`]).
//!
//! JPEGs that are already upright are embedded byte for byte (PDF supports
//! JPEG natively via `DCTDecode`), so exporting never loses quality and is
//! fast. Images that need work — EXIF-rotated photos, PNG, WebP — are
//! decoded, oriented and re-encoded as JPEG first, because PDF viewers
//! ignore EXIF orientation.

mod embed;
mod layout;
mod numbers;
mod text;

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref, TextStr};

pub use layout::{PageSize, PdfOptions};
pub use text::TextBox;

/// One PDF page: an image and the text recognized in it, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfPage {
    pub image: PathBuf,
    /// Lines in reading order, with boxes relative to the upright image.
    pub text: Vec<TextBox>,
}

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("a PDF needs at least one page")]
    NoPages,

    #[error("could not read page image {}: {source}", path.display())]
    Image {
        path: PathBuf,
        #[source]
        source: image_engine::ImageError,
    },

    #[error("unsupported image format: {}", .0.display())]
    UnsupportedFormat(PathBuf),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Summary of a written PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdfSummary {
    pub page_count: u32,
    pub size_bytes: u64,
}

/// Writes a PDF with one page per [`PdfPage`] to `output`, atomically.
pub fn write_pdf(
    title: &str,
    pages: &[PdfPage],
    options: &PdfOptions,
    output: &Path,
) -> Result<PdfSummary, PdfError> {
    if pages.is_empty() {
        return Err(PdfError::NoPages);
    }

    let bytes = build_pdf(title, pages, options)?;

    let partial = output.with_extension("pdf.partial");
    let result = (|| {
        let mut writer = BufWriter::new(File::create(&partial)?);
        writer.write_all(&bytes)?;
        writer
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        fs::rename(&partial, output)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&partial);
        return Err(e.into());
    }

    Ok(PdfSummary {
        page_count: pages.len() as u32,
        size_bytes: bytes.len() as u64,
    })
}

fn build_pdf(title: &str, pages: &[PdfPage], options: &PdfOptions) -> Result<Vec<u8>, PdfError> {
    let mut pdf = Pdf::new();
    let mut next_id = Ref::new(1);
    let mut alloc = || next_id.bump();

    let catalog_id = alloc();
    let tree_id = alloc();
    let info_id = alloc();

    let page_ids: Vec<Ref> = pages.iter().map(|_| alloc()).collect();
    pdf.catalog(catalog_id).pages(tree_id);
    pdf.pages(tree_id)
        .kids(page_ids.iter().copied())
        .count(page_ids.len() as i32);
    pdf.document_info(info_id)
        .title(TextStr(title))
        .producer(TextStr("Nolva"));

    // One font for every page's text layer, written only if some page has text.
    let has_text = pages.iter().any(|p| !p.text.is_empty());
    let font_id = alloc();
    if has_text {
        let mut font = pdf.type1_font(font_id);
        font.base_font(Name(b"Helvetica"));
        font.encoding_predefined(Name(b"WinAnsiEncoding"));
        font.first_char(text::FIRST_CHAR);
        font.last_char(text::LAST_CHAR);
        font.widths(std::iter::repeat_n(
            text::GLYPH_WIDTH,
            usize::from(text::LAST_CHAR - text::FIRST_CHAR) + 1,
        ));
        font.finish();
    }
    let numbers_font_id = alloc();
    if options.page_numbers {
        pdf.type1_font(numbers_font_id)
            .base_font(Name(b"Helvetica"))
            .encoding_predefined(Name(b"WinAnsiEncoding"));
    }

    let image_name = Name(b"Im0");
    for (index, (source, &page_id)) in pages.iter().zip(&page_ids).enumerate() {
        let path = &source.image;
        let embedded = embed::prepare(path)?;
        let (page_w, page_h, placement) = layout::place(options, embedded.width, embedded.height);

        let image_id = alloc();
        let content_id = alloc();

        let mut image = pdf.image_xobject(image_id, &embedded.bytes);
        image.filter(pdf_writer::Filter::DctDecode);
        image.width(embedded.width as i32);
        image.height(embedded.height as i32);
        if embedded.grayscale {
            image.color_space().device_gray();
        } else {
            image.color_space().device_rgb();
        }
        image.bits_per_component(8);
        image.finish();

        let mut content = Content::new();
        content.save_state();
        // Image space is a unit square; scale and move it onto the page.
        content.transform([
            placement.width,
            0.0,
            0.0,
            placement.height,
            placement.x,
            placement.y,
        ]);
        content.x_object(image_name);
        content.restore_state();
        text::draw(&mut content, &source.text, &placement);
        if options.page_numbers {
            numbers::draw(
                &mut content,
                &numbers::label(index + 1, pages.len()),
                page_w,
            );
        }
        pdf.stream(content_id, &content.finish());

        let mut page = pdf.page(page_id);
        page.media_box(Rect::new(0.0, 0.0, page_w, page_h));
        page.parent(tree_id);
        page.contents(content_id);
        let mut resources = page.resources();
        resources.x_objects().pair(image_name, image_id);
        // One /Font dictionary per page: writing it twice would keep only the last.
        if !source.text.is_empty() || options.page_numbers {
            let mut fonts = resources.fonts();
            if !source.text.is_empty() {
                fonts.pair(text::FONT_NAME, font_id);
            }
            if options.page_numbers {
                fonts.pair(numbers::FONT_NAME, numbers_font_id);
            }
        }
        resources.finish();
        page.finish();
    }

    Ok(pdf.finish())
}
