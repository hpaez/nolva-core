/// Paper size of the exported pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    /// Each page takes the proportions of its image, at A4 width.
    FitImage,
    A4,
    Letter,
    /// Legal (8.5 × 14 in), the usual "oficio" in Latin America.
    Legal,
}

impl PageSize {
    /// Portrait dimensions in PDF points (1/72 in), or `None` for [`Self::FitImage`].
    fn portrait_points(self) -> Option<(f32, f32)> {
        match self {
            Self::FitImage => None,
            Self::A4 => Some((595.28, 841.89)),
            Self::Letter => Some((612.0, 792.0)),
            Self::Legal => Some((612.0, 1008.0)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfOptions {
    pub page_size: PageSize,
    /// White margin around the image on fixed-size pages, in millimetres.
    pub margin_mm: f32,
    /// Print "n / total" at the bottom centre of each page. Off by default:
    /// the PDF looks exactly like the scans unless asked otherwise.
    pub page_numbers: bool,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            page_size: PageSize::A4,
            margin_mm: 0.0,
            page_numbers: false,
        }
    }
}

/// Where the image is drawn on the page, in points (origin bottom left).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Placement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

const POINTS_PER_MM: f32 = 72.0 / 25.4;
const A4_WIDTH: f32 = 595.28;

/// Page size and image placement for an image of `width` × `height` pixels.
///
/// Fixed sizes pick portrait or landscape to match the image, then fit it
/// inside the margins keeping its aspect ratio, centred.
pub(crate) fn place(options: &PdfOptions, width: u32, height: u32) -> (f32, f32, Placement) {
    let aspect = height as f32 / width as f32;

    let Some((short, long)) = options.page_size.portrait_points() else {
        let (w, h) = (A4_WIDTH, A4_WIDTH * aspect);
        return (
            w,
            h,
            Placement {
                x: 0.0,
                y: 0.0,
                width: w,
                height: h,
            },
        );
    };

    let (page_w, page_h) = if width > height {
        (long, short)
    } else {
        (short, long)
    };
    let margin = (options.margin_mm.max(0.0) * POINTS_PER_MM).min(page_w.min(page_h) / 4.0);
    let (box_w, box_h) = (page_w - 2.0 * margin, page_h - 2.0 * margin);

    let scale = (box_w / width as f32).min(box_h / height as f32);
    let (w, h) = (width as f32 * scale, height as f32 * scale);
    let placement = Placement {
        x: (page_w - w) / 2.0,
        y: (page_h - h) / 2.0,
        width: w,
        height: h,
    };
    (page_w, page_h, placement)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn fit_image_uses_the_image_proportions() {
        let options = PdfOptions {
            page_size: PageSize::FitImage,
            margin_mm: 10.0,
            page_numbers: false,
        };
        let (w, h, p) = place(&options, 1000, 2000);
        assert!(approx(w, A4_WIDTH) && approx(h, A4_WIDTH * 2.0));
        assert!(approx(p.width, w) && approx(p.height, h));
    }

    #[test]
    fn landscape_images_get_landscape_pages() {
        let (w, h, _) = place(&PdfOptions::default(), 4000, 3000);
        assert!(w > h);
    }

    #[test]
    fn images_are_centred_inside_the_margins() {
        let options = PdfOptions {
            page_size: PageSize::Letter,
            margin_mm: 25.4,
            page_numbers: false,
        }; // 1 inch
        let (w, h, p) = place(&options, 1000, 1000);
        assert_eq!((w, h), (612.0, 792.0));
        // Square image limited by the width: 612 − 2×72 = 468 pt.
        assert!(approx(p.width, 468.0) && approx(p.height, 468.0));
        assert!(approx(p.x, 72.0) && approx(p.y, (792.0 - 468.0) / 2.0));
    }
}
