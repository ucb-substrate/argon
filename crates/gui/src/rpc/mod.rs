//! The GUI's side of the RPC services it shares with the analyzer.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
pub mod web;

use std::{
    fmt::Display,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use argonc::{
    ast::Span,
    compile::{BasicRect, CellId},
};
use futures::{
    channel::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    prelude::*,
};
use gpui::{AsyncApp, Task};
use ls_types::MessageType;
use protocol::config::ArgonConfig;
use protocol::rpc::{
    AddSignalRequest, AddSymbolItemsRequest, CallArguments, CellEntry, ClipboardItem,
    CompilationSnapshot, CompressedCompilationUpdate, ConnectRequest, CreateSymbolRequest,
    DeleteOutcome, DeleteRequest, DevicePreview, DimensionParams, EditReceipt, EditorCommand,
    FocusEditorParams, FocusedCellPreview, GuiRequest, GuiResponse, GuiUpdateResult,
    InitialConditionEdit, InstancePreview, LangServerAction, LangServerRequest, LangServerResponse,
    PasteRequest, PathParams, PlaceDeviceRequest, PlaceRequest, PlaceSchematicInstanceRequest,
    PolygonParams, RecentCell, RectangleEditResult, RenameRequest, SchematicInstancePreview,
    SetArgumentRequest, ValueEdit,
};
use tracing::error;
use web_time::Instant;

#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use native::TestLangServerTransport;
#[cfg(not(target_family = "wasm"))]
pub(crate) use native::{Channel, Endpoint};
#[cfg(target_family = "wasm")]
pub(crate) use web::{Channel, Endpoint};

use crate::editor::{Editor, prepare_compilation_snapshot};

pub const LANG_SERVER_CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_SNAPSHOT_PREPARATION_ID: AtomicU64 = AtomicU64::new(1);

fn lock_unpoisoned<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        // This lock only protects cloning or replacing a complete RPC client,
        // so the contained value remains usable after a panicking caller.
        lock.clear_poison();
        poisoned.into_inner()
    })
}

/// Why a request got no response.
pub(crate) enum CallError {
    /// The connection is gone, so the request may be resent on a new one.
    Disconnected(anyhow::Error),
    Timeout,
    Failed(anyhow::Error),
}

/// Sends `$variant` and resolves to the value of its response.
macro_rules! request {
    ($client:expr, $variant:ident { $($field:ident $(: $value:expr)?),* $(,)? }) => {
        $client.request(
            LangServerRequest::$variant { $($field $(: $value)?),* },
            |response| match response {
                LangServerResponse::$variant(value) => Some(value),
                _ => None,
            },
        )
    };
}

/// Sends `$variant` after every request sent this way before it, without
/// waiting for its response.
macro_rules! send {
    ($client:expr, $variant:ident { $($field:ident $(: $value:expr)?),* $(,)? }) => {
        $client.send(LangServerRequest::$variant { $($field $(: $value)?),* })
    };
}

/// The GUI's connection to the analyzer.
///
/// Requests never block the UI thread. Calls whose results are awaited run
/// as tasks; calls whose results nobody waits for run one at a time, in the
/// order they were made.
#[derive(Clone)]
pub struct SyncLangServerClient {
    inner: Rc<ClientInner>,
    ordered: UnboundedSender<LangServerRequest>,
}

struct ClientInner {
    app: AsyncApp,
    /// Where to connect, or `None` for a test client that never connects.
    endpoint: Option<Endpoint>,
    /// The connected channel, or `None` until the first call connects.
    channel: Mutex<Option<Channel>>,
    to_exec: UnboundedSender<EditorFn>,
}

impl SyncLangServerClient {
    fn with_channel(
        app: AsyncApp,
        endpoint: Option<Endpoint>,
        channel: Option<Channel>,
    ) -> (Self, UnboundedReceiver<EditorFn>) {
        let (to_exec, rx) = mpsc::unbounded();
        let inner = Rc::new(ClientInner {
            app: app.clone(),
            endpoint,
            channel: Mutex::new(channel),
            to_exec,
        });
        let (ordered, mut requests) = mpsc::unbounded::<LangServerRequest>();
        app.spawn({
            let inner = inner.clone();
            async move |_| {
                while let Some(request) = requests.next().await {
                    let _ = inner.request(request, |_| Some(())).await;
                }
            }
        })
        .detach();
        (Self { inner, ordered }, rx)
    }

    /// Disconnected client for rendering tests that never issue source edits.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn for_render_test(app: AsyncApp) -> Self {
        let (transport, _) = tarpc::transport::channel::unbounded();
        let channel = Channel::from_transport(&app, transport);
        Self::with_channel(app, None, Some(channel)).0
    }

    /// Real client dispatch over an in-memory transport, with replies controlled
    /// by the rendering test. This exercises yielding while an edit is pending.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn for_rpc_test(app: AsyncApp) -> (Self, TestLangServerTransport) {
        let (transport, server) = tarpc::transport::channel::unbounded();
        let channel = Channel::from_transport(&app, transport);
        (Self::with_channel(app, None, Some(channel)).0, server)
    }

    /// A client for the analyzer at `endpoint`, which connects on its first
    /// call.
    pub(crate) fn new(app: AsyncApp, endpoint: Endpoint) -> (Self, UnboundedReceiver<EditorFn>) {
        Self::with_channel(app, Some(endpoint), None)
    }

    /// Sends `request` and resolves to the response `extract` takes from it.
    pub(crate) async fn request<T>(
        &self,
        request: LangServerRequest,
        extract: fn(LangServerResponse) -> Option<T>,
    ) -> Result<T> {
        self.inner.request(request, extract).await
    }

    /// Sends `request` after every request sent this way before it.
    fn send(&self, request: LangServerRequest) {
        let _ = self.ordered.unbounded_send(request);
    }

    fn report_message(&self, typ: MessageType, message: String) {
        self.inner.report_message(typ, message);
    }

    /// Opens the connection the analyzer calls the GUI back over and serves
    /// it until the returned task is dropped.
    pub fn serve_callbacks(&self) -> Task<()> {
        let inner = self.inner.clone();
        self.inner.app.spawn(async move |_| {
            let server = GuiServer {
                to_exec: inner.to_exec.clone(),
                snapshot: Arc::default(),
            };
            let served = match &inner.endpoint {
                Some(endpoint) => Channel::serve_callbacks(&inner.app, endpoint, server).await,
                None => Err(anyhow!("no analyzer to connect to")),
            };
            match served {
                Ok(server) => {
                    inner.report_connection_result(&Ok(()));
                    server.await;
                }
                Err(error) => {
                    error!("Failed to connect the GUI to the analyzer: {error}");
                    inner.report_connection_result::<()>(&Err(error));
                }
            }
        })
    }

    /// Reveals `span` in the editor.
    pub fn select_span(&self, span: Span) {
        send!(self, SelectSpan { span });
    }

    pub async fn draw_rect(
        &self,
        scope_span: Span,
        var_name: String,
        rect: BasicRect<f64>,
    ) -> Result<Option<RectangleEditResult>> {
        request!(
            self,
            DrawRect {
                scope_span,
                var_name,
                rect
            }
        )
        .await
    }

    pub async fn draw_polygon(
        &self,
        scope_span: Span,
        var_name: String,
        polygon: PolygonParams,
    ) -> Result<Option<Span>> {
        request!(
            self,
            DrawPolygon {
                scope_span,
                var_name,
                polygon
            }
        )
        .await
    }

    pub async fn draw_path(
        &self,
        scope_span: Span,
        var_name: String,
        path: PathParams,
    ) -> Result<Option<Span>> {
        request!(
            self,
            DrawPath {
                scope_span,
                var_name,
                path
            }
        )
        .await
    }

    pub async fn place_instance(
        &self,
        scope_span: Span,
        invocation: String,
        x: f64,
        y: f64,
    ) -> Result<Option<Span>> {
        request!(
            self,
            PlaceInstance {
                scope_span,
                invocation,
                x,
                y
            }
        )
        .await
    }

    pub async fn draw_dimension(
        &self,
        scope_span: Span,
        params: DimensionParams,
    ) -> Result<Option<Span>> {
        request!(self, DrawDimension { scope_span, params }).await
    }

    pub async fn edit_dimension(&self, span: Span, value: String) -> Result<Option<Span>> {
        request!(self, EditDimension { span, value }).await
    }

    pub async fn update_values_async(
        &self,
        edits: Vec<ValueEdit>,
        initial_conditions: Vec<InitialConditionEdit>,
        base_revision: Option<u64>,
    ) -> Result<Option<Vec<ValueEdit>>> {
        request!(
            self,
            UpdateValues {
                edits,
                initial_conditions,
                base_revision
            }
        )
        .await
    }

    pub fn dispatch_sse_source_edit_result(
        &self,
        generation: u64,
        result: Result<Option<Vec<ValueEdit>>>,
        initial_conditions: Vec<InitialConditionEdit>,
    ) {
        let _ = self
            .inner
            .to_exec
            .unbounded_send(Box::new(move |editor, cx| {
                cx.update(|cx| {
                    editor.finish_sse_source_edit(cx, generation, result, initial_conditions)
                });
            }));
    }

    /// Applies a schematic placement or route gesture, off the UI thread.
    pub async fn place(&self, request: PlaceRequest) -> Result<Option<EditReceipt>> {
        request!(self, Place { request }).await
    }

    pub async fn add_signal(&self, request: AddSignalRequest) -> Result<Option<EditReceipt>> {
        request!(self, AddSignal { request }).await
    }

    pub async fn place_schematic_instance(
        &self,
        request: PlaceSchematicInstanceRequest,
    ) -> Result<Option<EditReceipt>> {
        request!(self, PlaceSchematicInstance { request }).await
    }

    pub async fn place_device(&self, request: PlaceDeviceRequest) -> Result<Option<EditReceipt>> {
        request!(self, PlaceDevice { request }).await
    }

    pub async fn connect(&self, request: ConnectRequest) -> Result<Option<EditReceipt>> {
        request!(self, Connect { request }).await
    }

    pub async fn delete(&self, request: DeleteRequest) -> Result<Option<DeleteOutcome>> {
        request!(self, Delete { request }).await
    }

    pub async fn set_argument(&self, request: SetArgumentRequest) -> Result<Option<EditReceipt>> {
        request!(self, SetArgument { request }).await
    }

    pub async fn add_symbol_items(
        &self,
        request: AddSymbolItemsRequest,
    ) -> Result<Option<EditReceipt>> {
        request!(self, AddSymbolItems { request }).await
    }

    pub async fn create_symbol(&self, request: CreateSymbolRequest) -> Result<Option<EditReceipt>> {
        request!(self, CreateSymbol { request }).await
    }

    pub async fn rename(&self, request: RenameRequest) -> Result<Option<EditReceipt>> {
        request!(self, Rename { request }).await
    }

    pub async fn copy_statements(
        &self,
        base_revision: u64,
        targets: Vec<Span>,
    ) -> Result<Option<Vec<ClipboardItem>>> {
        request!(
            self,
            CopyStatements {
                base_revision,
                targets
            }
        )
        .await
    }

    pub async fn paste(&self, request: PasteRequest) -> Result<Option<EditReceipt>> {
        request!(self, Paste { request }).await
    }

    pub async fn cell_parameters(
        &self,
        base_revision: u64,
        cell: Span,
    ) -> Result<Option<Vec<String>>> {
        request!(
            self,
            CellParameters {
                base_revision,
                cell
            }
        )
        .await
    }

    pub async fn call_arguments(
        &self,
        base_revision: u64,
        call: Span,
    ) -> Result<Option<CallArguments>> {
        request!(
            self,
            CallArguments {
                base_revision,
                call
            }
        )
        .await
    }

    pub fn add_eq_constraint(&self, scope_span: Span, lhs: String, rhs: String) {
        send!(
            self,
            AddEqConstraint {
                scope_span,
                lhs,
                rhs
            }
        );
    }

    pub fn show_message<M: Display>(&self, typ: MessageType, message: M) {
        let message = message.to_string();
        self.report_message(typ, message.clone());
        send!(self, ShowMessage { typ, message });
    }

    pub fn dispatch_action(&self, action: LangServerAction) {
        send!(self, DispatchAction { action });
    }

    /// Opens the cell invocation `cell`, such as `inverter()`.
    pub fn open_cell(&self, cell: String) {
        send!(self, OpenCell { cell });
    }

    pub fn open_command_bar(&self, command: Option<EditorCommand>, return_to_gui: bool) {
        send!(
            self,
            FocusEditor {
                params: FocusEditorParams {
                    command,
                    return_to_gui,
                },
            }
        );
    }
}

impl ClientInner {
    /// The connected channel, connecting first if there is none.
    async fn channel(&self) -> Result<Channel> {
        if let Some(channel) = lock_unpoisoned(&self.channel).clone() {
            return Ok(channel);
        }
        self.reconnect().await
    }

    async fn reconnect(&self) -> Result<Channel> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow!("no analyzer to connect to"))?;
        let channel = Channel::connect(&self.app, endpoint).await?;
        *lock_unpoisoned(&self.channel) = Some(channel.clone());
        Ok(channel)
    }

    async fn request<T>(
        &self,
        request: LangServerRequest,
        extract: fn(LangServerResponse) -> Option<T>,
    ) -> Result<T> {
        let result = match self.channel().await {
            Ok(channel) => channel.call(&self.app, request.clone()).await,
            Err(error) => {
                let result = Err(error);
                self.report_connection_result(&result);
                return result;
            }
        };
        let result = match result {
            Err(CallError::Disconnected(_)) => match self.reconnect().await {
                Ok(channel) => channel.call(&self.app, request).await,
                Err(error) => {
                    let result = Err(error);
                    self.report_connection_result(&result);
                    return result;
                }
            },
            result => result,
        };
        let result = match result {
            Ok(response) => {
                extract(response).ok_or_else(|| anyhow!("the analyzer answered another request"))
            }
            Err(CallError::Disconnected(error) | CallError::Failed(error)) => Err(error),
            Err(CallError::Timeout) => Err(anyhow!(
                "timeout reaching language server after {LANG_SERVER_CLIENT_TIMEOUT:?}"
            )),
        };
        self.report_connection_result(&result);
        result
    }

    fn report_connection_result<T>(&self, result: &Result<T>) {
        let error = result.as_ref().err().map(ToString::to_string);
        let _ = self.to_exec.unbounded_send(Box::new(move |editor, cx| {
            editor.state.update(cx, |state, cx| {
                state.connection_error = error.map(Into::into);
                cx.notify();
            });
        }));
    }

    fn report_message(&self, typ: MessageType, message: String) {
        if typ == MessageType::LOG {
            return;
        }
        let _ = self.to_exec.unbounded_send(Box::new(move |editor, cx| {
            editor.state.update(cx, |state, cx| {
                state.show_message(typ, message);
                cx.notify();
            });
        }));
    }
}

type EditorFn = Box<dyn FnOnce(&Editor, &mut AsyncApp) + Send>;

/// Answers the analyzer's calls by running them against the editor.
#[derive(Clone)]
pub struct GuiServer {
    to_exec: UnboundedSender<EditorFn>,
    snapshot: Arc<Mutex<Option<CompilationSnapshot>>>,
}

impl GuiServer {
    /// Answers one request from the analyzer.
    pub(crate) async fn handle(self, request: GuiRequest) -> GuiResponse {
        match request {
            GuiRequest::CompilationStarted { activity_id } => {
                self.compilation_started(activity_id).await;
                GuiResponse::CompilationStarted(())
            }
            GuiRequest::CompilationFinished { activity_id } => {
                self.compilation_finished(activity_id).await;
                GuiResponse::CompilationFinished(())
            }
            GuiRequest::UpdateCell { update } => {
                GuiResponse::UpdateCell(self.update_cell(update).await)
            }
            GuiRequest::PreviewCell { preview } => {
                GuiResponse::PreviewCell(self.preview_cell(preview).await)
            }
            GuiRequest::Fit {} => {
                self.fit().await;
                GuiResponse::Fit(())
            }
            GuiRequest::SetWorkspacePath { path } => {
                self.set_workspace_path(path).await;
                GuiResponse::SetWorkspacePath(())
            }
            GuiRequest::WorkspaceModified { modified } => {
                self.workspace_modified(modified).await;
                GuiResponse::WorkspaceModified(())
            }
            GuiRequest::ShowMessage { typ, message } => {
                self.show_message(typ, message).await;
                GuiResponse::ShowMessage(())
            }
            GuiRequest::SelectedScope {} => GuiResponse::SelectedScope(self.selected_scope().await),
            GuiRequest::SelectedCell {} => GuiResponse::SelectedCell(self.selected_cell().await),
            GuiRequest::PlaceInstance { preview } => {
                self.place_instance(preview).await;
                GuiResponse::PlaceInstance(())
            }
            GuiRequest::SchematicTarget {} => {
                GuiResponse::SchematicTarget(self.schematic_target().await)
            }
            GuiRequest::PlaceSchematicInstance { preview } => {
                self.place_schematic_instance(preview).await;
                GuiResponse::PlaceSchematicInstance(())
            }
            GuiRequest::PlaceDevice { preview } => {
                self.place_device(preview).await;
                GuiResponse::PlaceDevice(())
            }
            GuiRequest::Configure { config } => {
                self.configure(config).await;
                GuiResponse::Configure(())
            }
            GuiRequest::HighlightSpans { revision, spans } => {
                self.highlight_spans(revision, spans).await;
                GuiResponse::HighlightSpans(())
            }
            GuiRequest::AgentActivity { label } => {
                self.agent_activity(label).await;
                GuiResponse::AgentActivity(())
            }
            GuiRequest::Activate {} => {
                self.activate().await;
                GuiResponse::Activate(())
            }
            GuiRequest::SetCells { cells } => {
                self.set_cells(cells).await;
                GuiResponse::SetCells(())
            }
            GuiRequest::SetRecentCells { cells } => {
                self.set_recent_cells(cells).await;
                GuiResponse::SetRecentCells(())
            }
        }
    }

    pub(crate) async fn compilation_started(mut self, activity_id: u64) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| editor.set_compilation_active(cx, activity_id, true));
            }))
            .await
            .ok();
    }

    pub(crate) async fn compilation_finished(mut self, activity_id: u64) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| editor.set_compilation_active(cx, activity_id, false));
            }))
            .await
            .ok();
    }

    pub(crate) async fn update_cell(
        mut self,
        update: CompressedCompilationUpdate,
    ) -> GuiUpdateResult {
        let decode_started = Instant::now();
        let update = match update.decode() {
            Ok(update) => update,
            Err(error) => {
                error!("could not decode compilation update: {error}");
                return GuiUpdateResult::default();
            }
        };
        let decode_seconds = decode_started.elapsed().as_secs_f64();
        let materialize_started = Instant::now();
        let snapshot = {
            let mut previous = lock_unpoisoned(&self.snapshot);
            let Some(snapshot) = update.materialize(previous.as_ref()) else {
                return GuiUpdateResult {
                    decode_seconds,
                    ..GuiUpdateResult::default()
                };
            };
            *previous = Some(snapshot.clone());
            snapshot
        };
        let materialize_seconds = materialize_started.elapsed().as_secs_f64();
        let preparation_id = NEXT_SNAPSHOT_PREPARATION_ID.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.to_exec
            .send(Box::new(move |editor, app| {
                let context =
                    app.update(|cx| editor.begin_snapshot_preparation(cx, preparation_id));
                let _ = sender.send(Some(context));
            }))
            .await
            .ok();
        let Ok(Some(preparation_context)) = receiver.await else {
            return GuiUpdateResult {
                accepted: true,
                decode_seconds,
                materialize_seconds,
                prepare_seconds: 0.,
            };
        };

        // Hierarchy metadata, bounding boxes, and layer usage can be expensive
        // for a large cell. This RPC future runs on GPUI's background executor, so
        // prepare the immutable presentation data here instead of blocking the
        // UI thread and freezing its activity animation.
        let prepare_started = Instant::now();
        let snapshot = prepare_compilation_snapshot(snapshot, preparation_context);
        let prepare_seconds = prepare_started.elapsed().as_secs_f64();
        self.to_exec
            .send(Box::new(move |editor, app| {
                app.update(|cx| editor.finish_snapshot_preparation(cx, preparation_id, snapshot));
            }))
            .await
            .ok();
        GuiUpdateResult {
            accepted: true,
            decode_seconds,
            materialize_seconds,
            prepare_seconds,
        }
    }

    pub(crate) async fn preview_cell(mut self, preview: FocusedCellPreview) -> GuiUpdateResult {
        let decode_started = Instant::now();
        let Ok(update) = preview.update.decode() else {
            return GuiUpdateResult::default();
        };
        // A preview may be a delta against the current full snapshot, but it
        // never becomes the base for later updates. A preview built against
        // any other base is rejected before it is prepared.
        let Some(snapshot) = update.materialize(lock_unpoisoned(&self.snapshot).as_ref()) else {
            return GuiUpdateResult::default();
        };
        let decode_seconds = decode_started.elapsed().as_secs_f64();
        let (sender, receiver) = oneshot::channel();
        let previous_cell = preview.previous_cell;
        let revision = snapshot.revision;
        self.to_exec
            .send(Box::new(move |editor, app| {
                let context = app
                    .update(|cx| editor.preview_preparation_context(cx, previous_cell, revision));
                let _ = sender.send(context);
            }))
            .await
            .ok();
        let Ok(Some((context, source_scope))) = receiver.await else {
            return GuiUpdateResult::default();
        };
        let prepare_started = Instant::now();
        let snapshot = prepare_compilation_snapshot(snapshot, context);
        let prepare_seconds = prepare_started.elapsed().as_secs_f64();
        let (sender, receiver) = oneshot::channel();
        self.to_exec
            .send(Box::new(move |editor, app| {
                let accepted = app.update(|cx| {
                    editor.finish_cell_preview(cx, previous_cell, source_scope, snapshot)
                });
                let _ = sender.send(accepted);
            }))
            .await
            .ok();
        GuiUpdateResult {
            accepted: receiver.await.unwrap_or(false),
            decode_seconds,
            prepare_seconds,
            ..GuiUpdateResult::default()
        }
    }

    pub(crate) async fn fit(mut self) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| editor.fit_to_screen(cx));
            }))
            .await
            .ok();
    }

    pub(crate) async fn set_workspace_path(mut self, path: Option<std::path::PathBuf>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| editor.set_workspace_path(cx, path));
            }))
            .await
            .ok();
    }

    pub(crate) async fn workspace_modified(mut self, modified: bool) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| editor.set_workspace_modified(cx, modified));
            }))
            .await
            .ok();
    }

    pub(crate) async fn show_message(mut self, typ: MessageType, message: String) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| {
                    editor.state.update(cx, |state, cx| {
                        state.show_message(typ, message);
                        cx.notify();
                    });
                });
            }))
            .await
            .ok();
    }

    pub(crate) async fn selected_scope(mut self) -> Option<Span> {
        let (sender, receiver) = oneshot::channel();
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let selected = cx.update(|cx| editor.selected_scope_span(cx));
                let _ = sender.send(selected);
            }))
            .await
            .ok()?;
        receiver.await.ok().flatten()
    }

    pub(crate) async fn selected_cell(mut self) -> Option<CellId> {
        let opened_top = lock_unpoisoned(&self.snapshot)
            .as_ref()
            .and_then(CompilationSnapshot::data)
            .map(|data| data.top)?;
        let (sender, receiver) = oneshot::channel();
        self.to_exec
            .send(Box::new(move |editor, app| {
                let selected = app
                    .update(|cx| editor.selected_cell_id(cx))
                    .filter(|selected| *selected != opened_top);
                let _ = sender.send(selected);
            }))
            .await
            .ok()?;
        receiver.await.ok().flatten()
    }

    pub(crate) async fn place_instance(mut self, preview: InstancePreview) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| {
                    editor.place_instance(cx, preview);
                    editor.activate(cx);
                });
            }))
            .await
            .ok();
    }

    pub(crate) async fn schematic_target(mut self) -> Option<Span> {
        let (sender, receiver) = oneshot::channel();
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let target = cx.update(|cx| editor.schematic_target(cx));
                let _ = sender.send(target);
            }))
            .await
            .ok()?;
        receiver.await.ok().flatten()
    }

    pub(crate) async fn place_schematic_instance(mut self, preview: SchematicInstancePreview) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| {
                    editor.place_schematic_instance(cx, preview);
                    editor.activate(cx);
                });
            }))
            .await
            .ok();
    }

    pub(crate) async fn place_device(mut self, preview: DevicePreview) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| {
                    editor.place_device(cx, preview);
                    editor.activate(cx);
                });
            }))
            .await
            .ok();
    }
    pub(crate) async fn configure(mut self, config: ArgonConfig) {
        #[cfg(not(target_family = "wasm"))]
        let _ = analyzer::reload_log_filter(&config.log.level);
        self.to_exec
            .send(Box::new(move |editor, cx| {
                editor.state.update(cx, |state, cx| {
                    state.hierarchy_depth = config.gui.hierarchy_depth.unwrap_or(usize::MAX);
                    state.dark_mode = config.gui.dark_mode;
                    state.icon_size = config.gui.icon_size;
                    state.font_size = config.gui.font_size;
                    state.option_as_meta = config.gui.option_as_meta;
                    cx.notify();
                });
            }))
            .await
            .ok();
    }

    pub(crate) async fn highlight_spans(mut self, revision: u64, spans: Vec<argonc::ast::Span>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| editor.highlight_agent_edit(cx, revision, spans));
            }))
            .await
            .ok();
    }

    pub(crate) async fn agent_activity(mut self, label: Option<String>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                cx.update(|cx| editor.set_agent_activity(cx, label));
            }))
            .await
            .ok();
    }

    pub(crate) async fn activate(mut self) {
        self.to_exec
            .send(Box::new(|editor, cx| {
                cx.update(|cx| {
                    #[cfg(not(target_family = "wasm"))]
                    crate::main_window::reopen_standalone(editor, cx);
                    editor.activate(cx);
                });
            }))
            .await
            .ok();
    }

    pub(crate) async fn set_cells(mut self, cells: Vec<CellEntry>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                editor.state.update(cx, |state, cx| {
                    state.cells = Some(cells);
                    cx.notify();
                });
            }))
            .await
            .ok();
    }

    pub(crate) async fn set_recent_cells(mut self, cells: Vec<RecentCell>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                editor.state.update(cx, |state, cx| {
                    state.recent_cells = cells;
                    cx.notify();
                });
            }))
            .await
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::lock_unpoisoned;

    #[gpui::test]
    fn sse_source_edit_yields_until_neovim_accepts_it(cx: &mut gpui::TestAppContext) {
        use futures::{FutureExt, SinkExt, StreamExt};
        use protocol::rpc::{LangServerRequest, LangServerResponse};
        use std::sync::atomic::{AtomicBool, Ordering};

        let (client, mut server) = super::SyncLangServerClient::for_rpc_test(cx.to_async());
        let completed = Arc::new(AtomicBool::new(false));
        let done = completed.clone();
        cx.to_async()
            .spawn(async move |_| {
                client
                    .update_values_async(vec![], vec![], None)
                    .await
                    .unwrap();
                done.store(true, Ordering::Release);
            })
            .detach();
        let request = loop {
            if let Some(message) = server.next().now_or_never()
                && let tarpc::ClientMessage::Request(request) = message.unwrap().unwrap()
            {
                break request;
            }
            assert!(cx.dispatcher.tick(false));
        };
        assert!(matches!(
            request.message,
            LangServerRequest::UpdateValues { .. }
        ));
        assert!(!completed.load(Ordering::Acquire));
        let foreground_ran = Arc::new(AtomicBool::new(false));
        let ran = foreground_ran.clone();
        cx.to_async()
            .spawn(async move |_| ran.store(true, Ordering::Release))
            .detach();
        while !foreground_ran.load(Ordering::Acquire) {
            assert!(cx.dispatcher.tick(false));
        }
        assert!(!completed.load(Ordering::Acquire));
        server
            .send(tarpc::Response {
                request_id: request.id,
                message: Ok(LangServerResponse::UpdateValues(Some(vec![]))),
            })
            .now_or_never()
            .unwrap()
            .unwrap();
        while !completed.load(Ordering::Acquire) {
            assert!(cx.dispatcher.tick(false));
        }
    }

    #[test]
    fn client_lock_recovers_from_poisoning() {
        let lock = Arc::new(Mutex::new(1));
        let poisoned_lock = lock.clone();
        let result = std::thread::spawn(move || {
            let _guard = lock_unpoisoned(&poisoned_lock);
            panic!("poison test lock");
        })
        .join();
        assert!(result.is_err());

        *lock_unpoisoned(&lock) = 2;
        assert_eq!(*lock_unpoisoned(&lock), 2);
    }
}
