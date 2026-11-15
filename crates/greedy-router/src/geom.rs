//! Integer layout geometry: points, spans, and rectangles.

use std::fmt::Display;
use std::str::FromStr;

use thiserror::Error;

/// Snaps `pos` to the nearest multiple of `grid`.
pub fn snap_to_grid(pos: i64, grid: i64) -> i64 {
    assert!(grid > 0);

    let rem = pos.rem_euclid(grid);
    assert!(rem >= 0);
    assert!(rem < grid);
    if rem <= grid / 2 {
        pos - rem
    } else {
        pos + grid - rem
    }
}

/// A point in two-dimensional layout-space.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub x: i64,
    pub y: i64,
}

impl Point {
    /// Creates a new [`Point`] from (x,y) coordinates.
    pub fn new(x: i64, y: i64) -> Self {
        Self { x, y }
    }

    /// Creates a new point from the given direction and coordinates.
    ///
    /// If `dir` is [`Dir::Horiz`], `a` becomes the x-coordinate and `b` becomes the y-coordinate.
    /// If `dir` is [`Dir::Vert`], `a` becomes the y-coordinate and `b` becomes the x-coordinate.
    pub fn from_dir_coords(dir: Dir, a: i64, b: i64) -> Self {
        match dir {
            Dir::Horiz => Self::new(a, b),
            Dir::Vert => Self::new(b, a),
        }
    }

    /// Returns the origin, (0, 0).
    #[inline]
    pub fn zero() -> Self {
        Self { x: 0, y: 0 }
    }
    /// Creates a new [`Point`] that serves as an offset in direction `dir`.
    pub fn offset(val: i64, dir: Dir) -> Self {
        match dir {
            Dir::Horiz => Self { x: val, y: 0 },
            Dir::Vert => Self { x: 0, y: val },
        }
    }
    /// Gets the coordinate associated with direction `dir`.
    pub fn coord(&self, dir: Dir) -> i64 {
        match dir {
            Dir::Horiz => self.x,
            Dir::Vert => self.y,
        }
    }

    #[inline]
    pub fn snap_to_grid(&self, grid: i64) -> Self {
        self.snap_x_to_grid(grid).snap_y_to_grid(grid)
    }

    #[inline]
    pub fn snap_x_to_grid(&self, grid: i64) -> Self {
        let x = snap_to_grid(self.x, grid);
        Self { x, y: self.y }
    }

    #[inline]
    pub fn snap_y_to_grid(&self, grid: i64) -> Self {
        let y = snap_to_grid(self.y, grid);
        Self { x: self.x, y }
    }
}

impl std::ops::Add<Point> for Point {
    type Output = Self;
    fn add(self, rhs: Point) -> Self::Output {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::AddAssign<Point> for Point {
    fn add_assign(&mut self, rhs: Point) {
        self.x += rhs.x;
        self.y += rhs.y;
    }
}

impl std::ops::Sub<Point> for Point {
    type Output = Self;
    fn sub(self, rhs: Point) -> Self::Output {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::SubAssign<Point> for Point {
    fn sub_assign(&mut self, rhs: Point) {
        self.x -= rhs.x;
        self.y -= rhs.y;
    }
}

impl From<(i64, i64)> for Point {
    fn from(value: (i64, i64)) -> Self {
        Self {
            x: value.0,
            y: value.1,
        }
    }
}

/// A one-dimensional span.
#[derive(Debug, Default, Clone, Copy, Hash, Ord, PartialOrd, PartialEq, Eq)]
pub struct Span {
    start: i64,
    stop: i64,
}

impl Span {
    /// Creates a new [`Span`] from 0 until the specified stop.
    ///
    /// # Panics
    ///
    /// This function panics if `stop` is less than 0.
    pub fn until(stop: i64) -> Self {
        debug_assert!(stop >= 0);
        Self { start: 0, stop }
    }

    /// Creates a new [`Span`] between two integers.
    ///
    /// The caller must ensure that `start` is less
    /// than or equal to `stop`.
    pub const fn new_unchecked(start: i64, stop: i64) -> Self {
        Self { start, stop }
    }

    /// Creates a new [`Span`] between two integers.
    pub fn new(start: i64, stop: i64) -> Self {
        use std::cmp::{max, min};
        let lower = min(start, stop);
        let upper = max(start, stop);
        Self {
            start: lower,
            stop: upper,
        }
    }

    /// Creates a span of zero length encompassing the given point.
    pub fn from_point(x: i64) -> Self {
        Self { start: x, stop: x }
    }

    pub fn with_start_and_length(start: i64, length: i64) -> Self {
        Self {
            stop: start + length,
            start,
        }
    }

    pub fn with_stop_and_length(stop: i64, length: i64) -> Self {
        Self {
            start: stop - length,
            stop,
        }
    }

    /// Creates a span with the given endpoint and length.
    ///
    /// If `sign` is [`Sign::Pos`], `point` is treated as the ending/stopping point of the span.
    /// If `sign` is [`Sign::Neg`], `point` is treated as the beginning/starting point of the span.
    pub fn with_point_and_length(sign: Sign, point: i64, length: i64) -> Self {
        match sign {
            Sign::Pos => Self::with_stop_and_length(point, length),
            Sign::Neg => Self::with_start_and_length(point, length),
        }
    }

    /// Creates a new [`Span`] expanded by `amount` in the direction indicated by `pos`.
    pub fn expand(mut self, pos: bool, amount: i64) -> Self {
        if pos {
            self.stop += amount;
        } else {
            self.start -= amount;
        }
        self
    }

    /// Creates a new [`Span`] expanded by `amount` in both directions.
    pub fn expand_all(mut self, amount: i64) -> Self {
        self.stop += amount;
        self.start -= amount;
        self
    }

    /// Gets the starting ([`Sign::Neg`]) or stopping ([`Sign::Pos`]) point of a span.
    #[inline]
    pub fn point(&self, sign: Sign) -> i64 {
        match sign {
            Sign::Neg => self.start(),
            Sign::Pos => self.stop(),
        }
    }

    /// Gets the shortest distance to a point.
    pub fn distance_to(&self, point: i64) -> i64 {
        std::cmp::min((point - self.start()).abs(), (point - self.stop()).abs())
    }

    /// Creates a new [`Span`] with center `center` and length `span`.
    pub fn from_center_span(center: i64, span: i64) -> Self {
        assert!(span >= 0);
        assert_eq!(span % 2, 0);

        Self::new(center - (span / 2), center + (span / 2))
    }

    /// Creates a new [`Span`] with center `center` and length `span` and snap the edges to the
    /// grid.
    pub fn from_center_span_gridded(center: i64, span: i64, grid: i64) -> Self {
        assert!(span >= 0);
        assert_eq!(span % 2, 0);
        assert_eq!(span % grid, 0);

        let start = snap_to_grid(center - (span / 2), grid);

        Self::new(start, start + span)
    }

    /// Gets the center of the span.
    #[inline]
    pub fn center(&self) -> i64 {
        (self.start + self.stop) / 2
    }

    /// Gets the length of the span.
    #[inline]
    pub fn length(&self) -> i64 {
        self.stop - self.start
    }

    /// Gets the start of the span.
    #[inline]
    pub fn start(&self) -> i64 {
        self.start
    }

    /// Gets the stop of the span.
    #[inline]
    pub fn stop(&self) -> i64 {
        self.stop
    }

    /// Checks if the span intersects with the [`Span`] `other`.
    #[inline]
    pub fn intersects(&self, other: &Self) -> bool {
        !(other.stop < self.start || self.stop < other.start)
    }

    /// Creates a new minimal [`Span`] that contains all of the elements of `spans`.
    pub fn merge(spans: impl IntoIterator<Item = Self>) -> Self {
        use std::cmp::{max, min};
        let mut spans = spans.into_iter();
        let (mut start, mut stop) = spans
            .next()
            .expect("Span::merge requires at least one span")
            .into();

        for span in spans {
            start = min(start, span.start);
            stop = max(stop, span.stop);
        }

        debug_assert!(start <= stop);

        Span { start, stop }
    }

    /// Merges adjacent spans when `merge_fn` evaluates to true.
    pub fn merge_adjacent(
        spans: impl IntoIterator<Item = Self>,
        mut merge_fn: impl FnMut(Span, Span) -> bool,
    ) -> impl Iterator<Item = Span> {
        let mut spans: Vec<Span> = spans.into_iter().collect();
        spans.sort_by_key(|span| span.start());

        let mut merged_spans = Vec::new();

        let mut j = 0;
        while j < spans.len() {
            let mut curr_span = spans[j];
            j += 1;
            while j < spans.len() && merge_fn(curr_span, spans[j]) {
                curr_span = curr_span.union(spans[j]);
                j += 1;
            }
            merged_spans.push(curr_span);
        }

        merged_spans.into_iter()
    }

    pub fn union(self, other: Self) -> Self {
        use std::cmp::{max, min};
        Self {
            start: min(self.start, other.start),
            stop: max(self.stop, other.stop),
        }
    }

    pub fn contains(self, other: Self) -> bool {
        self.union(other) == self
    }

    /// Returns a new [`Span`] representing the union of the current span with the given point.
    pub fn add_point(self, pos: i64) -> Self {
        use std::cmp::{max, min};
        Self {
            start: min(self.start, pos),
            stop: max(self.stop, pos),
        }
    }

    /// Shrinks the given side by the given amount.
    ///
    /// Behavior is controlled by the given [`Sign`]:
    /// * If `side` is [`Sign::Pos`], shrinks from the positive end (ie. decreases the `stop`).
    /// * If `side` is [`Sign::Neg`], shrinks from the negative end (ie. increases the `start`).
    pub fn shrink(self, side: Sign, amount: i64) -> Self {
        assert!(self.length() >= amount);
        match side {
            Sign::Pos => Self::new(self.start, self.stop - amount),
            Sign::Neg => Self::new(self.start + amount, self.stop),
        }
    }

    pub fn shrink_all(self, amount: i64) -> Self {
        assert!(self.length() >= 2 * amount);
        Self {
            start: self.start + amount,
            stop: self.stop - amount,
        }
    }

    pub fn translate(self, amount: i64) -> Self {
        Self {
            start: self.start + amount,
            stop: self.stop + amount,
        }
    }

    pub fn min_distance(self, other: Span) -> i64 {
        std::cmp::max(
            0,
            self.union(other).length() - self.length() - other.length(),
        )
    }
}

impl From<(i64, i64)> for Span {
    #[inline]
    fn from(tup: (i64, i64)) -> Self {
        Self::new(tup.0, tup.1)
    }
}

impl From<Span> for (i64, i64) {
    #[inline]
    fn from(s: Span) -> Self {
        (s.start(), s.stop())
    }
}

/// An enumeration of axis-aligned directions.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Dir {
    /// The horizontal, or x-aligned, direction.
    Horiz,
    /// The vertical, or y-aligned, direction.
    Vert,
}

#[derive(Debug, Clone, Eq, PartialEq, Error)]
#[error("error parsing direction `{original}`; expected horizontal or vertical")]
pub struct DirParseError {
    original: String,
}

impl FromStr for Dir {
    type Err = DirParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lowercase = s.to_lowercase();
        match lowercase.trim() {
            "vertical" | "vert" | "v" => Ok(Self::Vert),
            "horizontal" | "horiz" | "h" => Ok(Self::Horiz),
            _ => Err(DirParseError {
                original: s.to_string(),
            }),
        }
    }
}

/// Enumeration over possible signs.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[repr(u8)]
pub enum Sign {
    /// Positive.
    Pos,
    /// Negative.
    Neg,
}

impl Sign {
    #[inline]
    pub fn as_int(&self) -> i64 {
        match self {
            Self::Pos => 1,
            Self::Neg => -1,
        }
    }
}

impl std::ops::Not for Sign {
    type Output = Self;
    /// Flips the [`Sign`].
    fn not(self) -> Self::Output {
        match self {
            Self::Pos => Self::Neg,
            Self::Neg => Self::Pos,
        }
    }
}

impl Dir {
    /// Returns the perpendicular direction.
    pub fn other(self) -> Self {
        match self {
            Self::Horiz => Self::Vert,
            Self::Vert => Self::Horiz,
        }
    }
    /// Returns the direction as a string.
    pub fn short_form(&self) -> &'static str {
        match *self {
            Self::Horiz => "h",
            Self::Vert => "v",
        }
    }
}

impl Default for Dir {
    #[inline]
    fn default() -> Self {
        Self::Horiz
    }
}

impl Display for Dir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Horiz => write!(f, "horizontal"),
            Self::Vert => write!(f, "vertical"),
        }
    }
}

impl std::ops::Not for Dir {
    type Output = Self;
    /// Exclamation Operator returns the opposite direction
    fn not(self) -> Self::Output {
        self.other()
    }
}

/// An enumeration of the sides of a axis-aligned rectangle.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Top,
    Right,
    Bot,
    Left,
}

impl Side {
    /// Gets the direction of the coordinate corresponding to this side.
    ///
    /// Top and bottom edges are y-coordinates, so they are on the **vertical** axis.
    /// Left and right edges are x-coordinates, so they are on the **horizontal** axis.
    ///
    /// Also see [`Side::edge_dir`].
    pub fn coord_dir(&self) -> Dir {
        use Dir::*;
        use Side::*;
        match self {
            Top | Bot => Vert,
            Left | Right => Horiz,
        }
    }

    /// Gets the direction of the edge corresponding to this side.
    ///
    /// Top and bottom edges are **horizontal** line segments;
    /// left and right edges are **vertical** line segments.
    ///
    /// Also see [`Side::coord_dir`].
    pub fn edge_dir(&self) -> Dir {
        use Dir::*;
        use Side::*;
        match self {
            Top | Bot => Horiz,
            Left | Right => Vert,
        }
    }

    /// Returns the opposite direction.
    pub fn other(&self) -> Self {
        match self {
            Side::Top => Side::Bot,
            Side::Right => Side::Left,
            Side::Bot => Side::Top,
            Side::Left => Side::Right,
        }
    }

    /// Returns the sign corresponding to moving towards this side.
    pub fn sign(&self) -> Sign {
        use Side::*;
        use Sign::*;
        match self {
            Top | Right => Pos,
            Bot | Left => Neg,
        }
    }

    /// Returns the side corresponding with the given [`Dir`] and [`Sign`].
    pub fn with_dir_and_sign(dir: Dir, sign: Sign) -> Side {
        match dir {
            Dir::Horiz => match sign {
                Sign::Pos => Side::Right,
                Sign::Neg => Side::Left,
            },
            Dir::Vert => match sign {
                Sign::Pos => Side::Top,
                Sign::Neg => Side::Bot,
            },
        }
    }

    /// Returns sides that bound the given direction.
    pub fn with_dir(dir: Dir) -> impl Iterator<Item = Side> {
        match dir {
            Dir::Horiz => [Side::Left, Side::Right].into_iter(),
            Dir::Vert => [Side::Bot, Side::Top].into_iter(),
        }
    }
}

impl std::ops::Not for Side {
    type Output = Self;
    /// Exclamation Operator returns the opposite direction
    fn not(self) -> Self::Output {
        self.other()
    }
}

/// An association of a value with type `T` to each of the four [`Side`]s.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Sides<T> {
    inner: [T; 4],
}

impl<T> Sides<T> {
    /// Creates a new [`Sides`] with with the provided values for each side.
    pub const fn new(top: T, right: T, bot: T, left: T) -> Self {
        // IMPORTANT: the ordering of array elements here must match
        // the ordering of variants in the [`Side`] enum.
        Self {
            inner: [top, right, bot, left],
        }
    }
}

impl<T> std::ops::Index<Side> for Sides<T> {
    type Output = T;
    fn index(&self, index: Side) -> &Self::Output {
        &self.inner[index as usize]
    }
}

impl<T> std::ops::IndexMut<Side> for Sides<T> {
    fn index_mut(&mut self, index: Side) -> &mut Self::Output {
        &mut self.inner[index as usize]
    }
}

/// An enumeration of the corners of an axis-aligned rectangle.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[repr(u8)]
pub enum Corner {
    /// The lower-left corner.
    LowerLeft,
    /// The lower-right corner.
    LowerRight,
    /// The upper-left corner.
    UpperLeft,
    /// The upper-right corner.
    UpperRight,
}

impl Corner {
    pub fn side(&self, dir: Dir) -> Side {
        use Corner::*;
        use Dir::*;
        use Side::*;
        match dir {
            Horiz => match self {
                LowerLeft | UpperLeft => Left,
                LowerRight | UpperRight => Right,
            },
            Vert => match self {
                LowerLeft | LowerRight => Bot,
                UpperLeft | UpperRight => Top,
            },
        }
    }
}

/// An axis-aligned rectangle, specified by lower-left and upper-right corners.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rect {
    /// The lower-left corner.
    pub p0: Point,
    /// The upper-right corner.
    pub p1: Point,
}
impl Rect {
    /// Returns the center point of the rectangle.
    pub fn center(&self) -> Point {
        Point::new((self.p0.x + self.p1.x) / 2, (self.p0.y + self.p1.y) / 2)
    }

    /// Creates an empty rectangle containing the given point.
    pub fn from_point(p: Point) -> Self {
        Self { p0: p, p1: p }
    }

    /// Creates an empty rectangle containing the given `(x, y)` coordinates.
    pub fn from_xy(x: i64, y: i64) -> Self {
        let p = Point::new(x, y);
        Self::from_point(p)
    }

    /// Creates a new rectangle.
    pub fn new(p0: Point, p1: Point) -> Self {
        Self {
            p0: Point::new(p0.x.min(p1.x), p0.y.min(p1.y)),
            p1: Point::new(p0.x.max(p1.x), p0.y.max(p1.y)),
        }
    }

    /// Creates a rectangle from horizontal and vertical [`Span`]s.
    pub fn from_spans(h: Span, v: Span) -> Self {
        Self {
            p0: Point::new(h.start(), v.start()),
            p1: Point::new(h.stop(), v.stop()),
        }
    }

    /// Returns the bottom y-coordinate of the rectangle.
    #[inline]
    pub fn bottom(&self) -> i64 {
        self.p0.y
    }

    /// Returns the top y-coordinate of the rectangle.
    #[inline]
    pub fn top(&self) -> i64 {
        self.p1.y
    }

    /// Returns the left x-coordinate of the rectangle.
    #[inline]
    pub fn left(&self) -> i64 {
        self.p0.x
    }

    /// Returns the right x-coordinate of the rectangle.
    #[inline]
    pub fn right(&self) -> i64 {
        self.p1.x
    }

    /// Returns the horizontal span of the rectangle.
    pub fn hspan(&self) -> Span {
        Span::new(self.p0.x, self.p1.x)
    }

    /// Returns a [`Rect`] with the given `hspan` and the same vertical span.
    pub fn with_hspan(self, hspan: Span) -> Self {
        Rect::new(
            Point::new(hspan.start(), self.p0.y),
            Point::new(hspan.stop(), self.p1.y),
        )
    }

    /// Returns a [`Rect`] with the given `vspan` and the same horizontal span.
    pub fn with_vspan(self, vspan: Span) -> Self {
        Rect::new(
            Point::new(self.p0.x, vspan.start()),
            Point::new(self.p1.x, vspan.stop()),
        )
    }

    /// Returns a [`Rect`] with the given `span` in the given `dir`, and the current span in the
    /// other direction.
    pub fn with_span(self, span: Span, dir: Dir) -> Self {
        match dir {
            Dir::Vert => self.with_vspan(span),
            Dir::Horiz => self.with_hspan(span),
        }
    }

    /// Returns the vertical span of the rectangle.
    pub fn vspan(&self) -> Span {
        Span::new(self.p0.y, self.p1.y)
    }

    /// Returns the horizontal width of the rectangle.
    #[inline]
    pub fn width(&self) -> i64 {
        self.hspan().length()
    }

    /// Returns the vertical height of the rectangle.
    #[inline]
    pub fn height(&self) -> i64 {
        self.vspan().length()
    }

    /// Returns the area of the rectangle.
    #[inline]
    pub fn area(&self) -> i64 {
        self.width() * self.height()
    }

    /// Returns the lower edge of the rectangle in the [`Dir`] `dir.
    pub fn lower_edge(&self, dir: Dir) -> i64 {
        self.span(dir).start()
    }

    /// Returns the upper edge of the rectangle in the [`Dir`] `dir.
    pub fn upper_edge(&self, dir: Dir) -> i64 {
        self.span(dir).stop()
    }

    /// Returns the span of the rectangle in the [`Dir`] `dir.
    pub fn span(&self, dir: Dir) -> Span {
        match dir {
            Dir::Horiz => self.hspan(),
            Dir::Vert => self.vspan(),
        }
    }

    /// Returns the edges of two rectangles along the [`Dir`] `dir` in increasing order.
    fn sorted_edges(&self, other: &Self, dir: Dir) -> [i64; 4] {
        let mut edges = [
            self.lower_edge(dir),
            self.upper_edge(dir),
            other.lower_edge(dir),
            other.upper_edge(dir),
        ];
        edges.sort();
        edges
    }

    /// Returns the inner two edges of two rectangles along the [`Dir`] `dir` in increasing order.
    #[inline]
    pub fn inner_span(&self, other: &Self, dir: Dir) -> Span {
        let edges = self.sorted_edges(other, dir);
        Span::new(edges[1], edges[2])
    }

    /// Returns the outer two edges of two rectangles along the [`Dir`] `dir` in increasing order.
    #[inline]
    pub fn outer_span(&self, other: &Self, dir: Dir) -> Span {
        let edges = self.sorted_edges(other, dir);
        Span::new(edges[0], edges[3])
    }

    /// Returns the edge of a rectangle closest to the coordinate `x` along a given direction.
    pub fn edge_closer_to(&self, x: i64, dir: Dir) -> i64 {
        let (x0, x1) = self.span(dir).into();
        if (x - x0).abs() <= (x - x1).abs() {
            x0
        } else {
            x1
        }
    }

    /// Returns the edge of a rectangle farthest from the coordinate `x` along a given direction.
    pub fn edge_farther_from(&self, x: i64, dir: Dir) -> i64 {
        let (x0, x1) = self.span(dir).into();
        if (x - x0).abs() <= (x - x1).abs() {
            x1
        } else {
            x0
        }
    }

    /// Returns a builder for creating a rectangle from [`Span`]s.
    #[inline]
    pub fn span_builder() -> RectSpanBuilder {
        RectSpanBuilder::new()
    }

    /// Returns the length of this rectangle in the given direction.
    pub fn length(&self, dir: Dir) -> i64 {
        self.span(dir).length()
    }

    /// Returns the direction in which the rectangle is longer, choosing [`Dir::Vert`] if the sides
    /// are equal.
    #[inline]
    pub fn longer_dir(&self) -> Dir {
        if self.width() > self.height() {
            Dir::Horiz
        } else {
            Dir::Vert
        }
    }

    /// Returns the direction in which the rectangle is longer, choosing [`Dir::Vert`] if the sides
    /// are equal.
    #[inline]
    pub fn shorter_dir(&self) -> Dir {
        !self.longer_dir()
    }

    /// Expands the rectangle by `amount` on all sides.
    #[inline]
    pub fn expand(&self, amount: i64) -> Self {
        Self::new(
            Point::new(self.p0.x - amount, self.p0.y - amount),
            Point::new(self.p1.x + amount, self.p1.y + amount),
        )
    }

    /// Shrinks the rectangle by `amount` on all sides.
    #[inline]
    pub fn shrink(&self, amount: i64) -> Self {
        assert!(2 * amount <= self.width());
        assert!(2 * amount <= self.height());
        Self::new(
            Point::new(self.p0.x + amount, self.p0.y + amount),
            Point::new(self.p1.x - amount, self.p1.y - amount),
        )
    }

    /// Expands the rectangle by `amount` on both sides associated with the direction `dir`.
    #[inline]
    pub fn expand_dir(&self, dir: Dir, amount: i64) -> Self {
        match dir {
            Dir::Horiz => Self::new(
                Point::new(self.p0.x - amount, self.p0.y),
                Point::new(self.p1.x + amount, self.p1.y),
            ),
            Dir::Vert => Self::new(
                Point::new(self.p0.x, self.p0.y - amount),
                Point::new(self.p1.x, self.p1.y + amount),
            ),
        }
    }

    /// Expands the rectangle by `amount` on the given side.
    #[inline]
    pub fn expand_side(&self, side: Side, amount: i64) -> Self {
        match side {
            Side::Top => Self::new(
                Point::new(self.p0.x, self.p0.y),
                Point::new(self.p1.x, self.p1.y + amount),
            ),
            Side::Bot => Self::new(
                Point::new(self.p0.x, self.p0.y - amount),
                Point::new(self.p1.x, self.p1.y),
            ),
            Side::Right => Self::new(
                Point::new(self.p0.x, self.p0.y),
                Point::new(self.p1.x + amount, self.p1.y),
            ),
            Side::Left => Self::new(
                Point::new(self.p0.x - amount, self.p0.y),
                Point::new(self.p1.x, self.p1.y),
            ),
        }
    }

    /// Returns the desired corner of the rectangle.
    pub fn corner(&self, corner: Corner) -> Point {
        match corner {
            Corner::LowerLeft => self.p0,
            Corner::LowerRight => Point::new(self.p1.x, self.p0.y),
            Corner::UpperLeft => Point::new(self.p0.x, self.p1.y),
            Corner::UpperRight => self.p1,
        }
    }

    /// Grows this rectangle by a factor of 2 on the given [`Side`].
    ///
    /// Sometimes useful for half-track geometry.
    pub fn double(self, side: Side) -> Self {
        match side {
            Side::Top => Self::from_spans(
                self.hspan(),
                Span::with_start_and_length(self.bottom(), 2 * self.height()),
            ),
            Side::Bot => Self::from_spans(
                self.hspan(),
                Span::with_stop_and_length(self.top(), 2 * self.height()),
            ),
            Side::Left => Self::from_spans(
                Span::with_stop_and_length(self.right(), 2 * self.width()),
                self.vspan(),
            ),
            Side::Right => Self::from_spans(
                Span::with_start_and_length(self.left(), 2 * self.width()),
                self.vspan(),
            ),
        }
    }

    #[inline]
    pub fn side(&self, side: Side) -> i64 {
        match side {
            Side::Top => self.top(),
            Side::Bot => self.bottom(),
            Side::Right => self.right(),
            Side::Left => self.left(),
        }
    }

    /// Snaps the corners of this rectangle to the given grid.
    ///
    /// Note that the rectangle may have zero area after snapping.
    #[inline]
    pub fn snap_to_grid(&self, grid: i64) -> Self {
        Self::new(self.p0.snap_to_grid(grid), self.p1.snap_to_grid(grid))
    }

    pub fn cutout(&self, clip: Rect) -> [Rect; 4] {
        let src = *self;
        let t_span = Span::new(clip.top(), src.top());
        let b_span = Span::new(src.bottom(), clip.bottom());
        let l_span = Span::new(src.left(), clip.left());
        let r_span = Span::new(clip.right(), src.right());

        [
            Rect::from_spans(src.hspan(), t_span),
            Rect::from_spans(src.hspan(), b_span),
            Rect::from_spans(l_span, src.vspan()),
            Rect::from_spans(r_span, src.vspan()),
        ]
    }
}

/// A helper struct for building [`Rect`]s from [`Span`]s.
#[derive(Clone, Default, Eq, PartialEq, Hash)]
pub struct RectSpanBuilder {
    hspan: Option<Span>,
    vspan: Option<Span>,
}

impl RectSpanBuilder {
    /// Creates a new [`RectSpanBuilder`].
    pub fn new() -> Self {
        Default::default()
    }

    /// Associates [`Span`] span with direction `dir`.
    pub fn with(&mut self, dir: Dir, span: Span) -> &mut Self {
        match dir {
            Dir::Horiz => self.hspan = Some(span),
            Dir::Vert => self.vspan = Some(span),
        }
        self
    }

    /// Builds a [`Rect`] from the specified spans.
    ///
    /// Panics if one or more directions were left unspecified.
    pub fn build(&self) -> Rect {
        Rect::from_spans(self.hspan.unwrap(), self.vspan.unwrap())
    }
}
