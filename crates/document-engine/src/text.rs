//! Recognized page text (OCR results) and full-text search hits.
//!
//! Recognition itself runs on each platform (ML Kit on Android, Vision on
//! iOS); the core stores the results, keeps them in sync with the page image
//! and searches them.

use std::path::PathBuf;

use crate::error::DocumentError;
use crate::model::{DocumentId, PageId};

/// Most lines kept per page: far above any real page, it stops runaway input.
pub const MAX_LINES_PER_PAGE: usize = 5_000;
/// Most characters of text per page, for the same reason.
pub const MAX_TEXT_CHARS_PER_PAGE: usize = 200_000;

/// Rectangle in normalized coordinates (`0.0..=1.0`) of the image the text
/// was recognized in, with the origin at the top left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextBounds {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl TextBounds {
    /// Clamps the rectangle into the image (recognizers can report boxes a
    /// little outside it) and rejects non-finite or inverted rectangles.
    pub fn new(left: f32, top: f32, right: f32, bottom: f32) -> Result<Self, DocumentError> {
        if ![left, top, right, bottom].iter().all(|v| v.is_finite()) {
            return Err(DocumentError::InvalidText {
                reason: "bounds must be finite".into(),
            });
        }
        if left > right || top > bottom {
            return Err(DocumentError::InvalidText {
                reason: "bounds are inverted".into(),
            });
        }
        let clamp = |v: f32| v.clamp(0.0, 1.0);
        Ok(Self {
            left: clamp(left),
            top: clamp(top),
            right: clamp(right),
            bottom: clamp(bottom),
        })
    }
}

/// One line of recognized text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextLine {
    /// Index of the block (paragraph or column) the line belongs to, in
    /// reading order. Lines of the same block are consecutive.
    pub block: u32,
    pub text: String,
    pub bounds: TextBounds,
}

/// The text recognized in a page image.
#[derive(Debug, Clone, PartialEq)]
pub struct PageText {
    pub page_id: PageId,
    /// Image the text was recognized in, relative to the storage root. The
    /// text is current only while this is still the page's displayed image
    /// (its processed image, or the original if it has none).
    pub source_path: PathBuf,
    pub lines: Vec<TextLine>,
    pub recognized_at_ms: i64,
}

impl PageText {
    /// The text as plain text: lines of a block separated by a newline,
    /// blocks by a blank line.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        let mut previous_block = None;
        for line in &self.lines {
            match previous_block {
                None => {}
                Some(block) if block == line.block => out.push('\n'),
                Some(_) => out.push_str("\n\n"),
            }
            out.push_str(&line.text);
            previous_block = Some(line.block);
        }
        out
    }
}

/// A page whose text matches a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    pub document_id: DocumentId,
    pub page_id: PageId,
    pub page_position: u32,
    /// A short excerpt of the page text around the match.
    pub snippet: String,
}

/// Validates recognizer output: trims lines, drops empty ones and enforces
/// the size limits.
pub(crate) fn normalize_lines(lines: Vec<TextLine>) -> Result<Vec<TextLine>, DocumentError> {
    let lines: Vec<TextLine> = lines
        .into_iter()
        .filter_map(|mut line| {
            let trimmed = line.text.trim();
            if trimmed.is_empty() {
                return None;
            }
            if trimmed.len() != line.text.len() {
                line.text = trimmed.to_owned();
            }
            Some(line)
        })
        .collect();
    if lines.len() > MAX_LINES_PER_PAGE {
        return Err(DocumentError::InvalidText {
            reason: format!("more than {MAX_LINES_PER_PAGE} lines"),
        });
    }
    let chars: usize = lines.iter().map(|l| l.text.chars().count()).sum();
    if chars > MAX_TEXT_CHARS_PER_PAGE {
        return Err(DocumentError::InvalidText {
            reason: format!("more than {MAX_TEXT_CHARS_PER_PAGE} characters"),
        });
    }
    Ok(lines)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn line(block: u32, text: &str) -> TextLine {
        TextLine {
            block,
            text: text.into(),
            bounds: TextBounds::new(0.1, 0.1, 0.5, 0.2).unwrap(),
        }
    }

    #[test]
    fn bounds_are_clamped_and_validated() {
        let b = TextBounds::new(-0.01, 0.2, 1.02, 0.3).unwrap();
        assert_eq!((b.left, b.right), (0.0, 1.0));
        assert!(TextBounds::new(0.5, 0.1, 0.4, 0.2).is_err());
        assert!(TextBounds::new(f32::NAN, 0.1, 0.4, 0.2).is_err());
    }

    #[test]
    fn plain_text_separates_lines_and_blocks() {
        let text = PageText {
            page_id: PageId::new(),
            source_path: PathBuf::from("p.jpg"),
            lines: vec![
                line(0, "Factura"),
                line(0, "N° 123"),
                line(1, "Total: $5.990"),
            ],
            recognized_at_ms: 1,
        };
        assert_eq!(text.plain_text(), "Factura\nN° 123\n\nTotal: $5.990");
    }

    #[test]
    fn normalizing_trims_and_drops_empty_lines() {
        let lines =
            normalize_lines(vec![line(0, "  hola "), line(0, "   "), line(1, "chao")]).unwrap();
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["hola", "chao"]
        );
    }

    #[test]
    fn normalizing_enforces_limits() {
        let many = vec![line(0, "x"); MAX_LINES_PER_PAGE + 1];
        assert!(normalize_lines(many).is_err());
        let long = vec![line(0, &"x".repeat(MAX_TEXT_CHARS_PER_PAGE + 1))];
        assert!(normalize_lines(long).is_err());
    }
}
