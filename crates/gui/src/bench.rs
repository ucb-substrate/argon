//! A scripted benchmark of the layout canvas. `argone gui --bench CONFIG`
//! opens a cell, replays a camera path, drags a rectangle edge, writes frame
//! and edit timings as CSV, and exits.

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{App, Bounds, Global, Pixels, Point};
use web_time::Instant;

#[cfg(not(target_family = "wasm"))]
mod driver;

#[cfg(not(target_family = "wasm"))]
pub use driver::BenchConfig;
#[cfg(not(target_family = "wasm"))]
pub(crate) use driver::start;

/// One paint of the layout canvas.
#[derive(Clone, Debug)]
#[cfg_attr(
    target_family = "wasm",
    expect(dead_code, reason = "the benchmark driver is native only")
)]
pub(crate) struct PaintedFrame {
    pub(crate) paint_start: Instant,
    pub(crate) paint_end: Instant,
    /// When the window had submitted the frame to the GPU.
    pub(crate) presented: Instant,
    /// `direct`, `raster`, `retained` (an earlier direct frame), or `blank`.
    pub(crate) presentation: &'static str,
    /// Whether the frame draws the current content at the requested camera.
    pub(crate) complete: bool,
    pub(crate) scale: f32,
    pub(crate) offset: Point<Pixels>,
    pub(crate) canvas: Bounds<Pixels>,
    pub(crate) content_revision: u64,
    pub(crate) compilation_revision: Option<u64>,
    pub(crate) has_layout: bool,
    /// Whether a drag preview, rather than compiled geometry, places the
    /// dragged objects.
    pub(crate) sse_preview: bool,
    /// The selected rectangle as painted, in layout units.
    pub(crate) selected_rect: Option<[f32; 4]>,
}

/// Collects painted frames while a benchmark runs; absent otherwise.
pub(crate) struct FrameProbe(UnboundedSender<PaintedFrame>);

impl Global for FrameProbe {}

impl FrameProbe {
    #[cfg_attr(
        target_family = "wasm",
        expect(dead_code, reason = "the benchmark driver is native only")
    )]
    pub(crate) fn install(cx: &mut App) -> UnboundedReceiver<PaintedFrame> {
        let (sender, receiver) = unbounded();
        cx.set_global(Self(sender));
        receiver
    }

    pub(crate) fn active(cx: &App) -> bool {
        cx.has_global::<Self>()
    }

    /// Sends `frame` once the window has presented it.
    pub(crate) fn report(mut frame: PaintedFrame, cx: &mut App) {
        let Some(probe) = cx.try_global::<Self>() else {
            return;
        };
        let sender = probe.0.clone();
        // Deferred work runs after the window's draw and present return.
        cx.defer(move |_| {
            frame.presented = Instant::now();
            let _ = sender.unbounded_send(frame);
        });
    }
}
