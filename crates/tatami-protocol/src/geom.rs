//! Geometry in Accessibility coordinates: global, origin top-left, y grows
//! down. The platform crate flips AppKit frames once at its boundary; no
//! other crate ever sees bottom-left coordinates.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Axis {
    H,
    V,
}

impl Axis {
    pub fn other(self) -> Axis {
        match self {
            Axis::H => Axis::V,
            Axis::V => Axis::H,
        }
    }
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }
    pub fn origin(&self) -> Point {
        Point {
            x: self.x,
            y: self.y,
        }
    }
    pub fn size(&self) -> Size {
        Size {
            w: self.w,
            h: self.h,
        }
    }
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
    pub fn center(&self) -> Point {
        Point {
            x: self.x + self.w / 2.0,
            y: self.y + self.h / 2.0,
        }
    }
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    /// Shrink by `l, t, r, b`.
    pub fn inset(&self, l: f64, t: f64, r: f64, b: f64) -> Rect {
        Rect {
            x: self.x + l,
            y: self.y + t,
            w: (self.w - l - r).max(0.0),
            h: (self.h - t - b).max(0.0),
        }
    }
    /// True when every edge is within `eps` of `o`'s.
    pub fn approx_eq(&self, o: &Rect, eps: f64) -> bool {
        (self.x - o.x).abs() <= eps
            && (self.y - o.y).abs() <= eps
            && (self.w - o.w).abs() <= eps
            && (self.h - o.h).abs() <= eps
    }
    pub fn area(&self) -> f64 {
        self.w * self.h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_basics() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.right(), 110.0);
        assert_eq!(r.bottom(), 70.0);
        assert!(r.contains(Point { x: 10.0, y: 20.0 }));
        assert!(!r.contains(Point { x: 110.0, y: 20.0 }));
        assert!(r.intersects(&Rect::new(100.0, 60.0, 50.0, 50.0)));
        assert!(!r.intersects(&Rect::new(110.0, 20.0, 5.0, 5.0)));
        assert_eq!(
            r.inset(1.0, 2.0, 3.0, 4.0),
            Rect::new(11.0, 22.0, 96.0, 44.0)
        );
        assert!(r.approx_eq(&Rect::new(11.0, 21.0, 99.0, 49.0), 1.0));
        assert!(!r.approx_eq(&Rect::new(12.0, 21.0, 99.0, 49.0), 1.0));
    }
}
