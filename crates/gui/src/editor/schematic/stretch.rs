//! Route rewrites that keep wires orthogonal when their pins or segments
//! move. A wire is its polyline on the grid: the start pin, the corners, and
//! the end pin.

/// A grid point.
pub type GridPt = (i64, i64);

fn add((x, y): GridPt, (dx, dy): GridPt) -> GridPt {
    (x + dx, y + dy)
}

/// Drops repeated points and corners that lie on a straight axis-aligned
/// run between their neighbors.
pub fn simplify(points: &[GridPt]) -> Vec<GridPt> {
    let mut out: Vec<GridPt> = Vec::with_capacity(points.len());
    for point in points {
        if out.last() == Some(point) {
            continue;
        }
        if out.len() >= 2 {
            let a = out[out.len() - 2];
            let b = out[out.len() - 1];
            let straight = (a.0 == b.0 && b.0 == point.0) || (a.1 == b.1 && b.1 == point.1);
            if straight {
                out.pop();
            }
        }
        out.push(*point);
    }
    out
}

/// The corners of a polyline: everything but its ends.
pub fn corners(points: &[GridPt]) -> Vec<GridPt> {
    if points.len() < 2 {
        return Vec::new();
    }
    points[1..points.len() - 1].to_vec()
}

/// Moves the start pin of `points` by `delta`. The end segment keeps its
/// direction and the next one stretches; a straight wire, or a diagonal end
/// segment, gets a corner.
pub fn move_start(points: &[GridPt], delta: GridPt) -> Vec<GridPt> {
    let Some(&start) = points.first() else {
        return Vec::new();
    };
    let moved = add(start, delta);
    if points.len() == 1 {
        return vec![moved];
    }
    let next = points[1];
    let horizontal = start.1 == next.1;
    let vertical = start.0 == next.0;
    let mut out = vec![moved];
    if points.len() == 2 {
        let end = next;
        if moved.0 != end.0 && moved.1 != end.1 {
            // Bend once, keeping the end segment's direction at the pin.
            out.push(if vertical {
                (moved.0, end.1)
            } else {
                (end.0, moved.1)
            });
        }
        out.push(end);
        return simplify(&out);
    }
    if horizontal {
        out.push((next.0, next.1 + delta.1));
    } else if vertical {
        out.push((next.0 + delta.0, next.1));
    } else {
        out.push((next.0, moved.1));
        out.push(next);
    }
    out.extend_from_slice(&points[2..]);
    simplify(&out)
}

/// Moves the end pin of `points` by `delta`, as [`move_start`] does.
pub fn move_end(points: &[GridPt], delta: GridPt) -> Vec<GridPt> {
    let reversed = points.iter().rev().copied().collect::<Vec<_>>();
    let mut moved = move_start(&reversed, delta);
    moved.reverse();
    moved
}

/// Moves both pins, and so the whole wire, by `delta`.
pub fn move_both(points: &[GridPt], delta: GridPt) -> Vec<GridPt> {
    points.iter().map(|point| add(*point, delta)).collect()
}

/// Moves segment `index`, between `points[index]` and `points[index + 1]`,
/// sideways by the part of `delta` across it. The corners at its ends move
/// with it, and a corner is added where it ends at a pin.
pub fn move_segment(points: &[GridPt], index: usize, delta: GridPt) -> Vec<GridPt> {
    if index + 1 >= points.len() {
        return points.to_vec();
    }
    let (p, q) = (points[index], points[index + 1]);
    let shift = if p.1 == q.1 {
        (0, delta.1)
    } else if p.0 == q.0 {
        (delta.0, 0)
    } else {
        delta
    };
    if shift == (0, 0) {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(points.len() + 2);
    out.extend_from_slice(&points[..index]);
    if index == 0 {
        // The start pin stays; a new corner connects it to the moved run.
        out.push(p);
    }
    out.push(add(p, shift));
    out.push(add(q, shift));
    if index + 1 == points.len() - 1 {
        out.push(q);
    }
    out.extend_from_slice(&points[index + 2..]);
    simplify(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_move_keeps_the_end_segments_direction() {
        // Horizontal end segment: the corner follows the vertical part.
        let wire = [(-6, 0), (-4, 0), (-4, 4), (-2, 4)];
        assert_eq!(
            move_start(&wire, (0, 2)),
            [(-6, 2), (-4, 2), (-4, 4), (-2, 4)]
        );
        assert_eq!(
            move_start(&wire, (1, 0)),
            [(-5, 0), (-4, 0), (-4, 4), (-2, 4)]
        );
        // Vertical end segment: the corner follows the horizontal part.
        assert_eq!(move_end(&wire, (2, 0)), [(-6, 0), (-4, 0), (-4, 4), (0, 4)]);
        let vertical = [(0, 0), (0, 4), (3, 4)];
        assert_eq!(move_start(&vertical, (1, 1)), [(1, 1), (1, 4), (3, 4)]);
    }

    #[test]
    fn straight_and_diagonal_wires_get_a_corner() {
        assert_eq!(
            move_start(&[(0, 0), (4, 0)], (0, 2)),
            [(0, 2), (4, 2), (4, 0)]
        );
        assert_eq!(
            move_start(&[(0, 0), (0, 4)], (2, 0)),
            [(2, 0), (2, 4), (0, 4)]
        );
        // Still straight: no corner.
        assert_eq!(move_start(&[(0, 0), (4, 0)], (1, 0)), [(1, 0), (4, 0)]);
        // A diagonal end segment bends into a horizontal run first.
        assert_eq!(
            move_start(&[(0, 0), (2, 3), (5, 3)], (0, 1)),
            [(0, 1), (2, 1), (2, 3), (5, 3)]
        );
    }

    #[test]
    fn a_wire_whose_ends_both_move_moves_with_them() {
        assert_eq!(
            move_both(&[(0, 0), (0, 4), (3, 4)], (1, -1)),
            [(1, -1), (1, 3), (4, 3)]
        );
    }

    #[test]
    fn a_segment_drag_moves_its_corners_and_bends_at_pins() {
        let wire = [(0, 0), (0, 4), (6, 4), (6, 0)];
        // The middle horizontal run moves up.
        assert_eq!(
            move_segment(&wire, 1, (3, 2)),
            [(0, 0), (0, 6), (6, 6), (6, 0)]
        );
        // The first vertical run moves right and bends at the start pin.
        assert_eq!(
            move_segment(&wire, 0, (2, 0)),
            [(0, 0), (2, 0), (2, 4), (6, 4), (6, 0)]
        );
        // The last run moves and bends at the end pin.
        assert_eq!(
            move_segment(&wire, 2, (-1, 0)),
            [(0, 0), (0, 4), (5, 4), (5, 0), (6, 0)]
        );
        // Moving a straight wire sideways bends at both pins.
        assert_eq!(
            move_segment(&[(0, 0), (4, 0)], 0, (0, 2)),
            [(0, 0), (0, 2), (4, 2), (4, 0)]
        );
    }

    #[test]
    fn simplification_removes_collinear_corners() {
        assert_eq!(
            simplify(&[(0, 0), (2, 0), (4, 0), (4, 4)]),
            [(0, 0), (4, 0), (4, 4)]
        );
        assert_eq!(simplify(&[(0, 0), (0, 0), (4, 0)]), [(0, 0), (4, 0)]);
        assert_eq!(corners(&[(0, 0), (2, 0), (2, 2)]), [(2, 0)]);
    }
}
