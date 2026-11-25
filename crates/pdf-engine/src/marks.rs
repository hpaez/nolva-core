//! Annotations drawn over the page image as vectors: pen strokes,
//! translucent highlights and visible text. The scan itself is untouched.

use pdf_writer::types::{BlendMode, LineCapStyle, LineJoinStyle, TextRenderingMode};
use pdf_writer::{Chunk, Content, Name, Ref, Str};

use crate::layout::Placement;
use crate::text::win_ansi;

/// Name of the highlight graphics state (translucent, multiplied) in each
/// page's resources.
pub(crate) const HIGHLIGHT_STATE: Name<'static> = Name(b"GsHighlight");
/// Opacity of highlights: the scan shows through, as with a real marker.
const HIGHLIGHT_ALPHA: f32 = 0.4;
/// Baseline of a line below its top, and the distance between lines, as
/// shares of the font size (Helvetica).
const ASCENT: f32 = 0.8;
const LINE_HEIGHT: f32 = 1.2;

/// An annotation in normalized coordinates (`0.0..=1.0`, origin top left)
/// of the page image. Widths and sizes are fractions of its shorter side.
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    Ink {
        strokes: Vec<Vec<(f32, f32)>>,
        /// `0xRRGGBB`.
        color: u32,
        width: f32,
    },
    Highlight {
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
        color: u32,
    },
    Text {
        text: String,
        x: f32,
        y: f32,
        size: f32,
        color: u32,
    },
}

pub(crate) fn has_highlight(marks: &[Mark]) -> bool {
    marks.iter().any(|m| matches!(m, Mark::Highlight { .. }))
}

pub(crate) fn has_text(marks: &[Mark]) -> bool {
    marks.iter().any(|m| matches!(m, Mark::Text { .. }))
}

/// Writes the graphics state that [`HIGHLIGHT_STATE`] names.
pub(crate) fn write_highlight_state(chunk: &mut Chunk, id: Ref) {
    chunk
        .ext_graphics(id)
        .non_stroking_alpha(HIGHLIGHT_ALPHA)
        .blend_mode(BlendMode::Multiply);
}

/// Appends `marks`, bottom to top, over an image drawn at `image`.
/// `font` is the visible text font's name in the page's resources.
pub(crate) fn draw(content: &mut Content, marks: &[Mark], image: &Placement, font: Name<'_>) {
    let unit = image.width.min(image.height);
    let at = |x: f32, y: f32| {
        (
            image.x + x * image.width,
            image.y + (1.0 - y) * image.height,
        )
    };
    for mark in marks {
        content.save_state();
        match mark {
            Mark::Ink {
                strokes,
                color,
                width,
            } => {
                let (r, g, b) = rgb(*color);
                content.set_stroke_rgb(r, g, b);
                content.set_line_width(width * unit);
                content.set_line_cap(LineCapStyle::RoundCap);
                content.set_line_join(LineJoinStyle::RoundJoin);
                for stroke in strokes {
                    let Some(&(x0, y0)) = stroke.first() else {
                        continue;
                    };
                    let (px, py) = at(x0, y0);
                    content.move_to(px, py);
                    if stroke.len() == 1 {
                        // A tap: a zero-length line with round caps is a dot.
                        content.line_to(px, py);
                    }
                    for &(x, y) in &stroke[1..] {
                        let (px, py) = at(x, y);
                        content.line_to(px, py);
                    }
                }
                content.stroke();
            }
            Mark::Highlight {
                left,
                top,
                right,
                bottom,
                color,
            } => {
                let (r, g, b) = rgb(*color);
                content.set_parameters(HIGHLIGHT_STATE);
                content.set_fill_rgb(r, g, b);
                let (x0, y0) = at(*left, *bottom);
                let (x1, y1) = at(*right, *top);
                content.rect(x0, y0, x1 - x0, y1 - y0);
                content.fill_nonzero();
            }
            Mark::Text {
                text,
                x,
                y,
                size,
                color,
            } => {
                let (r, g, b) = rgb(*color);
                let font_size = size * unit;
                let (left, top) = at(*x, *y);
                content.set_fill_rgb(r, g, b);
                content.begin_text();
                // The text layer before it may have left rendering invisible.
                content.set_text_rendering_mode(TextRenderingMode::Fill);
                content.set_font(font, font_size);
                for (i, line) in text.lines().enumerate() {
                    let baseline = top - font_size * (ASCENT + LINE_HEIGHT * i as f32);
                    content.set_text_matrix([1.0, 0.0, 0.0, 1.0, left, baseline]);
                    content.show(Str(&win_ansi(line)));
                }
                content.end_text();
            }
        }
        content.restore_state();
    }
}

fn rgb(color: u32) -> (f32, f32, f32) {
    let channel = |shift: u32| ((color >> shift) & 0xFF) as f32 / 255.0;
    (channel(16), channel(8), channel(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_split_into_channels() {
        assert_eq!(rgb(0xFF_80_00), (1.0, 128.0 / 255.0, 0.0));
    }
}
