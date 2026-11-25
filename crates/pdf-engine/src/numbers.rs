//! Optional page numbers ("3 / 12") at the bottom centre of each page: black
//! on a small white label, so they read on any scan, dark or light.
//!
//! Drawn with the standard Helvetica (nothing to embed). Its real widths
//! are used to centre the label; only digits, spaces and `/` are drawn.

use pdf_writer::types::TextRenderingMode;
use pdf_writer::{Content, Name, Str};

/// Name of the page-number font in each page's resources.
pub(crate) const FONT_NAME: Name<'static> = Name(b"F2");
/// Font size, in points.
const SIZE: f32 = 9.0;
/// Baseline height above the bottom edge of the page, in points.
const BASELINE: f32 = 14.0;
/// Helvetica's digit height, as a share of the font size.
const CAP_HEIGHT: f32 = 0.72;
const PAD_X: f32 = 5.0;
const PAD_Y: f32 = 3.0;

pub(crate) fn label(number: usize, total: usize) -> String {
    format!("{number} / {total}")
}

/// Width of `text` in Helvetica at [`SIZE`], in points.
fn width(text: &str) -> f32 {
    let thousandths: f32 = text
        .chars()
        .map(|c| match c {
            ' ' | '/' => 278.0,
            _ => 556.0, // digits
        })
        .sum();
    thousandths / 1000.0 * SIZE
}

/// Appends `label` centred at the bottom of a page `page_width` points wide.
pub(crate) fn draw(content: &mut Content, label: &str, page_width: f32) {
    let text_width = width(label);
    let x = (page_width - text_width) / 2.0;
    content.save_state();
    content.set_fill_gray(1.0);
    content.rect(
        x - PAD_X,
        BASELINE - PAD_Y,
        text_width + 2.0 * PAD_X,
        CAP_HEIGHT * SIZE + 2.0 * PAD_Y,
    );
    content.fill_nonzero();
    content.set_fill_gray(0.0);
    content.begin_text();
    // The text layer before it leaves rendering mode at "invisible".
    content.set_text_rendering_mode(TextRenderingMode::Fill);
    content.set_font(FONT_NAME, SIZE);
    content.set_text_matrix([1.0, 0.0, 0.0, 1.0, x, BASELINE]);
    content.show(Str(label.as_bytes()));
    content.end_text();
    content.restore_state();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_widths() {
        assert_eq!(label(3, 12), "3 / 12");
        // 3 digits (556) + 2 spaces and a slash (278 each), at 9 pt.
        let expected = (3.0 * 556.0 + 3.0 * 278.0) / 1000.0 * SIZE;
        assert!((width("3 / 12") - expected).abs() < 1e-4);
    }
}
