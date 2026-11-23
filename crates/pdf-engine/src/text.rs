//! Invisible text layer: recognized text drawn over each page image with
//! text rendering mode 3 ("invisible"), so the PDF can be searched, selected
//! and copied while it looks exactly like the scan.
//!
//! The font is the standard Helvetica (no file to embed) with WinAnsi
//! encoding, which covers Spanish, English and the rest of Western Europe.
//! Every glyph is declared [`GLYPH_WIDTH`] wide, so a line's width is known
//! exactly and can be stretched to cover its box: selections then line up
//! with the words in the image. Characters outside WinAnsi become `?`.

use pdf_writer::types::TextRenderingMode;
use pdf_writer::{Content, Name, Str};

use crate::layout::Placement;

/// Width declared for every glyph, in thousandths of the font size.
pub(crate) const GLYPH_WIDTH: f32 = 500.0;
/// First and last WinAnsi codes declared in the font.
pub(crate) const FIRST_CHAR: u8 = 32;
pub(crate) const LAST_CHAR: u8 = 255;
/// Name of the text layer font in each page's resources.
pub(crate) const FONT_NAME: Name<'static> = Name(b"F1");
/// Baseline offset from the bottom of a line box, as a share of its height:
/// roughly Helvetica's descender, so descenders stay inside the box.
const BASELINE_RISE: f32 = 0.2;

/// A line of recognized text and its box in normalized coordinates
/// (`0.0..=1.0`, origin top left) of the page image.
#[derive(Debug, Clone, PartialEq)]
pub struct TextBox {
    pub text: String,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Appends the invisible text of `lines` to `content`, over an image drawn
/// at `image` on the page.
pub(crate) fn draw(content: &mut Content, lines: &[TextBox], image: &Placement) {
    let mut started = false;
    for line in lines {
        let encoded = win_ansi(&line.text);
        let chars = encoded.len() as f32;
        let box_w = (line.right - line.left) * image.width;
        let box_h = (line.bottom - line.top) * image.height;
        if encoded.is_empty() || !(box_w > 0.0 && box_h > 0.0) {
            continue;
        }
        if !started {
            content.begin_text();
            content.set_text_rendering_mode(TextRenderingMode::Invisible);
            started = true;
        }
        let size = box_h;
        let natural_width = chars * GLYPH_WIDTH / 1000.0 * size;
        let x = image.x + line.left * image.width;
        // PDF's origin is the bottom left; the boxes' is the top left.
        let y = image.y + (1.0 - line.bottom) * image.height + BASELINE_RISE * size;
        content.set_font(FONT_NAME, size);
        content.set_horizontal_scaling(100.0 * box_w / natural_width);
        content.set_text_matrix([1.0, 0.0, 0.0, 1.0, x, y]);
        content.show(Str(&encoded));
    }
    if started {
        content.end_text();
    }
}

/// Encodes `text` in WinAnsi (Windows-1252), replacing what it can't hold
/// (and control characters) with `?`.
pub(crate) fn win_ansi(text: &str) -> Vec<u8> {
    text.chars().map(win_ansi_byte).collect()
}

fn win_ansi_byte(c: char) -> u8 {
    let code = c as u32;
    match code {
        0x20..=0x7E | 0xA0..=0xFF => code as u8,
        _ => match c {
            '€' => 0x80,
            '‚' => 0x82,
            'ƒ' => 0x83,
            '„' => 0x84,
            '…' => 0x85,
            '†' => 0x86,
            '‡' => 0x87,
            'ˆ' => 0x88,
            '‰' => 0x89,
            'Š' => 0x8A,
            '‹' => 0x8B,
            'Œ' => 0x8C,
            'Ž' => 0x8E,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            '–' => 0x96,
            '—' => 0x97,
            '˜' => 0x98,
            '™' => 0x99,
            'š' => 0x9A,
            '›' => 0x9B,
            'œ' => 0x9C,
            'ž' => 0x9E,
            'Ÿ' => 0x9F,
            '\t' => b' ',
            _ => b'?',
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn encodes_spanish_and_typographic_characters() {
        assert_eq!(win_ansi("Ñandú ¿qué?"), b"\xD1and\xFA \xBFqu\xE9?");
        assert_eq!(win_ansi("“Total” – 5€"), b"\x93Total\x94 \x96 5\x80");
        assert_eq!(win_ansi("日本\u{7}"), b"???");
    }

    #[test]
    fn lines_are_stretched_over_their_boxes() {
        let image = Placement {
            x: 10.0,
            y: 20.0,
            width: 400.0,
            height: 600.0,
        };
        let line = TextBox {
            text: "Total".into(),
            left: 0.25,
            top: 0.5,
            right: 0.75,
            bottom: 0.55,
        };
        let mut content = Content::new();
        draw(&mut content, &[line], &image);
        let ops = String::from_utf8(content.finish().to_vec()).unwrap();

        // Box: 200 × 30 pt. Natural width: 5 × 0.5 × 30 = 75 pt.
        assert!(ops.contains("3 Tr"), "{ops}");
        assert!(ops.contains("/F1 30"), "{ops}"); // ~30, floats aside
        assert!(ops.contains("266.66"), "{ops}"); // 100 × 200 / 75
        // x = 10 + 0.25 × 400; y = 20 + 0.45 × 600 + 0.2 × 30
        assert!(ops.contains("1 0 0 1 110 296 Tm"), "{ops}");
        assert!(ops.contains("(Total) Tj"), "{ops}");
    }

    #[test]
    fn empty_or_degenerate_lines_draw_nothing() {
        let image = Placement {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let flat = TextBox {
            text: "x".into(),
            left: 0.1,
            top: 0.5,
            right: 0.2,
            bottom: 0.5,
        };
        let mut content = Content::new();
        draw(&mut content, &[flat], &image);
        assert!(content.finish().is_empty());
    }
}
