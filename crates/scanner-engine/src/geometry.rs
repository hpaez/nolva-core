/// A point in normalized image coordinates (`0.0..=1.0`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Self) -> f32 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }
}

/// A quadrilateral with corners in clockwise order starting at the top left,
/// as seen in the (oriented) image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    pub top_left: Point,
    pub top_right: Point,
    pub bottom_right: Point,
    pub bottom_left: Point,
}

impl Quad {
    /// The whole image: the default when nothing better is known.
    pub const FULL: Self = Self {
        top_left: Point::new(0.0, 0.0),
        top_right: Point::new(1.0, 0.0),
        bottom_right: Point::new(1.0, 1.0),
        bottom_left: Point::new(0.0, 1.0),
    };

    /// Builds a quad from four corners in any order.
    ///
    /// Uses the classic sum/difference rule: the top-left corner has the
    /// smallest `x + y`, the bottom-right the largest; the top-right has the
    /// largest `x - y`, the bottom-left the smallest. Robust for documents
    /// rotated up to roughly ±45°, which covers hand-held captures.
    pub fn from_unordered(points: [Point; 4]) -> Self {
        let by = |key: fn(&Point) -> f32, max: bool| {
            let mut sorted = points;
            sorted.sort_by(|a, b| key(a).total_cmp(&key(b)));
            if max { sorted[3] } else { sorted[0] }
        };
        Self {
            top_left: by(|p| p.x + p.y, false),
            bottom_right: by(|p| p.x + p.y, true),
            top_right: by(|p| p.x - p.y, true),
            bottom_left: by(|p| p.x - p.y, false),
        }
    }

    pub fn corners(&self) -> [Point; 4] {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
    }

    /// Area in normalized units (the whole image is `1.0`), via the shoelace formula.
    pub fn area(&self) -> f32 {
        let c = self.corners();
        let twice: f32 = (0..4)
            .map(|i| {
                let (a, b) = (c[i], c[(i + 1) % 4]);
                a.x * b.y - b.x * a.y
            })
            .sum();
        twice.abs() / 2.0
    }

    /// True when the four corners form a convex shape in clockwise order.
    pub fn is_convex(&self) -> bool {
        let c = self.corners();
        (0..4).all(|i| {
            let (a, b, d) = (c[i], c[(i + 1) % 4], c[(i + 2) % 4]);
            let cross = (b.x - a.x) * (d.y - b.y) - (b.y - a.y) * (d.x - b.x);
            // Image y grows downwards, so clockwise turns have positive cross products.
            cross > 0.0
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn orders_corners_regardless_of_input_order() {
        let tl = Point::new(0.1, 0.1);
        let tr = Point::new(0.9, 0.15);
        let br = Point::new(0.85, 0.9);
        let bl = Point::new(0.12, 0.88);

        let quad = Quad::from_unordered([br, tl, bl, tr]);

        assert_eq!(quad.corners(), [tl, tr, br, bl]);
        assert!(quad.is_convex());
    }

    #[test]
    fn full_quad_has_unit_area() {
        assert!((Quad::FULL.area() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn self_intersecting_quad_is_not_convex() {
        let bow_tie = Quad {
            top_left: Point::new(0.0, 0.0),
            top_right: Point::new(1.0, 1.0),
            bottom_right: Point::new(1.0, 0.0),
            bottom_left: Point::new(0.0, 1.0),
        };
        assert!(!bow_tie.is_convex());
    }
}
