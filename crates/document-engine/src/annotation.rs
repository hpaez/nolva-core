//! Annotations drawn over a page: pen strokes, highlights and text. A
//! signature is a pen drawing saved once ([`Signature`]) and placed on pages
//! as [`Annotation::Ink`].
//!
//! Annotations never touch the scan. They are vectors in normalized
//! coordinates (`0.0..=1.0`, origin top left) of the page image as
//! displayed, so they follow the page at any size. Stroke widths and text
//! sizes are fractions of the image's shorter side, which a quarter turn
//! doesn't change: turning a page turns its annotations with it.

use crate::error::DocumentError;
use crate::model::{Rotation, SignatureId};

/// Most annotations per page, and most points among a page's strokes: far
/// above real use, they stop runaway input.
pub const MAX_ANNOTATIONS_PER_PAGE: usize = 500;
pub const MAX_POINTS_PER_PAGE: usize = 50_000;
/// Most characters in a text annotation.
pub const MAX_ANNOTATION_TEXT_CHARS: usize = 1_000;
/// Largest stroke width and text size, as fractions of the shorter side.
pub const MAX_STROKE_WIDTH: f32 = 0.1;
pub const MAX_TEXT_SIZE: f32 = 0.5;

/// A point in normalized page coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Where the point lands when the page turns by `rotation`.
    fn turned(self, rotation: Rotation) -> Self {
        match rotation {
            Rotation::None => self,
            Rotation::Clockwise90 => Self::new(1.0 - self.y, self.x),
            Rotation::Clockwise180 => Self::new(1.0 - self.x, 1.0 - self.y),
            Rotation::Clockwise270 => Self::new(self.y, 1.0 - self.x),
        }
    }
}

/// Colour as `0xRRGGBB`.
pub type Rgb = u32;

#[derive(Debug, Clone, PartialEq)]
pub enum Annotation {
    /// Pen strokes, e.g. handwriting or a placed signature.
    Ink {
        /// Each stroke is the path of one touch, in order.
        strokes: Vec<Vec<Point>>,
        color: Rgb,
        /// Line width, as a fraction of the shorter side.
        width: f32,
    },
    /// A translucent marker over a rectangle, multiplied with the scan so
    /// the text under it stays readable.
    Highlight {
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
        color: Rgb,
    },
    /// Typed text; `\n` starts a new line.
    Text {
        text: String,
        /// Top left of the first line.
        x: f32,
        y: f32,
        /// Font size, as a fraction of the shorter side.
        size: f32,
        color: Rgb,
    },
}

impl Annotation {
    /// Checks one annotation and clamps it into the page (touches can end a
    /// little outside it).
    pub(crate) fn validated(self) -> Result<Self, DocumentError> {
        let clamp = |v: f32| v.clamp(0.0, 1.0);
        let finite = |values: &[f32]| values.iter().all(|v| v.is_finite());
        match self {
            Self::Ink {
                strokes,
                color,
                width,
            } => {
                check_color(color)?;
                if !(width > 0.0 && width <= MAX_STROKE_WIDTH) {
                    return Err(invalid(format!("stroke width {width} out of range")));
                }
                if strokes.is_empty() || strokes.iter().any(Vec::is_empty) {
                    return Err(invalid("ink needs at least one point per stroke"));
                }
                let strokes = strokes
                    .into_iter()
                    .map(|stroke| {
                        stroke
                            .into_iter()
                            .map(|p| {
                                if finite(&[p.x, p.y]) {
                                    Ok(Point::new(clamp(p.x), clamp(p.y)))
                                } else {
                                    Err(invalid("points must be finite"))
                                }
                            })
                            .collect()
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Self::Ink {
                    strokes,
                    color,
                    width,
                })
            }
            Self::Highlight {
                left,
                top,
                right,
                bottom,
                color,
            } => {
                check_color(color)?;
                if !finite(&[left, top, right, bottom]) {
                    return Err(invalid("highlight bounds must be finite"));
                }
                // Drags can go either way: keep the rectangle, not the gesture.
                let (left, right) = (clamp(left.min(right)), clamp(left.max(right)));
                let (top, bottom) = (clamp(top.min(bottom)), clamp(top.max(bottom)));
                if left == right || top == bottom {
                    return Err(invalid("highlight is empty"));
                }
                Ok(Self::Highlight {
                    left,
                    top,
                    right,
                    bottom,
                    color,
                })
            }
            Self::Text {
                text,
                x,
                y,
                size,
                color,
            } => {
                check_color(color)?;
                if text.trim().is_empty() || text.chars().count() > MAX_ANNOTATION_TEXT_CHARS {
                    return Err(invalid(format!(
                        "text must have 1 to {MAX_ANNOTATION_TEXT_CHARS} characters"
                    )));
                }
                if !finite(&[x, y]) {
                    return Err(invalid("text position must be finite"));
                }
                if !(size > 0.0 && size <= MAX_TEXT_SIZE) {
                    return Err(invalid(format!("text size {size} out of range")));
                }
                Ok(Self::Text {
                    text,
                    x: clamp(x),
                    y: clamp(y),
                    size,
                    color,
                })
            }
        }
    }

    fn point_count(&self) -> usize {
        match self {
            Self::Ink { strokes, .. } => strokes.iter().map(Vec::len).sum(),
            Self::Highlight { .. } | Self::Text { .. } => 1,
        }
    }

    /// The annotation where it lands when the page turns by `rotation`.
    /// Text keeps reading left to right; its anchor moves with the page.
    pub(crate) fn turned(self, rotation: Rotation) -> Self {
        match self {
            Self::Ink {
                strokes,
                color,
                width,
            } => Self::Ink {
                strokes: strokes
                    .into_iter()
                    .map(|s| s.into_iter().map(|p| p.turned(rotation)).collect())
                    .collect(),
                color,
                width,
            },
            Self::Highlight {
                left,
                top,
                right,
                bottom,
                color,
            } => {
                let a = Point::new(left, top).turned(rotation);
                let b = Point::new(right, bottom).turned(rotation);
                Self::Highlight {
                    left: a.x.min(b.x),
                    top: a.y.min(b.y),
                    right: a.x.max(b.x),
                    bottom: a.y.max(b.y),
                    color,
                }
            }
            Self::Text {
                text,
                x,
                y,
                size,
                color,
            } => {
                let p = Point::new(x, y).turned(rotation);
                Self::Text {
                    text,
                    x: p.x,
                    y: p.y,
                    size,
                    color,
                }
            }
        }
    }
}

/// Checks a page's whole set of annotations.
pub(crate) fn validate_all(annotations: Vec<Annotation>) -> Result<Vec<Annotation>, DocumentError> {
    if annotations.len() > MAX_ANNOTATIONS_PER_PAGE {
        return Err(invalid(format!(
            "at most {MAX_ANNOTATIONS_PER_PAGE} annotations per page"
        )));
    }
    let annotations: Vec<_> = annotations
        .into_iter()
        .map(Annotation::validated)
        .collect::<Result<_, _>>()?;
    if annotations
        .iter()
        .map(Annotation::point_count)
        .sum::<usize>()
        > MAX_POINTS_PER_PAGE
    {
        return Err(invalid(format!(
            "at most {MAX_POINTS_PER_PAGE} points per page"
        )));
    }
    Ok(annotations)
}

/// A handwritten signature saved to be placed on pages. Its strokes are in
/// its own box: `0.0..=1.0` on both axes, `aspect_ratio` (width / height)
/// wide, so it keeps its shape wherever it is placed.
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub id: SignatureId,
    pub strokes: Vec<Vec<Point>>,
    pub aspect_ratio: f32,
    pub created_at_ms: i64,
}

/// Most saved signatures: a few for different styles, not a library.
pub const MAX_SIGNATURES: usize = 10;

pub(crate) fn validate_signature(
    strokes: Vec<Vec<Point>>,
    aspect_ratio: f32,
) -> Result<Vec<Vec<Point>>, DocumentError> {
    if !(aspect_ratio.is_finite() && (0.05..=20.0).contains(&aspect_ratio)) {
        return Err(invalid("signature aspect ratio out of range"));
    }
    match (Annotation::Ink {
        strokes,
        color: 0,
        width: MAX_STROKE_WIDTH,
    })
    .validated()?
    {
        Annotation::Ink { strokes, .. }
            if strokes.iter().map(Vec::len).sum::<usize>() <= MAX_POINTS_PER_PAGE =>
        {
            Ok(strokes)
        }
        _ => Err(invalid("signature has too many points")),
    }
}

fn check_color(color: Rgb) -> Result<(), DocumentError> {
    if color > 0xFF_FFFF {
        return Err(invalid(format!("color {color:#x} is not 0xRRGGBB")));
    }
    Ok(())
}

fn invalid(reason: impl Into<String>) -> DocumentError {
    DocumentError::InvalidAnnotation {
        reason: reason.into(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ink(points: &[(f32, f32)]) -> Annotation {
        Annotation::Ink {
            strokes: vec![points.iter().map(|&(x, y)| Point::new(x, y)).collect()],
            color: 0x1A_4D_8F,
            width: 0.004,
        }
    }

    #[test]
    fn clamps_into_the_page_and_orders_highlights() {
        let a = ink(&[(-0.1, 0.5), (1.2, 0.5)]).validated().unwrap();
        assert_eq!(a, ink(&[(0.0, 0.5), (1.0, 0.5)]));

        let h = Annotation::Highlight {
            left: 0.8,
            top: 0.6,
            right: 0.2,
            bottom: 0.5,
            color: 0xFF_EB_3B,
        };
        assert_eq!(
            h.validated().unwrap(),
            Annotation::Highlight {
                left: 0.2,
                top: 0.5,
                right: 0.8,
                bottom: 0.6,
                color: 0xFF_EB_3B
            }
        );
    }

    #[test]
    fn rejects_what_cannot_be_drawn() {
        let bad = [
            ink(&[]),
            ink(&[(f32::NAN, 0.0)]),
            Annotation::Ink {
                strokes: vec![vec![Point::new(0.1, 0.1)]],
                color: 0x100_0000,
                width: 0.01,
            },
            Annotation::Ink {
                strokes: vec![vec![Point::new(0.1, 0.1)]],
                color: 0,
                width: 0.0,
            },
            Annotation::Highlight {
                left: 0.5,
                top: 0.1,
                right: 0.5,
                bottom: 0.2,
                color: 0,
            },
            Annotation::Text {
                text: "  ".into(),
                x: 0.1,
                y: 0.1,
                size: 0.03,
                color: 0,
            },
            Annotation::Text {
                text: "Hola".into(),
                x: 0.1,
                y: 0.1,
                size: 0.9,
                color: 0,
            },
        ];
        for annotation in bad {
            assert!(
                matches!(
                    annotation.clone().validated(),
                    Err(DocumentError::InvalidAnnotation { .. })
                ),
                "{annotation:?}"
            );
        }
        let too_many = vec![ink(&[(0.5, 0.5)]); MAX_ANNOTATIONS_PER_PAGE + 1];
        assert!(validate_all(too_many).is_err());
        let too_long = vec![ink(&vec![(0.5, 0.5); MAX_POINTS_PER_PAGE + 1])];
        assert!(validate_all(too_long).is_err());
    }

    #[test]
    fn turning_a_page_turns_its_annotations() {
        // The top left corner goes to the top right on a clockwise turn.
        let a = ink(&[(0.1, 0.2)]);
        assert_eq!(a.clone().turned(Rotation::Clockwise90), ink(&[(0.8, 0.1)]));
        assert_eq!(a.clone().turned(Rotation::Clockwise180), ink(&[(0.9, 0.8)]));
        assert_eq!(a.clone().turned(Rotation::Clockwise270), ink(&[(0.2, 0.9)]));
        // Four quarter turns are the identity.
        let mut b = a.clone();
        for _ in 0..4 {
            b = b.turned(Rotation::Clockwise90);
        }
        let Annotation::Ink { strokes, .. } = b else {
            unreachable!()
        };
        assert!((strokes[0][0].x - 0.1).abs() < 1e-6 && (strokes[0][0].y - 0.2).abs() < 1e-6);

        let h = Annotation::Highlight {
            left: 0.1,
            top: 0.2,
            right: 0.3,
            bottom: 0.25,
            color: 0,
        };
        let Annotation::Highlight {
            left,
            top,
            right,
            bottom,
            ..
        } = h.turned(Rotation::Clockwise90)
        else {
            unreachable!()
        };
        // A wide band turns into a tall one, still a proper rectangle.
        assert!((left - 0.75).abs() < 1e-6 && (right - 0.8).abs() < 1e-6);
        assert!((top - 0.1).abs() < 1e-6 && (bottom - 0.3).abs() < 1e-6);
    }

    #[test]
    fn signatures_keep_a_sane_shape() {
        let strokes = vec![vec![Point::new(0.0, 1.0), Point::new(1.0, 0.0)]];
        assert!(validate_signature(strokes.clone(), 3.0).is_ok());
        assert!(validate_signature(strokes.clone(), 0.0).is_err());
        assert!(validate_signature(strokes, f32::INFINITY).is_err());
        assert!(validate_signature(Vec::new(), 3.0).is_err());
    }
}
