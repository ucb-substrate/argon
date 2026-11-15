//! Drives a live window through the benchmark script.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use argonc::compile::BasicRect;
use futures::{FutureExt, StreamExt, channel::mpsc::UnboundedReceiver};
use gpui::{
    App, AppContext, AsyncApp, Bounds, Entity, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point, Size, Window, WindowBounds,
    WindowHandle, WindowOptions, px, size,
};
use serde::Deserialize;
use web_time::Instant;

use super::{FrameProbe, PaintedFrame};
use crate::{
    editor::{EditorState, canvas::LayoutCanvas},
    main_window::MainWindow,
};

/// How long to wait for a frame before requesting another one.
const POLL: Duration = Duration::from_millis(250);
/// How long the editor must stay idle before the next step starts.
const QUIET: Duration = Duration::from_millis(500);
/// How far, in layout units, painted geometry may be from where it is expected.
const POSITION_TOLERANCE: f32 = 0.5;

/// What the benchmark opens, which views it shows, and where it writes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchConfig {
    /// The cell invocation to open.
    cell: String,
    /// A JSON camera path whose `frames` hold `{phase, x0, y0, x1, y1}` boxes.
    camera: PathBuf,
    /// Layout units per camera-path unit.
    camera_scale: f64,
    /// Prefix of the CSV files to write.
    out: PathBuf,
    /// Canvas width in logical pixels.
    width: f32,
    /// Canvas height in logical pixels.
    height: f32,
    /// Passes over the camera path; the first is a warm-up.
    repeats: usize,
    /// Timed edge drags, after one warm-up drag.
    edits: usize,
    /// Index of the camera-path view in which to edit.
    edit_frame: usize,
    /// Layer of the rectangle whose edge is dragged.
    edit_layer: String,
    /// Width and height of that rectangle, in layout units.
    edit_size: [f64; 2],
    /// Distance of each drag, in layout units.
    edit_step: f64,
    /// Longest wait for any one step, in seconds.
    timeout_s: f64,
}

impl BenchConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("could not read `{}`", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("invalid benchmark configuration `{}`", path.display()))
    }

    /// A focused window large enough for the canvas and the panels around it.
    pub(crate) fn window_options(&self) -> WindowOptions {
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                Point::default(),
                size(px(self.width + 600.), px(self.height + 80.)),
            ))),
            focus: true,
            ..crate::editor_window_options()
        }
    }
}

#[derive(Deserialize)]
struct CameraPath {
    frames: Vec<CameraView>,
}

#[derive(Deserialize)]
struct CameraView {
    phase: String,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl CameraView {
    /// The scale and offset that fit this box, centered, into `canvas`.
    fn camera(&self, units: f64, canvas: Size<Pixels>) -> (f32, Point<Pixels>) {
        let width = f64::from(f32::from(canvas.width));
        let height = f64::from(f32::from(canvas.height));
        let (x0, y0, x1, y1) = (
            self.x0 * units,
            self.y0 * units,
            self.x1 * units,
            self.y1 * units,
        );
        let scale = (width / (x1 - x0)).min(height / (y1 - y0));
        let offset = Point::new(
            px(((width - (x0 + x1) * scale) / 2.) as f32),
            px(((height + (y0 + y1) * scale) / 2.) as f32),
        );
        (scale as f32, offset)
    }

    fn center(&self, units: f64) -> (f64, f64) {
        (
            (self.x0 + self.x1) / 2. * units,
            (self.y0 + self.y1) / 2. * units,
        )
    }
}

/// The frames painted after one camera change, through the complete one.
struct Step {
    rep: usize,
    frame: usize,
    phase: String,
    t0: Instant,
    window_active: bool,
    frames: Vec<PaintedFrame>,
}

/// The frames painted after one edge drag, through the complete recompiled one.
struct EditRecord {
    edit: usize,
    t0: Instant,
    frames: Vec<PaintedFrame>,
    /// The first frame showing the dragged edge.
    sse: Option<usize>,
    /// The first frame painted from the recompiled snapshot.
    snapshot: Option<usize>,
    /// The rectangle once recompiled.
    rect: [f32; 4],
}

/// Runs `config` against `window` and exits the application when done.
pub(crate) fn start(config: BenchConfig, window: WindowHandle<MainWindow>, cx: &mut App) {
    let frames = FrameProbe::install(cx);
    cx.spawn(
        async move |cx| match run(config, window, frames, cx).await {
            Ok(()) => cx.update(|cx| cx.quit()),
            Err(error) => {
                eprintln!("argone bench: {error:#}");
                std::process::exit(1);
            }
        },
    )
    .detach();
}

async fn run(
    config: BenchConfig,
    window: WindowHandle<MainWindow>,
    frames: UnboundedReceiver<PaintedFrame>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let editor = window
        .update(cx, |main, _, _| main.editor().cloned())?
        .context("the GUI did not connect to the analyzer")?;
    let (canvas, state) = editor.read_with(cx, |editor, _| {
        (editor.canvas.clone(), editor.state.clone())
    });
    let mut bench = Bench {
        config,
        window,
        canvas,
        state,
        frames,
        scale: 1.,
        notes: Vec::new(),
        trace: std::env::var_os("ARGONE_BENCH_TRACE").is_some(),
    };
    bench.run(cx).await
}

struct Bench {
    config: BenchConfig,
    window: WindowHandle<MainWindow>,
    canvas: Entity<LayoutCanvas>,
    state: Entity<EditorState>,
    frames: UnboundedReceiver<PaintedFrame>,
    /// The current camera scale, in pixels per layout unit.
    scale: f32,
    /// Run facts written as `key,value` rows.
    notes: Vec<(&'static str, String)>,
    /// Whether to print every frame, from `ARGONE_BENCH_TRACE`.
    trace: bool,
}

impl Bench {
    async fn run(&mut self, cx: &mut AsyncApp) -> Result<()> {
        let timeout = Duration::from_secs_f64(self.config.timeout_s);
        let path: CameraPath = serde_json::from_str(
            &fs::read_to_string(&self.config.camera)
                .with_context(|| format!("could not read `{}`", self.config.camera.display()))?,
        )
        .context("invalid camera path")?;
        self.with_window(cx, |window, _| window.activate_window())?;

        self.wait_state(cx, "the analyzer", timeout, |state| state.cells.is_some())
            .await?;
        let opened = Instant::now();
        let cell = self.config.cell.clone();
        self.state.read_with(cx, |state, _| {
            state.lang_server_client.open_cell(cell);
        });
        let first = self
            .wait_frame(cx, "the layout", timeout, |frame| frame.has_layout)
            .await?;
        self.note("open_to_layout_s", seconds(first.presented - opened));
        let complete = self
            .wait_frame(cx, "a complete frame", timeout, |frame| {
                frame.has_layout && frame.complete
            })
            .await?;
        self.note("open_to_complete_s", seconds(complete.presented - opened));
        self.settle(cx, timeout).await?;
        self.note("open_to_idle_s", seconds(opened.elapsed()));
        eprintln!(
            "argone bench: opened {} in {:.2?}",
            self.config.cell,
            opened.elapsed()
        );

        let canvas_size = self.resize_canvas(cx, timeout).await?;
        let mut steps = Vec::new();
        for rep in 0..self.config.repeats {
            let started = Instant::now();
            for (index, view) in path.frames.iter().enumerate() {
                let (t0, window_active, frames) = self.show(cx, view, canvas_size, timeout).await?;
                steps.push(Step {
                    rep,
                    frame: index,
                    phase: view.phase.clone(),
                    t0,
                    window_active,
                    frames,
                });
            }
            eprintln!(
                "argone bench: pass {} of {} took {:.2?}",
                rep + 1,
                self.config.repeats,
                started.elapsed()
            );
        }

        let view = path
            .frames
            .get(self.config.edit_frame)
            .context("the camera path has no view to edit in")?;
        self.show(cx, view, canvas_size, timeout).await?;
        self.settle(cx, timeout).await?;
        let center = self.draw_rect(cx, view, timeout).await?;
        let mut rect = self.select(cx, center, timeout).await?;
        let mut edits = Vec::new();
        for edit in 0..=self.config.edits {
            let record = self.drag_right_edge(cx, edit, rect, timeout).await?;
            rect = record.rect;
            edits.push(record);
            self.settle(cx, timeout).await?;
        }
        eprintln!("argone bench: {} edits done", edits.len());

        let scale_factor = self.with_window(cx, |window, _| window.scale_factor())?;
        self.note("cell", self.config.cell.clone());
        self.note("canvas_width", f32::from(canvas_size.width).to_string());
        self.note("canvas_height", f32::from(canvas_size.height).to_string());
        self.note("scale_factor", scale_factor.to_string());
        self.note("edit_layer", self.config.edit_layer.clone());
        self.note("edit_rect", format!("{:?}", rect));
        self.write(&steps, &edits)
    }

    fn note(&mut self, key: &'static str, value: impl ToString) {
        self.notes.push((key, value.to_string()));
    }

    fn with_window<R>(
        &self,
        cx: &mut AsyncApp,
        update: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> Result<R> {
        cx.update_window(self.window.into(), |_, window, cx| update(window, cx))
    }

    fn notify(&self, cx: &mut AsyncApp) {
        self.canvas.update(cx, |_, cx| cx.notify());
    }

    fn drain(&mut self) {
        while self.frames.try_recv().is_ok() {}
    }

    /// The next painted frame. Requests a repaint while none arrives.
    async fn next_frame(
        &mut self,
        cx: &mut AsyncApp,
        until: Instant,
        what: &str,
    ) -> Result<PaintedFrame> {
        loop {
            if Instant::now() >= until {
                bail!("timed out waiting for {what}");
            }
            let timer = cx.background_executor().timer(POLL);
            let frame = {
                let mut next = self.frames.next().fuse();
                let mut timer = timer.fuse();
                futures::select_biased! {
                    frame = next => Some(frame),
                    () = timer => None,
                }
            };
            match frame {
                Some(Some(frame)) => {
                    if self.trace {
                        eprintln!(
                            "argone bench: {what}: {} complete={} scale={} offset={:?} canvas={:?} revision={} layout={} preview={} rect={:?}",
                            frame.presentation,
                            frame.complete,
                            frame.scale,
                            frame.offset,
                            frame.canvas.size,
                            frame.content_revision,
                            frame.has_layout,
                            frame.sse_preview,
                            frame.selected_rect,
                        );
                    }
                    return Ok(frame);
                }
                Some(None) => bail!("the frame probe closed"),
                None => self.notify(cx),
            }
        }
    }

    /// The frames painted after `t0`, through the first one `done` accepts.
    /// Requests a repaint after each other frame, as continuous input would.
    async fn frames_until(
        &mut self,
        cx: &mut AsyncApp,
        t0: Instant,
        what: &str,
        timeout: Duration,
        mut done: impl FnMut(&PaintedFrame) -> bool,
    ) -> Result<Vec<PaintedFrame>> {
        let until = t0 + timeout;
        let mut frames = Vec::new();
        loop {
            let frame = self.next_frame(cx, until, what).await?;
            if frame.paint_start < t0 {
                continue;
            }
            let finished = done(&frame);
            frames.push(frame);
            if finished {
                return Ok(frames);
            }
            self.notify(cx);
        }
    }

    async fn wait_frame(
        &mut self,
        cx: &mut AsyncApp,
        what: &str,
        timeout: Duration,
        done: impl FnMut(&PaintedFrame) -> bool,
    ) -> Result<PaintedFrame> {
        let mut frames = self
            .frames_until(cx, Instant::now(), what, timeout, done)
            .await?;
        Ok(frames.pop().expect("at least the accepted frame"))
    }

    async fn wait_state(
        &mut self,
        cx: &mut AsyncApp,
        what: &str,
        timeout: Duration,
        ready: impl Fn(&EditorState) -> bool,
    ) -> Result<()> {
        let until = Instant::now() + timeout;
        while !self.state.read_with(cx, |state, _| ready(state)) {
            if Instant::now() >= until {
                bail!("timed out waiting for {what}");
            }
            cx.background_executor()
                .timer(Duration::from_millis(20))
                .await;
        }
        Ok(())
    }

    /// Waits until no compilation, snapshot preparation, or render has been
    /// in progress for [`QUIET`].
    async fn settle(&mut self, cx: &mut AsyncApp, timeout: Duration) -> Result<()> {
        let until = Instant::now() + timeout;
        let mut idle_since = None;
        loop {
            let now = Instant::now();
            if self.state.read_with(cx, |state, _| state.is_busy()) {
                idle_since = None;
            } else if now - *idle_since.get_or_insert(now) >= QUIET {
                break;
            }
            if now >= until {
                bail!("the editor was still busy after {timeout:?}");
            }
            cx.background_executor()
                .timer(Duration::from_millis(20))
                .await;
        }
        self.drain();
        Ok(())
    }

    /// Resizes the window so that the canvas has the configured size. A
    /// titled window cannot outgrow the screen, so the window first becomes
    /// borderless, which may then extend past the screen's edges.
    async fn resize_canvas(
        &mut self,
        cx: &mut AsyncApp,
        timeout: Duration,
    ) -> Result<Size<Pixels>> {
        let wanted = size(px(self.config.width), px(self.config.height));
        if !self.with_window(cx, |window, _| window.is_simple_fullscreen())? {
            self.with_window(cx, |window, _| window.toggle_simple_fullscreen())?;
            self.wait_frame(cx, "the borderless window", timeout, |frame| frame.complete)
                .await?;
            self.settle(cx, timeout).await?;
        }
        let current = self
            .canvas
            .read_with(cx, |canvas, _| canvas.screen_bounds().size);
        if current != wanted {
            self.with_window(cx, |window, _| {
                let viewport = window.viewport_size();
                window.resize(size(
                    viewport.width + wanted.width - current.width,
                    viewport.height + wanted.height - current.height,
                ));
            })?;
            let resized = self
                .wait_frame(cx, "the resized canvas", Duration::from_secs(10), |frame| {
                    frame.canvas.size == wanted
                })
                .await;
            if resized.is_err() {
                let actual = self
                    .canvas
                    .read_with(cx, |canvas, _| canvas.screen_bounds().size);
                bail!("the canvas is {actual:?}, not {wanted:?}; is the screen large enough?");
            }
        }
        self.settle(cx, timeout).await?;
        Ok(wanted)
    }

    /// Moves the camera to `view` and collects frames until one is complete.
    /// Returns when the camera moved, whether the window was active, and the
    /// frames.
    async fn show(
        &mut self,
        cx: &mut AsyncApp,
        view: &CameraView,
        canvas_size: Size<Pixels>,
        timeout: Duration,
    ) -> Result<(Instant, bool, Vec<PaintedFrame>)> {
        let (scale, offset) = view.camera(self.config.camera_scale, canvas_size);
        self.scale = scale;
        self.drain();
        let window_active = self.with_window(cx, |window, _| window.is_window_active())?;
        let t0 = Instant::now();
        self.canvas
            .update(cx, |canvas, cx| canvas.set_camera(scale, offset, cx));
        let frames = self
            .frames_until(cx, t0, "a complete frame", timeout, |frame| {
                frame.complete && frame.scale == scale && frame.offset == offset
            })
            .await?;
        Ok((t0, window_active, frames))
    }

    /// Draws the rectangle to edit at the center of `view`, through the same
    /// request as the rectangle tool, and waits for it to compile. Returns its
    /// center.
    async fn draw_rect(
        &mut self,
        cx: &mut AsyncApp,
        view: &CameraView,
        timeout: Duration,
    ) -> Result<Point<f32>> {
        let (scope, grid, client) = self
            .state
            .read_with(cx, |state, cx| {
                let solved = state.solved_cell.read(cx).clone()?;
                let cell = &solved.output.cells[&solved.selected_scope.cell];
                Some((
                    cell.scope_span(solved.selected_scope.scope).clone(),
                    solved.output.tech.grid_step(),
                    state.lang_server_client.clone(),
                ))
            })
            .context("no layout is open")?;
        let snap = |value: f64| (value / grid).round() * grid;
        let (x, y) = view.center(self.config.camera_scale);
        let (x, y) = (snap(x), snap(y));
        let [width, height] = self.config.edit_size;
        let rect = BasicRect {
            layer: Some(self.config.edit_layer.clone()),
            x0: snap(x - width / 2.),
            y0: snap(y - height / 2.),
            x1: snap(x + width / 2.),
            y1: snap(y + height / 2.),
            construction: false,
        };
        let receipt = client
            .draw_rect(scope, "bench_rect".to_owned(), rect)
            .await?
            .context("the analyzer did not insert the rectangle")?;
        self.wait_frame(cx, "the compiled rectangle", timeout, |frame| {
            frame.complete
                && frame
                    .compilation_revision
                    .is_some_and(|revision| revision >= receipt.revision)
        })
        .await?;
        self.settle(cx, timeout).await?;
        Ok(Point::new(x as f32, y as f32))
    }

    /// Selects the rectangle at `center` with a click and returns it.
    async fn select(
        &mut self,
        cx: &mut AsyncApp,
        center: Point<f32>,
        timeout: Duration,
    ) -> Result<[f32; 4]> {
        let position = self
            .canvas
            .read_with(cx, |canvas, _| canvas.layout_to_px(center));
        self.mouse(cx, mouse_down(position))?;
        self.mouse(cx, mouse_up(position))?;
        let frame = self
            .wait_frame(cx, "the selected rectangle", timeout, |frame| {
                frame.selected_rect.is_some()
            })
            .await?;
        self.settle(cx, timeout).await?;
        Ok(frame.selected_rect.expect("checked above"))
    }

    /// Drags the right edge of `rect` by the configured step: a press on its
    /// handle, then, at `t0`, a move and a release.
    async fn drag_right_edge(
        &mut self,
        cx: &mut AsyncApp,
        edit: usize,
        rect: [f32; 4],
        timeout: Duration,
    ) -> Result<EditRecord> {
        let [x0, y0, x1, y1] = rect;
        let handle = self.canvas.read_with(cx, |canvas, _| {
            canvas.layout_to_px(Point::new(x1, (y0 + y1) / 2.))
        });
        let step = self.config.edit_step as f32;
        let target = Point::new(handle.x + px(step * self.scale), handle.y);
        let expected = x1 + step;
        let pressed = Instant::now();
        self.mouse(cx, mouse_down(handle))?;
        self.notify(cx);
        self.frames_until(cx, pressed, "the pressed handle", timeout, |_| true)
            .await?;
        let revision = self
            .state
            .read_with(cx, |state, _| state.compilation_revision);
        self.drain();
        let t0 = Instant::now();
        self.mouse(
            cx,
            PlatformInput::MouseMove(MouseMoveEvent {
                position: target,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            }),
        )?;
        self.mouse(cx, mouse_up(target))?;
        let moved = |frame: &PaintedFrame| {
            frame.selected_rect.is_some_and(|[left, _, right, _]| {
                (left - x0).abs() < POSITION_TOLERANCE
                    && (right - expected).abs() < POSITION_TOLERANCE
            })
        };
        let frames = self
            .frames_until(cx, t0, "the recompiled edit", timeout, |frame| {
                moved(frame) && !frame.sse_preview && frame.complete
            })
            .await?;
        let sse = frames.iter().position(moved);
        let snapshot = frames
            .iter()
            .position(|frame| frame.compilation_revision > revision);
        let rect = frames
            .last()
            .and_then(|frame| frame.selected_rect)
            .expect("the accepted frame shows the rectangle");
        Ok(EditRecord {
            edit,
            t0,
            frames,
            sse,
            snapshot,
            rect,
        })
    }

    fn mouse(&self, cx: &mut AsyncApp, input: PlatformInput) -> Result<()> {
        self.with_window(cx, |window, cx| {
            window.dispatch_event(input, cx);
        })
    }

    fn write(&self, steps: &[Step], edits: &[EditRecord]) -> Result<()> {
        if let Some(parent) = self.config.out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut frames = String::from(
            "rep,frame,phase,complete_s,first_present_s,presents,max_interval_s,presentation,window_active\n",
        );
        let mut presents = String::from(
            "rep,frame,present,paint_start_s,paint_ms,presented_s,presentation,complete,content_revision\n",
        );
        for step in steps {
            let mut previous = step.t0;
            let mut max_interval = Duration::ZERO;
            for (index, frame) in step.frames.iter().enumerate() {
                max_interval = max_interval.max(frame.presented - previous);
                previous = frame.presented;
                writeln!(
                    presents,
                    "{},{},{},{:.6},{:.3},{:.6},{},{},{}",
                    step.rep,
                    step.frame,
                    index,
                    seconds(frame.paint_start - step.t0),
                    1e3 * seconds(frame.paint_end - frame.paint_start),
                    seconds(frame.presented - step.t0),
                    frame.presentation,
                    frame.complete,
                    frame.content_revision,
                )?;
            }
            let (Some(first), Some(last)) = (step.frames.first(), step.frames.last()) else {
                continue;
            };
            writeln!(
                frames,
                "{},{},{},{:.6},{:.6},{},{:.6},{},{}",
                step.rep,
                step.frame,
                step.phase,
                seconds(last.presented - step.t0),
                seconds(first.presented - step.t0),
                step.frames.len(),
                seconds(max_interval),
                last.presentation,
                step.window_active,
            )?;
        }

        let mut edit_rows = String::from("edit,sse_s,snapshot_s,complete_s,presents,x1\n");
        let mut edit_presents = String::from(
            "edit,present,paint_start_s,paint_ms,presented_s,presentation,complete,sse_preview,compilation_revision,x1\n",
        );
        for edit in edits {
            let since = |index: Option<usize>| {
                index.map_or(String::new(), |index| {
                    format!("{:.6}", seconds(edit.frames[index].presented - edit.t0))
                })
            };
            writeln!(
                edit_rows,
                "{},{},{},{},{},{}",
                edit.edit,
                since(edit.sse),
                since(edit.snapshot),
                since(Some(edit.frames.len() - 1)),
                edit.frames.len(),
                edit.rect[2],
            )?;
            for (index, frame) in edit.frames.iter().enumerate() {
                writeln!(
                    edit_presents,
                    "{},{},{:.6},{:.3},{:.6},{},{},{},{},{}",
                    edit.edit,
                    index,
                    seconds(frame.paint_start - edit.t0),
                    1e3 * seconds(frame.paint_end - frame.paint_start),
                    seconds(frame.presented - edit.t0),
                    frame.presentation,
                    frame.complete,
                    frame.sse_preview,
                    frame
                        .compilation_revision
                        .map_or(String::new(), |revision| revision.to_string()),
                    frame
                        .selected_rect
                        .map_or(String::new(), |rect| rect[2].to_string()),
                )?;
            }
        }

        let mut notes = String::from("key,value\n");
        for (key, value) in &self.notes {
            writeln!(notes, "{key},\"{value}\"")?;
        }

        for (suffix, text) in [
            ("frames", frames),
            ("presents", presents),
            ("edits", edit_rows),
            ("edit_presents", edit_presents),
            ("run", notes),
        ] {
            let path = PathBuf::from(format!("{}_{suffix}.csv", self.config.out.display()));
            fs::write(&path, text)
                .with_context(|| format!("could not write `{}`", path.display()))?;
        }
        Ok(())
    }
}

fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

fn mouse_down(position: Point<Pixels>) -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button: MouseButton::Left,
        position,
        modifiers: Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    })
}

fn mouse_up(position: Point<Pixels>) -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button: MouseButton::Left,
        position,
        modifiers: Modifiers::default(),
        click_count: 1,
    })
}
