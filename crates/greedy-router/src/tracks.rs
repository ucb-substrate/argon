//! Track management, including sizing and spacing.

use crate::IndexOwned;
use crate::geom::{Sign, Span};

/// Specifier for a track relative to a position.
///
/// If the position is already on the appropriate edge of a track, that track is specified.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum TrackLocator {
    /// The track nearest a position.
    Nearest,
    /// The track nearest a position that starts before that position.
    StartsBefore,
    /// The track nearest a position that starts beyond that position.
    StartsAfter,
    /// The track nearest a position that ends before that position.
    EndsBefore,
    /// The track nearest a position that ends beyond that position.
    EndsAfter,
}

/// An infinite number of tracks, starting at a fixed location.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UniformTracks {
    pub line: i64,
    pub space: i64,
    pub start: i64,
    pub sign: Sign,
}

impl UniformTracks {
    pub fn track_at(&self, pos: i64) -> i64 {
        (pos - self.start) / (self.line + self.space)
    }

    pub fn track_with_loc(&self, loc: TrackLocator, pos: i64) -> i64 {
        match loc {
            TrackLocator::Nearest => {
                let before = self.track_with_loc(TrackLocator::StartsBefore, pos);
                let after = self.track_with_loc(TrackLocator::EndsAfter, pos);

                if self.index(after).distance_to(pos) < self.index(before).distance_to(pos) {
                    after
                } else {
                    before
                }
            }
            TrackLocator::StartsBefore => self.track_at(pos),
            TrackLocator::StartsAfter => (pos - self.start - 1) / (self.line + self.space) + 1,
            TrackLocator::EndsBefore => {
                self.track_with_loc(TrackLocator::StartsBefore, pos - self.line)
            }
            TrackLocator::EndsAfter => {
                self.track_with_loc(TrackLocator::StartsAfter, pos - self.line)
            }
        }
    }
}

impl IndexOwned<i64> for UniformTracks {
    type Output = Span;
    fn index(&self, index: i64) -> Self::Output {
        let sgn = self.sign.as_int();
        let start = self.start + sgn * index * (self.line + self.space);
        let stop = start + sgn * self.line;
        Span::new(start, stop)
    }
}

impl IndexOwned<usize> for UniformTracks {
    type Output = Span;
    fn index(&self, index: usize) -> Self::Output {
        let index = i64::try_from(index).expect("index must be at most `i64::MAX`");
        self.index(index)
    }
}
