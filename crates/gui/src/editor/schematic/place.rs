//! Auto-placement of elements and ports that the source does not place.

use argonc::compile::BoxSide;

/// The columns of the slot grid that unplaced elements go to.
pub const COLUMNS: usize = 8;

/// The pitch of the slot grid for symbols no larger than `largest` grid
/// units: a multiple of 4 that fits the largest symbol with a margin.
pub fn slot_pitch(largest: f64) -> i64 {
    let pitch = (largest.max(0.) + 2.).ceil() as i64;
    ((pitch + 3) / 4 * 4).max(4)
}

/// The origin of slot `k`: slots fill rows of [`COLUMNS`] from the origin,
/// rightward and then downward.
pub fn slot_position(k: usize, pitch: i64) -> (i64, i64) {
    let column = (k % COLUMNS) as i64;
    let row = (k / COLUMNS) as i64;
    (column * pitch, -row * pitch)
}

/// The number of rows `count` slots fill.
pub fn slot_rows(count: usize) -> usize {
    count.div_ceil(COLUMNS)
}

/// Where an unplaced port goes: fixed columns beside the slot grid, by the
/// generated box's side rule. `index` counts the ports on the same side, in
/// port order; `pitch` and `rows` describe the slot grid.
pub fn port_position(side: BoxSide, index: usize, pitch: i64, rows: usize) -> (i64, i64) {
    let width = COLUMNS as i64 * pitch;
    let height = rows.max(1) as i64 * pitch;
    let step = 2 * index as i64;
    match side {
        BoxSide::Left => (-pitch, -step),
        BoxSide::Right => (width, -step),
        BoxSide::Top => (step, pitch),
        BoxSide::Bottom => (step, -height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pitch_is_a_multiple_of_four_that_fits_the_largest_symbol() {
        assert_eq!(slot_pitch(0.), 4);
        assert_eq!(slot_pitch(4.), 8);
        assert_eq!(slot_pitch(5.5), 8);
        assert_eq!(slot_pitch(6.5), 12);
    }

    #[test]
    fn slots_fill_rows_of_eight() {
        assert_eq!(slot_position(0, 8), (0, 0));
        assert_eq!(slot_position(7, 8), (56, 0));
        assert_eq!(slot_position(8, 8), (0, -8));
        assert_eq!(slot_rows(0), 0);
        assert_eq!(slot_rows(9), 2);
    }

    #[test]
    fn ports_line_the_sides_of_the_grid() {
        assert_eq!(port_position(BoxSide::Left, 1, 8, 1), (-8, -2));
        assert_eq!(port_position(BoxSide::Right, 0, 8, 1), (64, 0));
        assert_eq!(port_position(BoxSide::Top, 2, 8, 1), (4, 8));
        assert_eq!(port_position(BoxSide::Bottom, 0, 8, 2), (0, -16));
    }
}
