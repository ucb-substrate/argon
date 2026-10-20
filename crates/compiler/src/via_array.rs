//! The search behind the `max_via_array` builtin: the via array that fits the
//! most cuts between two overlapping shapes.
//!
//! Every comparison snaps both sides to the grid, exactly as an Argon
//! comparison does, and every rectangle the search builds has its coordinates
//! snapped, as a solved `crect` would.

/// A rectangle as `(x0, y0, x1, y1)`.
pub type Bounds = [f64; 4];

/// The rules and shapes of one via array search.
pub struct ViaArrayQuery {
    pub size: f64,
    pub space: f64,
    /// Enclosure of the cuts by the bottom shape, as `(short, long)`.
    pub bot_enc: (f64, f64),
    /// Enclosure of the cuts by the top shape, as `(short, long)`.
    pub top_enc: (f64, f64),
    pub bot: Bounds,
    pub top: Bounds,
    /// The direction the bottom enclosure's long side must take: 0 for
    /// either, 1 for horizontal, 2 for vertical.
    pub bot_ext_dir: i64,
    pub top_ext_dir: i64,
    /// Whether a shape too narrow for one cut still gets a row of them along
    /// its length, rather than a single cut.
    pub longer: bool,
    /// Whether a later candidate wins a tie in excess area.
    pub later_wins_ties: bool,
}

/// The chosen array: cuts in x and y, the bottom and top enclosures in x and
/// y, the number of cuts, and the enclosure area outside the shapes.
pub type ViaArrayChoice = (i64, i64, f64, f64, f64, f64, i64, f64);

struct Grid(f64);

impl Grid {
    fn snap(&self, value: f64) -> f64 {
        crate::tech::snap(value, self.0)
    }

    fn lt(&self, a: f64, b: f64) -> bool {
        self.snap(a) < self.snap(b)
    }

    fn le(&self, a: f64, b: f64) -> bool {
        self.snap(a) <= self.snap(b)
    }

    /// `max_float`: the first value on a tie.
    fn max(&self, a: f64, b: f64) -> f64 {
        if self.lt(a, b) { b } else { a }
    }

    /// `min_float`: the second value on a tie.
    fn min(&self, a: f64, b: f64) -> f64 {
        if self.lt(a, b) { a } else { b }
    }

    fn rect(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> Bounds {
        [self.snap(x0), self.snap(y0), self.snap(x1), self.snap(y1)]
    }

    /// The overlap of `a` and `b`, or an empty rectangle at `a`'s corner.
    fn intersection(&self, a: Bounds, b: Bounds) -> Bounds {
        let x0 = self.max(a[0], b[0]);
        let y0 = self.max(a[1], b[1]);
        let x1 = self.min(a[2], b[2]);
        let y1 = self.min(a[3], b[3]);
        if self.le(x0, x1) && self.le(y0, y1) {
            self.rect(x0, y0, x1, y1)
        } else {
            self.rect(a[0], a[1], a[0], a[1])
        }
    }

    fn area(&self, r: Bounds) -> f64 {
        self.max(0., r[2] - r[0]) * self.max(0., r[3] - r[1])
    }

    /// Rounds to the nearest multiple of 5, away from zero on a tie.
    fn snap5(&self, v: f64) -> f64 {
        if self.lt(v, 0.) {
            (((v / 5.) - 0.5) as i64) as f64 * 5.
        } else {
            (((v / 5.) + 0.5) as i64) as f64 * 5.
        }
    }
}

fn max_cuts(len: f64, ext2: f64, size: f64, space: f64) -> i64 {
    // A tiny epsilon keeps an exact quotient such as 14.0 that arrives as
    // 13.999999999999998 from truncating to 13.
    i64::max(
        (((len + space - ext2) / (size + space)) + 0.000001) as i64,
        0,
    )
}

fn cuts_along(len: [f64; 3], bot_ext: f64, top_ext: f64, size: f64, space: f64) -> i64 {
    let bot = max_cuts(len[0], 2. * bot_ext, size, space);
    let top = max_cuts(len[1], 2. * top_ext, size, space);
    let overlap = max_cuts(len[2], 0., size, space);
    i64::max(0, i64::min(i64::min(bot, top), overlap))
}

/// The enclosure in x and y for an extension direction.
fn enclosure(enc: (f64, f64), ext_dir: i64, transpose: bool) -> (f64, f64) {
    match ext_dir {
        1 => (enc.1, enc.0),
        2 => (enc.0, enc.1),
        _ if transpose => (enc.1, enc.0),
        _ => enc,
    }
}

fn candidate(q: &ViaArrayQuery, grid: &Grid, bot_t: bool, top_t: bool) -> ViaArrayChoice {
    if (q.bot_ext_dir != 0 && bot_t) || (q.top_ext_dir != 0 && top_t) {
        return (0, 0, 0., 0., 0., 0., -1, 1000000000000.);
    }
    let (bot, top) = (q.bot, q.top);
    let bd = enclosure(q.bot_enc, q.bot_ext_dir, bot_t);
    let td = enclosure(q.top_enc, q.top_ext_dir, top_t);
    let overlap = grid.intersection(bot, top);
    let ow = grid.max(0., overlap[2] - overlap[0]);
    let oh = grid.max(0., overlap[3] - overlap[1]);
    let mut nx = cuts_along(
        [bot[2] - bot[0], top[2] - top[0], ow],
        bd.0,
        td.0,
        q.size,
        q.space,
    );
    let mut ny = cuts_along(
        [bot[3] - bot[1], top[3] - top[1], oh],
        bd.1,
        td.1,
        q.size,
        q.space,
    );
    if nx == 0 || ny == 0 {
        (nx, ny) = if q.longer {
            (i64::max(nx, 1), i64::max(ny, 1))
        } else {
            (1, 1)
        };
    }
    let center = center(grid, bot, top);
    let aw = q.size * nx as f64 + q.space * (nx - 1) as f64;
    let ah = q.size * ny as f64 + q.space * (ny - 1) as f64;
    let ax0 = grid.snap5((((center[0] + center[2]) / 2.) as i64) as f64 - aw / 2.);
    let ay0 = grid.snap5((((center[1] + center[3]) / 2.) as i64) as f64 - ah / 2.);
    let arr = grid.rect(ax0, ay0, ax0 + aw, ay0 + ah);
    let expand = |d: (f64, f64)| grid.rect(arr[0] - d.0, arr[1] - d.1, arr[2] + d.0, arr[3] + d.1);
    let (br, tr) = (expand(bd), expand(td));
    let diff = grid.area(br) - grid.area(grid.intersection(br, bot)) + grid.area(tr)
        - grid.area(grid.intersection(tr, top));
    (nx, ny, bd.0, bd.1, td.0, td.1, nx * ny, diff)
}

/// The overlap of the two shapes, or the bottom shape if they do not overlap.
fn center(grid: &Grid, bot: Bounds, top: Bounds) -> Bounds {
    let x0 = grid.max(bot[0], top[0]);
    let y0 = grid.max(bot[1], top[1]);
    let x1 = grid.min(bot[2], top[2]);
    let y1 = grid.min(bot[3], top[3]);
    if grid.le(x0, x1) && grid.le(y0, y1) {
        grid.rect(x0, y0, x1, y1)
    } else {
        bot
    }
}

/// Chooses among the four enclosure orientations: the most cuts, then the
/// least enclosure area outside the shapes. A tie within half a square unit
/// keeps the earlier candidate, unless `later_wins_ties`.
pub fn max_via_array(q: &ViaArrayQuery, grid: f64) -> ViaArrayChoice {
    let grid = Grid(grid);
    let better = |old: ViaArrayChoice, new: ViaArrayChoice| {
        let replace = new.6 > old.6
            || (new.6 == old.6
                && if q.later_wins_ties {
                    grid.le(new.7, old.7 + 0.5)
                } else {
                    grid.lt(new.7 + 0.5, old.7)
                });
        if replace { new } else { old }
    };
    let c00 = candidate(q, &grid, false, false);
    let c01 = candidate(q, &grid, true, false);
    let c10 = candidate(q, &grid, false, true);
    let c11 = candidate(q, &grid, true, true);
    better(better(better(c00, c01), c10), c11)
}
