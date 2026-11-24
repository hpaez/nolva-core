//! Rebuilds paragraphs from OCR lines: a scan has positioned lines, Word
//! wants flowing paragraphs. Each recognized block becomes a paragraph whose
//! lines are joined back into one text (words split with a hyphen at a line
//! end are rejoined); list items start their own paragraph. Font size,
//! bold and centring are guessed from line height and position.

/// A recognized line of a page, with its box in normalized coordinates
/// (`0.0..=1.0`, origin top left) of the page image.
#[derive(Debug, Clone, PartialEq)]
pub struct DocxLine {
    /// Block (paragraph or column) index, in reading order.
    pub block: u32,
    pub text: String,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// A paragraph ready to write.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Paragraph {
    pub text: String,
    /// Font size in half points (Word's unit): 22 is 11 pt.
    pub size_half_points: u32,
    pub bold: bool,
    pub centered: bool,
}

/// Body text size, in half points.
pub(crate) const BODY_HALF_POINTS: u32 = 22;
const MIN_HALF_POINTS: u32 = 16;
const MAX_HALF_POINTS: u32 = 56;
/// Blocks whose lines are this much taller than the page's typical line
/// are headings: bigger and bold.
const HEADING_RATIO: f32 = 1.3;
/// Within this ratio of the typical line height, text keeps the body size,
/// so OCR jitter doesn't produce 10.5 pt and 11.5 pt paragraphs.
const BODY_TOLERANCE: f32 = 0.15;

/// The paragraphs of one page, in reading order.
pub(crate) fn paragraphs(lines: &[DocxLine]) -> Vec<Paragraph> {
    let typical = median(lines.iter().map(height));
    let mut out = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let block = lines[start].block;
        let end = lines[start..]
            .iter()
            .position(|l| l.block != block)
            .map_or(lines.len(), |n| start + n);
        block_paragraphs(&lines[start..end], typical, &mut out);
        start = end;
    }
    out
}

fn block_paragraphs(lines: &[DocxLine], typical: Option<f32>, out: &mut Vec<Paragraph>) {
    let ratio = match (median(lines.iter().map(height)), typical) {
        (Some(block), Some(page)) if page > 0.0 => block / page,
        _ => 1.0,
    };
    let size_half_points = if (ratio - 1.0).abs() <= BODY_TOLERANCE {
        BODY_HALF_POINTS
    } else {
        ((BODY_HALF_POINTS as f32 * ratio).round() as u32).clamp(MIN_HALF_POINTS, MAX_HALF_POINTS)
    };
    let bold = ratio >= HEADING_RATIO;
    let centered = is_centered(lines);

    let mut text = String::new();
    for line in lines {
        let line_text = line.text.trim();
        if line_text.is_empty() {
            continue;
        }
        if !text.is_empty() && starts_list_item(line_text) {
            out.push(Paragraph {
                text: std::mem::take(&mut text),
                size_half_points,
                bold,
                centered,
            });
        }
        append_line(&mut text, line_text);
    }
    if !text.is_empty() {
        out.push(Paragraph {
            text,
            size_half_points,
            bold,
            centered,
        });
    }
}

/// Joins `line` onto `text`: with a space, or without the hyphen when a
/// word was split across lines ("docu-" + "mento" → "documento").
fn append_line(text: &mut String, line: &str) {
    if text.is_empty() {
        text.push_str(line);
        return;
    }
    let mut chars = text.chars().rev();
    let split_word = chars.next() == Some('-')
        && chars.next().is_some_and(char::is_alphabetic)
        && line.chars().next().is_some_and(char::is_lowercase);
    if split_word {
        text.pop();
    } else {
        text.push(' ');
    }
    text.push_str(line);
}

/// Bullets ("•", "-", "*") and numbering ("1.", "2)", "a)") open a new item.
fn starts_list_item(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix(['•', '-', '*', '–', '·']) {
        return rest.starts_with(' ');
    }
    let marker: String = line
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let short = !marker.is_empty()
        && (marker.chars().all(|c| c.is_ascii_digit()) && marker.len() <= 3
            || marker.len() == 1 && marker.chars().all(|c| c.is_ascii_lowercase()));
    short
        && line[marker.len()..].starts_with(['.', ')'])
        && line[marker.len() + 1..].starts_with(' ')
}

/// A block narrower than most of the page and sitting on its centre line,
/// like a title or a signature, is centred.
fn is_centered(lines: &[DocxLine]) -> bool {
    let left = lines.iter().map(|l| l.left).fold(f32::INFINITY, f32::min);
    let right = lines
        .iter()
        .map(|l| l.right)
        .fold(f32::NEG_INFINITY, f32::max);
    let width = right - left;
    width < 0.7 && ((left + right) / 2.0 - 0.5).abs() < 0.04 && left > 0.15
}

fn height(line: &DocxLine) -> f32 {
    line.bottom - line.top
}

fn median(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut values: Vec<f32> = values.filter(|v| v.is_finite() && *v > 0.0).collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    Some(values[values.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(block: u32, text: &str, left: f32, top: f32, right: f32, height: f32) -> DocxLine {
        DocxLine {
            block,
            text: text.into(),
            left,
            top,
            right,
            bottom: top + height,
        }
    }

    fn body(block: u32, text: &str, top: f32) -> DocxLine {
        line(block, text, 0.1, top, 0.9, 0.02)
    }

    #[test]
    fn joins_a_blocks_lines_into_one_paragraph() {
        let lines = [
            body(0, "El arrendatario se obliga a pagar la", 0.30),
            body(0, "renta dentro de los primeros cinco docu-", 0.33),
            body(0, "mentos y días de cada mes.", 0.36),
            body(1, "Segundo párrafo.", 0.42),
        ];
        let paragraphs = paragraphs(&lines);
        let texts: Vec<_> = paragraphs.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "El arrendatario se obliga a pagar la renta dentro de los primeros cinco documentos y días de cada mes.",
                "Segundo párrafo.",
            ]
        );
        assert!(
            paragraphs
                .iter()
                .all(|p| p.size_half_points == BODY_HALF_POINTS && !p.bold)
        );
    }

    #[test]
    fn keeps_hyphens_that_are_not_split_words() {
        let mut text = String::from("Santiago -");
        append_line(&mut text, "Chile");
        assert_eq!(text, "Santiago - Chile");
        let mut text = String::from("ex-");
        append_line(&mut text, "Presidente");
        assert_eq!(text, "ex- Presidente");
    }

    #[test]
    fn list_items_get_their_own_paragraphs() {
        let lines = [
            body(0, "Requisitos:", 0.1),
            body(0, "• Cédula de identidad", 0.13),
            body(0, "• Comprobante de domicilio", 0.16),
            body(0, "1. Firmar", 0.19),
            body(0, "a) Entregar", 0.22),
        ];
        let texts: Vec<_> = paragraphs(&lines).into_iter().map(|p| p.text).collect();
        assert_eq!(
            texts,
            [
                "Requisitos:",
                "• Cédula de identidad",
                "• Comprobante de domicilio",
                "1. Firmar",
                "a) Entregar",
            ]
        );
        assert!(!starts_list_item("2026 fue un año"));
        assert!(!starts_list_item("Dr. Pérez"));
    }

    #[test]
    fn tall_centred_blocks_are_centred_headings() {
        let lines = [
            line(0, "CONTRATO DE ARRIENDO", 0.3, 0.05, 0.7, 0.04),
            body(1, "Texto normal", 0.2),
            body(2, "más texto", 0.25),
        ];
        let paragraphs = paragraphs(&lines);
        assert!(paragraphs[0].bold && paragraphs[0].centered);
        assert_eq!(paragraphs[0].size_half_points, 44);
        assert!(!paragraphs[1].centered && !paragraphs[1].bold);
    }

    #[test]
    fn sizes_are_clamped_and_empty_pages_have_no_paragraphs() {
        let lines = [
            line(0, "ENORME", 0.1, 0.0, 0.9, 0.5),
            body(1, "a", 0.6),
            body(2, "b", 0.7),
        ];
        assert_eq!(paragraphs(&lines)[0].size_half_points, MAX_HALF_POINTS);
        assert!(paragraphs(&[]).is_empty());
    }
}
