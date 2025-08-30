use std::net::SocketAddr;

use async_compat::CompatExt;
use gpui::{BackgroundExecutor, ForegroundExecutor};
use lsp_server::rpc::GuiToLspClient;
use tarpc::{context, tokio_serde::formats::Json};

#[tarpc::service]
pub trait LspToGui {
    async fn hello(name: String) -> String;
}

pub struct SyncGuiToLspClient {
    executor: BackgroundExecutor,
    client: GuiToLspClient,
}

impl SyncGuiToLspClient {
    pub fn new(executor: BackgroundExecutor, lsp_addr: SocketAddr) -> Self {
        let client = executor.block(
            async {
                let mut transport = tarpc::serde_transport::tcp::connect(lsp_addr, Json::default);
                transport.config_mut().max_frame_length(usize::MAX);

                GuiToLspClient::new(tarpc::client::Config::default(), transport.await.unwrap())
                    .spawn()
            }
            .compat(),
        );
        Self { executor, client };
    }

    pub fn hello(&self) -> Result<String, tarpc::client::RpcError> {
        self.executor.block(
            async {
                self.client
                    .hello(context::current(), "world".to_string())
                    .await
            }
            .compat(),
        )
    }
}
