//! The XML parts of a minimal WordprocessingML package.

use std::fmt::Write;

use crate::layout::{BODY_HALF_POINTS, Paragraph};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const PIC: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;

/// A4 in twentieths of a point, with 2 cm margins.
const PAGE_W: u32 = 11906;
const PAGE_H: u32 = 16838;
const MARGIN: u32 = 1134;
const EMU_PER_TWIP: u64 = 635;

pub(crate) fn image_part(number: usize) -> String {
    format!("word/media/page{number}.jpeg")
}

pub(crate) fn content_types() -> String {
    format!(
        r#"{HEADER}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="jpeg" ContentType="image/jpeg"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/></Types>"#
    )
}

pub(crate) fn package_rels() -> String {
    format!(
        r#"{HEADER}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/></Relationships>"#
    )
}

pub(crate) fn core_properties(title: &str) -> String {
    format!(
        r#"{HEADER}<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{}</dc:title><dc:creator>Nolva</dc:creator></cp:coreProperties>"#,
        escape(title)
    )
}

pub(crate) fn styles() -> String {
    format!(
        r#"{HEADER}<w:styles xmlns:w="{W}"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Calibri" w:eastAsia="Calibri"/><w:sz w:val="{BODY_HALF_POINTS}"/><w:szCs w:val="{BODY_HALF_POINTS}"/><w:lang w:val="es-CL"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="259" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style></w:styles>"#
    )
}

pub(crate) fn document_rels(images: usize) -> String {
    let mut xml = format!(
        r#"{HEADER}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#
    );
    for number in 1..=images {
        let _ = write!(
            xml,
            r#"<Relationship Id="rIdPage{number}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/page{number}.jpeg"/>"#
        );
    }
    xml.push_str("</Relationships>");
    xml
}

/// The `<w:body>` of `word/document.xml`, built page by page.
#[derive(Debug, Default)]
pub(crate) struct Body {
    xml: String,
}

impl Body {
    /// Appends a page's paragraphs, on a new Word page when `new_page`. A
    /// page without text still gets its (empty) Word page.
    pub(crate) fn page(&mut self, new_page: bool, paragraphs: &[Paragraph]) {
        if paragraphs.is_empty() {
            self.xml.push_str("<w:p>");
            if new_page {
                self.xml.push_str("<w:pPr><w:pageBreakBefore/></w:pPr>");
            }
            self.xml.push_str("</w:p>");
            return;
        }
        for (index, p) in paragraphs.iter().enumerate() {
            let break_before = new_page && index == 0;
            self.xml.push_str("<w:p>");
            if break_before || p.centered {
                self.xml.push_str("<w:pPr>");
                if break_before {
                    self.xml.push_str("<w:pageBreakBefore/>");
                }
                if p.centered {
                    self.xml.push_str(r#"<w:jc w:val="center"/>"#);
                }
                self.xml.push_str("</w:pPr>");
            }
            self.xml.push_str("<w:r>");
            if p.bold || p.size_half_points != BODY_HALF_POINTS {
                self.xml.push_str("<w:rPr>");
                if p.bold {
                    self.xml.push_str("<w:b/><w:bCs/>");
                }
                if p.size_half_points != BODY_HALF_POINTS {
                    let size = p.size_half_points;
                    let _ = write!(
                        self.xml,
                        r#"<w:sz w:val="{size}"/><w:szCs w:val="{size}"/>"#
                    );
                }
                self.xml.push_str("</w:rPr>");
            }
            let _ = write!(
                self.xml,
                r#"<w:t xml:space="preserve">{}</w:t></w:r></w:p>"#,
                escape(&p.text)
            );
        }
    }

    /// Appends page image `number` (1-based) on its own page, as large as
    /// fits inside the margins.
    pub(crate) fn picture(&mut self, number: usize, width: u32, height: u32) {
        let max_w = u64::from(PAGE_W - 2 * MARGIN) * EMU_PER_TWIP;
        // Leave room for the paragraph spacing so Word doesn't push the
        // picture onto an extra page.
        let max_h = u64::from(PAGE_H - 2 * MARGIN) * EMU_PER_TWIP * 95 / 100;
        let scale =
            (max_w as f64 / f64::from(width.max(1))).min(max_h as f64 / f64::from(height.max(1)));
        let cx = (f64::from(width) * scale) as u64;
        let cy = (f64::from(height) * scale) as u64;
        let _ = write!(
            self.xml,
            r#"<w:p><w:pPr><w:pageBreakBefore/><w:jc w:val="center"/></w:pPr><w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{cx}" cy="{cy}"/><wp:docPr id="{number}" name="Page {number}"/><a:graphic xmlns:a="{A}"><a:graphicData uri="{PIC}"><pic:pic xmlns:pic="{PIC}"><pic:nvPicPr><pic:cNvPr id="{number}" name="page{number}.jpeg"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="rIdPage{number}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"#
        );
    }

    pub(crate) fn finish(self) -> String {
        format!(
            r#"{HEADER}<w:document xmlns:w="{W}" xmlns:r="{R}" xmlns:wp="{WP}" xmlns:a="{A}" xmlns:pic="{PIC}"><w:body>{}<w:sectPr><w:pgSz w:w="{PAGE_W}" w:h="{PAGE_H}"/><w:pgMar w:top="{MARGIN}" w:right="{MARGIN}" w:bottom="{MARGIN}" w:left="{MARGIN}" w:header="709" w:footer="709" w:gutter="0"/></w:sectPr></w:body></w:document>"#,
            self.xml
        )
    }
}

/// Escapes text for XML, dropping characters XML 1.0 forbids (control
/// characters other than tab), which OCR can occasionally produce.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push('\t'),
            c if c < ' ' || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_drops_control_characters() {
        assert_eq!(
            escape("A&B <c> \"d\"\u{7}é"),
            "A&amp;B &lt;c&gt; &quot;d&quot;é"
        );
    }

    #[test]
    fn empty_pages_still_get_a_page_break() {
        let mut body = Body::default();
        body.page(false, &[]);
        body.page(true, &[]);
        let xml = body.finish();
        assert_eq!(xml.matches("<w:pageBreakBefore/>").count(), 1);
        assert_eq!(xml.matches("<w:p>").count(), 2);
    }

    #[test]
    fn pictures_fit_inside_the_margins() {
        let mut body = Body::default();
        body.picture(1, 1000, 1000);
        let xml = body.finish();
        let max_w = u64::from(PAGE_W - 2 * MARGIN) * EMU_PER_TWIP;
        assert!(
            xml.contains(&format!(r#"<wp:extent cx="{max_w}" cy="{max_w}"/>"#)),
            "{xml}"
        );
        assert!(xml.contains(r#"r:embed="rIdPage1""#));
    }
}
