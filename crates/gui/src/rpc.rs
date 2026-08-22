use std::{
    fmt::Display,
    future::Future,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    sync::{Arc, Mutex},
    time::Duration,
};

use analyzer::rpc::{
    DimensionParams, Gui, InstancePreview, LangServerAction, LangServerClient, ValueEdit,
};
use anyhow::{Result, anyhow};
use argonc::{
    ast::Span,
    compile::{BasicRect, CompileOutput},
};
use async_compat::CompatExt;
use futures::{
    channel::mpsc::{self, UnboundedReceiver, UnboundedSender},
    prelude::*,
};
use gpui::AsyncApp;
use tarpc::{
    context,
    server::{Channel, incoming::Incoming},
    tokio_serde::formats::Json,
};
use tower_lsp_server::ls_types::MessageType;
use tracing::error;

use crate::{editor::Editor, editor_window_options, focus};

pub const LANG_SERVER_CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct SyncLangServerClient {
    app: AsyncApp,
    lang_server_addr: SocketAddr,
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

fn connect_client(app: &AsyncApp, lang_server_addr: SocketAddr) -> Result<LangServerClient> {
    app.background_executor()
        .block(
            async move {
                let mut transport =
                    tarpc::serde_transport::tcp::connect(lang_server_addr, Json::default);
                transport.config_mut().max_frame_length(usize::MAX);
                let transport = transport.await?;
                Ok::<_, std::io::Error>(
                    LangServerClient::new(tarpc::client::Config::default(), transport).spawn(),
                )
            }
            .compat(),
        )
        .map_err(Into::into)
}

impl SyncLangServerClient {
    pub fn new(app: AsyncApp, lang_server_addr: SocketAddr) -> (Self, UnboundedReceiver<EditorFn>) {
        let client = connect_client(&app, lang_server_addr).unwrap();
        let (to_exec, rx) = mpsc::unbounded();
        (
            Self {
                app,
                lang_server_addr,
                client: Arc::new(Mutex::new(client)),
                to_exec,
            },
            rx,
        )
    }

    fn call<T, F, Fut>(&self, request: F) -> Result<T>
    where
        T: Send + 'static,
        F: Fn(LangServerClient) -> Fut,
        Fut: Future<Output = std::result::Result<T, tarpc::client::RpcError>> + Send + 'static,
    {
        let client = self.client.lock().unwrap().clone();
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
        let client = connect_client(&self.app, self.lang_server_addr)?;
        *self.client.lock().unwrap() = client.clone();
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

    pub fn register_server(
        &self,
        configured_port: Option<u16>,
        prebound_listener: Option<TcpListener>,
        register_addr: Option<SocketAddr>,
    ) {
        let background_executor = self.app.background_executor().clone();
        let mut listener = self.app.background_executor().block(
            async move {
                let result = if let Some(listener) = prebound_listener {
                    match listener
                        .set_nonblocking(true)
                        .and_then(|_| tokio::net::TcpListener::from_std(listener))
                    {
                        Ok(listener) => {
                            tarpc::serde_transport::tcp::listen_on(listener, Json::default).await
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    let port = configured_port.unwrap_or(0);
                    tarpc::serde_transport::tcp::listen((Ipv4Addr::LOCALHOST, port), Json::default)
                        .await
                };
                match result {
                    Ok(listener) => listener,
                    Err(error) => {
                        error!("Failed to start GUI RPC server: {error}");
                        std::process::exit(1);
                    }
                }
            }
            .compat(),
        );
        let server_addr = listener.local_addr();
        let register_addr = register_addr.unwrap_or(server_addr);
        let to_exec = self.to_exec.clone();
        self.app
            .background_executor()
            .spawn(
                async move {
                    listener.config_mut().max_frame_length(usize::MAX);
                    listener
                        // Ignore accept errors.
                        .filter_map(|r| futures::future::ready(r.ok()))
                        .map(tarpc::server::BaseChannel::with_defaults)
                        // Limit channels to 1 per IP.
                        .max_channels_per_key(1, |t| t.transport().peer_addr().unwrap().ip())
                        // serve is generated by the service attribute. It takes as input any type implementing
                        // the generated World trait.
                        .map(|channel| {
                            let server = GuiServer {
                                to_exec: to_exec.clone(),
                            };
                            channel
                                .execute(server.serve())
                                .for_each(|t| background_executor.spawn(t))
                        })
                        // Max 10 channels.
                        .buffer_unordered(10)
                        .for_each(|_| async {})
                        .await;
                }
                .compat(),
            )
            .detach();
        let client_clone = self.client.lock().unwrap().clone();
        match self
            .app
            .background_executor()
            .block_with_timeout(
                LANG_SERVER_CLIENT_TIMEOUT,
                async move {
                    client_clone
                        .register(context::current(), register_addr)
                        .await
                }
                .compat()
                .map_err(|e| format!("{}", e)),
            )
            .map_err(|_| format!("timeout after {LANG_SERVER_CLIENT_TIMEOUT:?}"))
        {
            Err(e) | Ok(Err(e)) => {
                error!("Failed to register: {e}");
                std::process::exit(1);
            }
            _ => {}
        }
    }

    pub fn select_rect(&self, span: Span) -> Result<()> {
        self.call(move |client| {
            let span = span.clone();
            async move { client.select_rect(context::current(), span).await }
        })
    }

    pub fn draw_rect(
        &self,
        scope_span: Span,
        var_name: String,
        rect: BasicRect<f64>,
    ) -> Result<Option<Span>> {
        self.call(move |client| {
            let scope_span = scope_span.clone();
            let var_name = var_name.clone();
            let rect = rect.clone();
            async move {
                client
                    .draw_rect(context::current(), scope_span, var_name, rect)
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

    pub fn update_values(&self, edits: Vec<ValueEdit>) -> Result<bool> {
        self.call(move |client| {
            let edits = edits.clone();
            async move { client.update_values(context::current(), edits).await }
        })
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

    pub fn open_command_bar(&self) -> Result<()> {
        self.call(move |client| async move { client.focus_editor(context::current(), true).await })
    }
}

type EditorFn = Box<dyn FnOnce(&Editor, &mut AsyncApp) + Send>;

#[derive(Clone)]
pub struct GuiServer {
    to_exec: UnboundedSender<EditorFn>,
}

impl Gui for GuiServer {
    async fn open_cell(mut self, _: context::Context, cell: CompileOutput, update: bool) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| {
                    editor.open_cell(cx, cell, update);
                    if !update {
                        focus::activate_gui(cx);
                    }
                });
            }))
            .await
            .unwrap();
    }

    async fn place_instance(mut self, _: context::Context, preview: InstancePreview) {
        self.to_exec
            .send(Box::new(move |editor, cx| {
                let _ = cx.update(|cx| {
                    editor.place_instance(cx, preview);
                    focus::activate_gui(cx);
                });
            }))
            .await
            .unwrap();
    }
    async fn set(mut self, _: tarpc::context::Context, key: String, value: String) -> () {
        match key.as_str() {
            "hierarchyDepth" => {
                self.to_exec
                    .send(Box::new(move |editor, cx| {
                        editor
                            .state
                            .update(cx, |state, cx| {
                                // TODO: Need better way to specify infinite hierarchy depth.
                                state.hierarchy_depth = value.parse().unwrap_or(usize::MAX);
                                cx.notify();
                            })
                            .unwrap();
                    }))
                    .await
                    .unwrap();
            }
            "darkMode" => {
                self.to_exec
                    .send(Box::new(move |editor, cx| {
                        if let Ok(new_mode) = value.parse() {
                            editor
                                .state
                                .update(cx, |state, cx| {
                                    // TODO: Need better way to specify infinite hierarchy depth.
                                    state.dark_mode = new_mode;
                                    cx.notify();
                                })
                                .unwrap();
                        }
                    }))
                    .await
                    .unwrap();
            }
            _ => {
                // TODO: handle errors.
            }
        }
    }

    async fn activate(mut self, _context: ::tarpc::context::Context) -> () {
        self.to_exec
            .send(Box::new(|editor, cx| {
                let editor = editor.clone();
                let _ = cx.update(|cx| {
                    if cx.windows().is_empty() {
                        let _ = cx.open_window(editor_window_options(), |window, cx| {
                            window.replace_root(cx, |_, _| editor)
                        });
                    }
                    focus::activate_gui(cx);
                });
            }))
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::is_disconnected;

    #[test]
    fn reconnects_only_for_transport_failures() {
        assert!(is_disconnected(&tarpc::client::RpcError::Shutdown));
        assert!(!is_disconnected(&tarpc::client::RpcError::DeadlineExceeded));
    }
}
