//! Camera math shared by the canvases: between world units and pixels, zoom
//! about a point, and fitting a box.

use gpui::{Bounds, Pixels, Point, Size, px};

/// A camera over a world whose y axis points up: `offset` is where the
/// world origin lands within the canvas, and `scale` is pixels per unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub offset: Point<Pixels>,
    pub scale: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            offset: Point::default(),
            scale: 1.,
        }
    }
}

impl Viewport {
    /// The window position of the world point `(x, y)` on a canvas at
    /// `bounds`.
    pub fn to_px(&self, bounds: Bounds<Pixels>, (x, y): (f32, f32)) -> Point<Pixels> {
        Point::new(self.scale * px(x), self.scale * px(-y)) + self.offset + bounds.origin
    }

    /// The world point under the window position `point` on a canvas at
    /// `bounds`.
    pub fn from_px(&self, bounds: Bounds<Pixels>, point: Point<Pixels>) -> (f32, f32) {
        let point = point - self.offset - bounds.origin;
        (
            f32::from(point.x / self.scale),
            f32::from(-point.y / self.scale),
        )
    }

    /// Changes the scale while keeping the world point under `position`
    /// fixed on screen.
    pub fn zoom_about(&mut self, bounds: Bounds<Pixels>, position: Point<Pixels>, scale: f32) {
        let ratio = scale / self.scale;
        let origin = bounds.origin + self.offset;
        let moved = Point::new(
            ratio * (origin.x - position.x),
            ratio * (origin.y - position.y),
        ) + position;
        self.offset = moved - bounds.origin;
        self.scale = scale;
    }

    /// Fits the world box `(x0, y0, x1, y1)` into `size` with `margin` on
    /// every side, in pixels.
    pub fn fit(&mut self, size: Size<Pixels>, (x0, y0, x1, y1): (f32, f32, f32, f32), margin: f32) {
        let (width, height) = (f32::from(size.width), f32::from(size.height));
        let available = (
            (width - 2. * margin).max(1.),
            (height - 2. * margin).max(1.),
        );
        let extent = ((x1 - x0).max(1e-3), (y1 - y0).max(1e-3));
        self.scale = (available.0 / extent.0).min(available.1 / extent.1);
        let center = ((x0 + x1) / 2., (y0 + y1) / 2.);
        self.offset = Point::new(
            px(width / 2. - center.0 * self.scale),
            px(height / 2. + center.1 * self.scale),
        );
    }

    /// The scale after a wheel of `delta` pixels, positive zooming out.
    pub fn zoomed_scale(scale: f32, delta: f32) -> f32 {
        (scale * (-delta / 400.).exp()).clamp(1e-3, 1e4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> Bounds<Pixels> {
        Bounds::new(Point::new(px(10.), px(20.)), Size::new(px(200.), px(100.)))
    }

    #[test]
    fn pixels_round_trip_through_the_world() {
        let viewport = Viewport {
            offset: Point::new(px(50.), px(60.)),
            scale: 2.,
        };
        let point = viewport.to_px(bounds(), (3., 4.));
        assert_eq!(point, Point::new(px(66.), px(72.)));
        let (x, y) = viewport.from_px(bounds(), point);
        assert!((x - 3.).abs() < 1e-5 && (y - 4.).abs() < 1e-5);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        let mut viewport = Viewport {
            offset: Point::new(px(50.), px(60.)),
            scale: 2.,
        };
        let cursor = Point::new(px(100.), px(70.));
        let before = viewport.from_px(bounds(), cursor);
        viewport.zoom_about(bounds(), cursor, 5.);
        let after = viewport.from_px(bounds(), cursor);
        assert!((before.0 - after.0).abs() < 1e-4 && (before.1 - after.1).abs() < 1e-4);
    }

    #[test]
    fn fitting_centers_the_box() {
        let mut viewport = Viewport::default();
        viewport.fit(Size::new(px(200.), px(100.)), (-10., -5., 10., 5.), 10.);
        assert!((viewport.scale - 8.).abs() < 1e-5);
        let center = viewport.to_px(
            Bounds::new(Point::default(), Size::new(px(200.), px(100.))),
            (0., 0.),
        );
        assert_eq!(center, Point::new(px(100.), px(50.)));
    }
}
