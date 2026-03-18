use std::{
    fmt::Display,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::{Result, anyhow};
use compiler::{
    ast::Span,
    compile::{BasicRect, CompileOutput},
};
use futures::StreamExt;
use lang_server::rpc::{DimensionParams, Gui, LangServerAction, LangServerClient};
use tarpc::{context, server::Channel, tokio_serde::formats::Json};
use tokio::runtime::Runtime;
use tower_lsp_server::lsp_types::MessageType;
use tracing::error;

pub const LANG_SERVER_CLIENT_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub enum GuiEvent {
    OpenCell { output: CompileOutput, update: bool },
    Set { key: String, value: String },
}

#[derive(Clone)]
pub struct SyncLangServerClient {
    runtime: Arc<Runtime>,
    client: LangServerClient,
}

impl SyncLangServerClient {
    pub fn new(lang_server_addr: SocketAddr) -> Result<(Self, mpsc::Receiver<GuiEvent>)> {
        let runtime = Arc::new(Runtime::new()?);
        let client = runtime.block_on(async move {
            let mut transport =
                tarpc::serde_transport::tcp::connect(lang_server_addr, Json::default);
            transport.config_mut().max_frame_length(usize::MAX);
            Ok::<_, anyhow::Error>(
                LangServerClient::new(tarpc::client::Config::default(), transport.await?).spawn(),
            )
        })?;
        let (tx, rx) = mpsc::channel();
        let out = Self { runtime, client };
        out.register_server(tx)?;
        Ok((out, rx))
    }

    fn with_timeout<F, T>(&self, future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T, tarpc::client::RpcError>>,
    {
        self.runtime.block_on(async {
            tokio::time::timeout(LANG_SERVER_CLIENT_TIMEOUT, future)
                .await
                .map_err(|_| {
                    anyhow!("timeout reaching language server after {LANG_SERVER_CLIENT_TIMEOUT:?}")
                })?
                .map_err(|err| anyhow!(err))
        })
    }

    fn register_server(&self, tx: mpsc::Sender<GuiEvent>) -> Result<()> {
        let runtime = self.runtime.clone();
        let listener = runtime.block_on(async {
            let port = std::env::var("ARGON_GUI_DEFAULT_PORT")
                .ok()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(12346);
            if let Ok(listener) =
                tarpc::serde_transport::tcp::listen((Ipv4Addr::LOCALHOST, port), Json::default)
                    .await
            {
                Ok::<_, anyhow::Error>(listener)
            } else {
                Ok::<_, anyhow::Error>(
                    tarpc::serde_transport::tcp::listen((Ipv4Addr::LOCALHOST, 0), Json::default)
                        .await?,
                )
            }
        })?;

        let server_addr = listener.local_addr();
        runtime.spawn(async move {
            let mut listener = listener;
            listener.config_mut().max_frame_length(usize::MAX);
            while let Some(result) = listener.next().await {
                let Ok(transport) = result else {
                    continue;
                };
                let channel = tarpc::server::BaseChannel::with_defaults(transport);
                let server = GuiServer { tx: tx.clone() };
                tokio::spawn(async move {
                    channel
                        .execute(server.serve())
                        .for_each(|future| async move {
                            tokio::spawn(future);
                        })
                        .await;
                });
            }
        });

        let client = self.client.clone();
        self.runtime.block_on(async {
            tokio::time::timeout(
                LANG_SERVER_CLIENT_TIMEOUT,
                client.register(context::current(), server_addr),
            )
            .await
            .map_err(|_| {
                anyhow!("timeout reaching language server after {LANG_SERVER_CLIENT_TIMEOUT:?}")
            })?
            .map_err(|err| anyhow!(err))
        })?;

        Ok(())
    }

    pub fn select_rect(&self, span: Span) -> Result<()> {
        self.with_timeout(async move {
            self.client
                .clone()
                .select_rect(context::current(), span)
                .await
        })
    }

    pub fn draw_rect(
        &self,
        scope_span: Span,
        var_name: String,
        rect: BasicRect<f64>,
    ) -> Result<Option<Span>> {
        self.with_timeout(async move {
            self.client
                .clone()
                .draw_rect(context::current(), scope_span, var_name, rect)
                .await
        })
    }

    pub fn draw_dimension(
        &self,
        scope_span: Span,
        params: DimensionParams,
    ) -> Result<Option<Span>> {
        self.with_timeout(async move {
            self.client
                .clone()
                .draw_dimension(context::current(), scope_span, params)
                .await
        })
    }

    pub fn edit_dimension(&self, span: Span, value: String) -> Result<Option<Span>> {
        self.with_timeout(async move {
            self.client
                .clone()
                .edit_dimension(context::current(), span, value)
                .await
        })
    }

    #[allow(dead_code)]
    pub fn add_eq_constraint(&self, scope_span: Span, lhs: String, rhs: String) -> Result<()> {
        self.with_timeout(async move {
            self.client
                .clone()
                .add_eq_constraint(context::current(), scope_span, lhs, rhs)
                .await
        })
    }

    pub fn open_cell(&self, cell: String) -> Result<()> {
        self.with_timeout(async move {
            self.client
                .clone()
                .open_cell(context::current(), cell)
                .await
        })
    }

    pub fn show_message<M: Display>(&self, typ: MessageType, message: M) -> Result<()> {
        self.with_timeout(async move {
            self.client
                .clone()
                .show_message(context::current(), typ, message.to_string())
                .await
        })
    }

    pub fn dispatch_action(&self, action: LangServerAction) -> Result<()> {
        self.with_timeout(async move {
            self.client
                .clone()
                .dispatch_action(context::current(), action)
                .await
        })
    }
}

#[derive(Clone)]
struct GuiServer {
    tx: mpsc::Sender<GuiEvent>,
}

impl Gui for GuiServer {
    async fn open_cell(self, _: context::Context, output: CompileOutput, update: bool) {
        if let Err(err) = self.tx.send(GuiEvent::OpenCell { output, update }) {
            error!("failed to dispatch open_cell to gui: {err}");
        }
    }

    async fn set(self, _: context::Context, key: String, value: String) {
        if let Err(err) = self.tx.send(GuiEvent::Set { key, value }) {
            error!("failed to dispatch set to gui: {err}");
        }
    }

    async fn activate(self, _: context::Context) {}
}
