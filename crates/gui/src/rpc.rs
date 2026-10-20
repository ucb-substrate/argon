use std::{
    fmt::Display,
    future::Future,
    net::SocketAddr,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use analyzer::ArgonConfig;
use analyzer::rpc::{
    AddSignalRequest, AddSymbolItemsRequest, CallArguments, CellEntry, ClipboardItem,
    CompilationSnapshot, CompressedCompilationUpdate, ConnectRequest, CreateSymbolRequest,
    DeleteOutcome, DeleteRequest, DevicePreview, DimensionParams, EditReceipt, FocusEditorParams,
    FocusedCellPreview, Gui, GuiUpdateResult, InitialConditionEdit, InstancePreview,
    LangServerAction, LangServerClient, PasteRequest, PathParams, PlaceDeviceRequest, PlaceRequest,
    PlaceSchematicInstanceRequest, PolygonParams, RecentCell, RectangleEditResult, RenameRequest,
    SchematicInstancePreview, SetArgumentRequest, ValueEdit,
};
use analyzer::transport::{self, Role, SessionToken};
use anyhow::{Result, anyhow};
use argonc::{
    ast::Span,
    compile::{BasicRect, CellId},
};
use async_compat::CompatExt;
use futures::{
    channel::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    prelude::*,
};
use gpui::{AsyncApp, Task};
use tarpc::{context, server::Channel, tokio_serde::formats::Bincode};
use tower_lsp_server::ls_types::MessageType;
use tracing::error;

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

#[derive(Clone)]
pub struct SyncLangServerClient {
    app: AsyncApp,
    lang_server_addr: SocketAddr,
    token: SessionToken,
    client: Arc<Mutex<LangServerClient>>,
    to_exec: UnboundedSender<EditorFn>,
}

enum RpcCallError {
    Rpc(tarpc::client::RpcError),
    Timeout,
}

fn is_disconnected(error: &tarpc::client::RpcError) -> bool {
    matches!(
        error,
        tarpc::client::RpcError::Shutdown
            | tarpc::client::RpcError::Send(_)
            | tarpc::client::RpcError::Channel(_)
    )
}

async fn connect_lang_server(
    lang_server_addr: SocketAddr,
    token: SessionToken,
) -> std::io::Result<LangServerClient> {
    let stream = transport::connect(lang_server_addr, &token, Role::Gui).await?;
    let transport = tarpc::serde_transport::new(stream, Bincode::default());
    Ok(LangServerClient::new(tarpc::client::Config::default(), transport).spawn())
}

fn connect_client(
    app: &AsyncApp,
    lang_server_addr: SocketAddr,
    token: &SessionToken,
) -> Result<LangServerClient> {
    app.background_executor()
        .block(connect_lang_server(lang_server_addr, token.clone()).compat())
        .map_err(Into::into)
}

#[cfg(test)]
pub(crate) type TestLangServerTransport = tarpc::transport::channel::UnboundedChannel<
    tarpc::ClientMessage<analyzer::rpc::LangServerRequest>,
    tarpc::Response<analyzer::rpc::LangServerResponse>,
>;

impl SyncLangServerClient {
    /// Disconnected client for rendering tests that never issue source edits.
    #[cfg(test)]
    pub(crate) fn for_render_test(app: AsyncApp) -> Self {
        let (transport, _) = tarpc::transport::channel::unbounded();
        let client = LangServerClient::new(tarpc::client::Config::default(), transport).client;
        let (to_exec, _) = mpsc::unbounded();
        Self {
            app,
            lang_server_addr: "127.0.0.1:1".parse().unwrap(),
            token: SessionToken::generate().unwrap(),
            client: Arc::new(Mutex::new(client)),
            to_exec,
        }
    }

    /// Real client dispatch over an in-memory transport, with replies controlled
    /// by the rendering test. This exercises yielding while an edit is pending.
    #[cfg(test)]
    pub(crate) fn for_rpc_test(app: AsyncApp) -> (Self, TestLangServerTransport) {
        let (transport, server) = tarpc::transport::channel::unbounded();
        let client = LangServerClient::new(tarpc::client::Config::default(), transport);
        app.background_executor()
            .spawn(client.dispatch.compat())
            .detach();
        let (to_exec, _) = mpsc::unbounded();
        (
            Self {
                app,
                lang_server_addr: "127.0.0.1:1".parse().unwrap(),
                token: SessionToken::generate().unwrap(),
                client: Arc::new(Mutex::new(client.client)),
                to_exec,
            },
            server,
        )
    }

    pub fn new(
        app: AsyncApp,
        lang_server_addr: SocketAddr,
        token: SessionToken,
    ) -> Result<(Self, UnboundedReceiver<EditorFn>)> {
        let client = connect_client(&app, lang_server_addr, &token)?;
        let (to_exec, rx) = mpsc::unbounded();
        Ok((
            Self {
                app,
                lang_server_addr,
                token,
                client: Arc::new(Mutex::new(client)),
                to_exec,
            },
            rx,
        ))
    }

    fn call<T, F, Fut>(&self, request: F) -> Result<T>
    where
        T: Send + 'static,
        F: Fn(LangServerClient) -> Fut,
        Fut: Future<Output = std::result::Result<T, tarpc::client::RpcError>> + Send + 'static,
    {
        let client = lock_unpoisoned(&self.client).clone();
        let result = self.call_once(request(client));
        let result = match result {
            Err(RpcCallError::Rpc(error)) if is_disconnected(&error) => match self.reconnect() {
                Ok(client) => self.call_once(request(client)),
                Err(error) => {
                    let result = Err(error);
                    self.report_connection_result(&result);
                    return result;
                }
            },
            result => result,
        };
        let result = match result {
            Ok(value) => Ok(value),
            Err(RpcCallError::Rpc(error)) => Err(error.into()),
            Err(RpcCallError::Timeout) => Err(anyhow!(
                "timeout reaching language server after {LANG_SERVER_CLIENT_TIMEOUT:?}"
            )),
        };
        self.report_connection_result(&result);
        result
    }

    /// Source placement must yield while Neovim accepts the edit, so painting
    /// and navigation continue throughout the request and reconnect timeout.
    pub(crate) async fn call_async<T, F, Fut>(&self, request: F) -> Result<T>
    where
        T: Send + 'static,
        F: Fn(LangServerClient) -> Fut,
        Fut: Future<Output = std::result::Result<T, tarpc::client::RpcError>> + Send + 'static,
    {
        let client = lock_unpoisoned(&self.client).clone();
        let result = self.call_once_async(request(client)).await;
        let result = match result {
            Err(RpcCallError::Rpc(error)) if is_disconnected(&error) => {
                let addr = self.lang_server_addr;
                let token = self.token.clone();
                let reconnect = self
                    .app
                    .background_executor()
                    .spawn(
                        async move {
                            tokio::time::timeout(
                                LANG_SERVER_CLIENT_TIMEOUT,
                                connect_lang_server(addr, token),
                            )
                            .await?
                        }
                        .compat(),
                    )
                    .await;
                match reconnect {
                    Ok(client) => {
                        *lock_unpoisoned(&self.client) = client.clone();
                        self.call_once_async(request(client)).await
                    }
                    Err(error) => {
                        let result = Err(error.into());
                        self.report_connection_result(&result);
                        return result;
                    }
                }
            }
            result => result,
        };
        let result = match result {
            Ok(value) => Ok(value),
            Err(RpcCallError::Rpc(error)) => Err(error.into()),
            Err(RpcCallError::Timeout) => Err(anyhow!(
                "timeout reaching language server after {LANG_SERVER_CLIENT_TIMEOUT:?}"
            )),
        };
        self.report_connection_result(&result);
        result
    }

    async fn call_once_async<T, Fut>(&self, request: Fut) -> std::result::Result<T, RpcCallError>
    where
        T: Send + 'static,
        Fut: Future<Output = std::result::Result<T, tarpc::client::RpcError>> + Send + 'static,
    {
        self.app
            .background_executor()
            .spawn(
                async move {
                    match tokio::time::timeout(LANG_SERVER_CLIENT_TIMEOUT, request).await {
                        Ok(result) => result.map_err(RpcCallError::Rpc),
                        Err(_) => Err(RpcCallError::Timeout),
                    }
                }
                .compat(),
            )
            .await
    }

    fn call_once<T, Fut>(&self, request: Fut) -> std::result::Result<T, RpcCallError>
    where
        T: Send + 'static,
        Fut: Future<Output = std::result::Result<T, tarpc::client::RpcError>> + Send + 'static,
    {
        match self
            .app
            .background_executor()
            .block_with_timeout(LANG_SERVER_CLIENT_TIMEOUT, request.compat())
        {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(RpcCallError::Rpc(error)),
            Err(_) => Err(RpcCallError::Timeout),
        }
    }

    fn reconnect(&self) -> Result<LangServerClient> {
        let client = connect_client(&self.app, self.lang_server_addr, &self.token)?;
        *lock_unpoisoned(&self.client) = client.clone();
        Ok(client)
    }

    fn report_connection_result<T>(&self, result: &Result<T>) {
        let error = result.as_ref().err().map(ToString::to_string);
        let _ = self.to_exec.unbounded_send(Box::new(move |editor, cx| {
            let _ = editor.state.update(cx, |state, cx| {
                if let Some(error) = error {
                    state.connection_error = Some(error.into());
                } else {
                    state.connection_error = None;
                }
                cx.notify();
            });
        }));
    }

    fn report_message(&self, typ: MessageType, message: String) {
        if typ == MessageType::LOG {
            return;
        }
        let _ = self.to_exec.unbounded_send(Box::new(move |editor, cx| {
            let _ = editor.state.update(cx, |state, cx| {
                state.show_message(typ, message);
                cx.notify();
            });
        }));
    }

    /// Opens the connection the analyzer calls the GUI back over and serves
    /// it until the returned task is dropped.
    pub fn serve_callbacks(&self) -> Task<()> {
        let client = self.clone();
        self.app
            .spawn(async move |_| match client.connect_callbacks().await {
                Ok(server) => {
                    client.report_connection_result(&Ok(()));
                    server.await;
                }
                Err(error) => {
                    error!("Failed to connect the GUI to the analyzer: {error}");
                    client.report_connection_result::<()>(&Err(error));
                }
            })
    }

    async fn connect_callbacks(&self) -> Result<Task<()>> {
        let addr = self.lang_server_addr;
        let token = self.token.clone();
        let stream = self
            .app
            .background_executor()
            .spawn(
                async move { transport::connect(addr, &token, Role::GuiCallback).await }.compat(),
            )
            .await?;
        let background_executor = self.app.background_executor().clone();
        let server = GuiServer {
            to_exec: self.to_exec.clone(),
            snapshot: Arc::default(),
        };
        Ok(self.app.background_executor().spawn(
            async move {
                let transport = tarpc::serde_transport::new(stream, Bincode::default());
                tarpc::server::BaseChannel::with_defaults(transport)
                    .execute(server.serve())
                    .for_each(|response| background_executor.spawn(response))
                    .await;
            }
            .compat(),
        ))
    }

    /// Reveals `span` in the editor.
    pub fn select_span(&self, span: Span) -> Result<()> {
        self.call(move |client| {
            let span = span.clone();
            async move { client.select_span(context::current(), span).await }
        })
    }

    pub async fn draw_rect(
        &self,
        scope_span: Span,
        var_name: String,
        rect: BasicRect<f64>,
    ) -> Result<Option<RectangleEditResult>> {
        self.call_async(move |client| {
            let scope_span = scope_span.clone();
            let var_name = var_name.clone();
            let rect = rect.clone();
            async move {
                client
                    .draw_rect(context::current(), scope_span, var_name, rect)
                    .await
            }
        })
        .await
    }

    pub fn draw_polygon(
        &self,
        scope_span: Span,
        var_name: String,
        polygon: PolygonParams,
    ) -> Result<Option<Span>> {
        self.call(move |client| {
            let scope_span = scope_span.clone();
            let var_name = var_name.clone();
            let polygon = polygon.clone();
            async move {
                client
                    .draw_polygon(context::current(), scope_span, var_name, polygon)
                    .await
            }
        })
    }

    pub fn draw_path(
        &self,
        scope_span: Span,
        var_name: String,
        path: PathParams,
    ) -> Result<Option<Span>> {
        self.call(move |client| {
            let scope_span = scope_span.clone();
            let var_name = var_name.clone();
            let path = path.clone();
            async move {
                client
                    .draw_path(context::current(), scope_span, var_name, path)
                    .await
            }
        })
    }

    pub fn place_instance(
        &self,
        scope_span: Span,
        invocation: String,
        x: f64,
        y: f64,
    ) -> Result<Option<Span>> {
        self.call(move |client| {
            let scope_span = scope_span.clone();
            let invocation = invocation.clone();
            async move {
                client
                    .place_instance(context::current(), scope_span, invocation, x, y)
                    .await
            }
        })
    }

    pub fn draw_dimension(
        &self,
        scope_span: Span,
        params: DimensionParams,
    ) -> Result<Option<Span>> {
        self.call(move |client| {
            let scope_span = scope_span.clone();
            let params = params.clone();
            async move {
                client
                    .draw_dimension(context::current(), scope_span, params)
                    .await
            }
        })
    }

    pub fn edit_dimension(&self, span: Span, value: String) -> Result<Option<Span>> {
        self.call(move |client| {
            let span = span.clone();
            let value = value.clone();
            async move { client.edit_dimension(context::current(), span, value).await }
        })
    }

    pub async fn update_values_async(
        &self,
        edits: Vec<ValueEdit>,
        initial_conditions: Vec<InitialConditionEdit>,
        base_revision: Option<u64>,
    ) -> Result<Option<Vec<ValueEdit>>> {
        self.call_async(move |rpc| {
            let edits = edits.clone();
            let initial_conditions = initial_conditions.clone();
            async move {
                rpc.update_values(context::current(), edits, initial_conditions, base_revision)
                    .await
            }
        })
        .await
    }

    pub fn dispatch_sse_source_edit_result(
        &self,
        generation: u64,
        result: Result<Option<Vec<ValueEdit>>>,
        initial_conditions: Vec<InitialConditionEdit>,
    ) {
        let _ = self.to_exec.unbounded_send(Box::new(move |editor, cx| {
            let _ = cx.update(|cx| {
                editor.finish_sse_source_edit(cx, generation, result, initial_conditions)
            });
        }));
    }

    /// Applies a schematic placement or route gesture, off the UI thread.
    pub async fn place(&self, request: PlaceRequest) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.place(context::current(), request).await }
        })
        .await
    }

    pub async fn add_signal(&self, request: AddSignalRequest) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.add_signal(context::current(), request).await }
        })
        .await
    }

    pub async fn place_schematic_instance(
        &self,
        request: PlaceSchematicInstanceRequest,
    ) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move {
                client
                    .place_schematic_instance(context::current(), request)
                    .await
            }
        })
        .await
    }

    pub async fn place_device(&self, request: PlaceDeviceRequest) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.place_device(context::current(), request).await }
        })
        .await
    }

    pub async fn connect(&self, request: ConnectRequest) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.connect(context::current(), request).await }
        })
        .await
    }

    pub async fn delete(&self, request: DeleteRequest) -> Result<Option<DeleteOutcome>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.delete(context::current(), request).await }
        })
        .await
    }

    pub async fn set_argument(&self, request: SetArgumentRequest) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.set_argument(context::current(), request).await }
        })
        .await
    }

    pub async fn add_symbol_items(
        &self,
        request: AddSymbolItemsRequest,
    ) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.add_symbol_items(context::current(), request).await }
        })
        .await
    }

    pub async fn create_symbol(&self, request: CreateSymbolRequest) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.create_symbol(context::current(), request).await }
        })
        .await
    }

    pub async fn rename(&self, request: RenameRequest) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.rename(context::current(), request).await }
        })
        .await
    }

    pub async fn copy_statements(
        &self,
        base_revision: u64,
        targets: Vec<Span>,
    ) -> Result<Option<Vec<ClipboardItem>>> {
        self.call_async(move |client| {
            let targets = targets.clone();
            async move {
                client
                    .copy_statements(context::current(), base_revision, targets)
                    .await
            }
        })
        .await
    }

    pub async fn paste(&self, request: PasteRequest) -> Result<Option<EditReceipt>> {
        self.call_async(move |client| {
            let request = request.clone();
            async move { client.paste(context::current(), request).await }
        })
        .await
    }

    pub async fn cell_parameters(
        &self,
        base_revision: u64,
        cell: Span,
    ) -> Result<Option<Vec<String>>> {
        self.call_async(move |client| {
            let cell = cell.clone();
            async move {
                client
                    .cell_parameters(context::current(), base_revision, cell)
                    .await
            }
        })
        .await
    }

    pub async fn call_arguments(
        &self,
        base_revision: u64,
        call: Span,
    ) -> Result<Option<CallArguments>> {
        self.call_async(move |client| {
            let call = call.clone();
            async move {
                client
                    .call_arguments(context::current(), base_revision, call)
                    .await
            }
        })
        .await
    }

    pub fn add_eq_constraint(&self, scope_span: Span, lhs: String, rhs: String) -> Result<()> {
        self.call(move |client| {
            let scope_span = scope_span.clone();
            let lhs = lhs.clone();
            let rhs = rhs.clone();
            async move {
                client
                    .add_eq_constraint(context::current(), scope_span, lhs, rhs)
                    .await
            }
        })
    }

    pub fn show_message<M: Display>(&self, typ: MessageType, message: M) -> Result<()> {
        let message = message.to_string();
        self.report_message(typ, message.clone());
        self.call(move |client| {
            let message = message.clone();
            async move { client.show_message(context::current(), typ, message).await }
        })
    }

    pub fn dispatch_action(&self, action: LangServerAction) -> Result<()> {
        self.call(move |client| {
            let action = action.clone();
            async move { client.dispatch_action(context::current(), action).await }
        })
    }

    /// Opens the cell invocation `cell`, such as `inverter()`.
    pub fn open_cell(&self, cell: String) -> Result<()> {
        self.call(move |client| {
            let cell = cell.clone();
            async move { client.open_cell(context::current(), cell).await }
        })
    }

    pub fn open_command_bar(&self, command: Option<String>, return_to_gui: bool) -> Result<()> {
        self.call(move |client| {
            let params = FocusEditorParams {
                command: command.clone(),
                return_to_gui,
            };
            async move { client.focus_editor(context::current(), params).await }
        })
    }
}

type EditorFn = Box<dyn FnOnce(&Editor, &mut AsyncApp) + Send>;

#[derive(Clone)]
pub struct GuiServer {
    to_exec: UnboundedSender<EditorFn>,
    snapshot: Arc<Mutex<Option<CompilationSnapshot>>>,
}

impl Gui for GuiServer {
    async fn compilation_started(mut self, _: context::Context, activity_id: u64) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| editor.set_compilation_active(cx, activity_id, true));
            }))
            .await
            .ok();
    }

    async fn compilation_finished(mut self, _: context::Context, activity_id: u64) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| editor.set_compilation_active(cx, activity_id, false));
            }))
            .await
            .ok();
    }

    async fn update_cell(
        mut self,
        _: context::Context,
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
                let context = app
                    .update(|cx| editor.begin_snapshot_preparation(cx, preparation_id))
                    .ok();
                let _ = sender.send(context);
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
                let _ = app
                    .update(|cx| editor.finish_snapshot_preparation(cx, preparation_id, snapshot));
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

    async fn preview_cell(
        mut self,
        _: context::Context,
        preview: FocusedCellPreview,
    ) -> GuiUpdateResult {
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
                    .update(|cx| editor.preview_preparation_context(cx, previous_cell, revision))
                    .ok()
                    .flatten();
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
                let accepted = app
                    .update(|cx| {
                        editor.finish_cell_preview(cx, previous_cell, source_scope, snapshot)
                    })
                    .unwrap_or(false);
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

    async fn fit(mut self, _: context::Context) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| editor.fit_to_screen(cx));
            }))
            .await
            .ok();
    }

    async fn set_workspace_path(mut self, _: context::Context, path: Option<std::path::PathBuf>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| editor.set_workspace_path(cx, path));
            }))
            .await
            .ok();
    }

    async fn workspace_modified(mut self, _: context::Context, modified: bool) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| editor.set_workspace_modified(cx, modified));
            }))
            .await
            .ok();
    }

    async fn show_message(mut self, _: context::Context, typ: MessageType, message: String) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| {
                    editor.state.update(cx, |state, cx| {
                        state.show_message(typ, message);
                        cx.notify();
                    });
                });
            }))
            .await
            .ok();
    }

    async fn selected_scope(mut self, _: context::Context) -> Option<Span> {
        let (sender, receiver) = oneshot::channel();
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let selected = cx
                    .update(|cx| editor.selected_scope_span(cx))
                    .ok()
                    .flatten();
                let _ = sender.send(selected);
            }))
            .await
            .ok()?;
        receiver.await.ok().flatten()
    }

    async fn selected_cell(mut self, _: context::Context) -> Option<CellId> {
        let opened_top = lock_unpoisoned(&self.snapshot)
            .as_ref()
            .and_then(CompilationSnapshot::data)
            .map(|data| data.top)?;
        let (sender, receiver) = oneshot::channel();
        self.to_exec
            .send(Box::new(move |editor, app| {
                let selected = app
                    .update(|cx| editor.selected_cell_id(cx))
                    .ok()
                    .flatten()
                    .filter(|selected| *selected != opened_top);
                let _ = sender.send(selected);
            }))
            .await
            .ok()?;
        receiver.await.ok().flatten()
    }

    async fn place_instance(mut self, _: context::Context, preview: InstancePreview) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| {
                    editor.place_instance(cx, preview);
                    editor.activate(cx);
                });
            }))
            .await
            .ok();
    }

    async fn schematic_target(mut self, _: context::Context) -> Option<Span> {
        let (sender, receiver) = oneshot::channel();
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let target = cx.update(|cx| editor.schematic_target(cx)).ok().flatten();
                let _ = sender.send(target);
            }))
            .await
            .ok()?;
        receiver.await.ok().flatten()
    }

    async fn place_schematic_instance(
        mut self,
        _: context::Context,
        preview: SchematicInstancePreview,
    ) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| {
                    editor.place_schematic_instance(cx, preview);
                    editor.activate(cx);
                });
            }))
            .await
            .ok();
    }

    async fn place_device(mut self, _: context::Context, preview: DevicePreview) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| {
                    editor.place_device(cx, preview);
                    editor.activate(cx);
                });
            }))
            .await
            .ok();
    }
    async fn configure(mut self, _: tarpc::context::Context, config: ArgonConfig) -> () {
        let _ = analyzer::reload_log_filter(&config.log.level);
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = editor.state.update(cx, |state, cx| {
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

    async fn highlight_spans(
        mut self,
        _: context::Context,
        revision: u64,
        spans: Vec<argonc::ast::Span>,
    ) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| editor.highlight_agent_edit(cx, revision, spans));
            }))
            .await
            .ok();
    }

    async fn agent_activity(mut self, _: context::Context, label: Option<String>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| editor.set_agent_activity(cx, label));
            }))
            .await
            .ok();
    }

    async fn activate(mut self, _context: ::tarpc::context::Context) -> () {
        self.to_exec
            .send(Box::new(|editor, cx| {
                let _ = cx.update(|cx| {
                    crate::main_window::reopen_standalone(editor, cx);
                    editor.activate(cx);
                });
            }))
            .await
            .ok();
    }

    async fn set_cells(mut self, _: context::Context, cells: Vec<CellEntry>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = editor.state.update(cx, |state, cx| {
                    state.cells = Some(cells);
                    cx.notify();
                });
            }))
            .await
            .ok();
    }

    async fn set_recent_cells(mut self, _: context::Context, cells: Vec<RecentCell>) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = editor.state.update(cx, |state, cx| {
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

    use super::{is_disconnected, lock_unpoisoned};

    #[gpui::test]
    fn sse_source_edit_yields_until_neovim_accepts_it(cx: &mut gpui::TestAppContext) {
        use analyzer::rpc::{LangServerRequest, LangServerResponse};
        use futures::{FutureExt, SinkExt, StreamExt};
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

    #[test]
    fn reconnects_only_for_transport_failures() {
        assert!(is_disconnected(&tarpc::client::RpcError::Shutdown));
        assert!(!is_disconnected(&tarpc::client::RpcError::DeadlineExceeded));
    }
}
