//! A greedy router for layout on uniform metal tracks.
//!
//! [`GreedyRouter`] blocks and occupies grid points from existing geometry,
//! then routes one connection at a time on alternating-direction layers,
//! reporting the wires and vias each route needs. [`straps::fill`] then turns
//! the free track segments into power straps. [`autoroute::route`] routes
//! whole nets between pins while keeping each layer's spacing.

pub mod abs;
pub mod autoroute;
pub mod error;
pub mod geom;
pub mod router;
pub mod straps;
pub mod tracks;

pub use router::{
    ExpandToGridStrategy, GreedyRouter, Layer, LayerConfig, RouterConfig, Segment, TrackInfo,
    ViaRequest,
};

/// Indexing that returns an owned value.
pub trait IndexOwned<Idx> {
    type Output;

    fn index(&self, index: Idx) -> Self::Output;
}
