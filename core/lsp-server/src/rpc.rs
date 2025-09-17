use std::net::SocketAddr;

use compiler::compile::CompiledCell;

use tarpc::tokio_serde::formats::Json;

use crate::SharedState;

#[tarpc::service]
pub trait GuiToLsp {
    async fn register(addr: SocketAddr);
}

#[tarpc::service]
pub trait LspToGui {
    async fn open_cell(cell: CompiledCell);
}

#[derive(Clone)]
pub struct LspServer {
    pub state: SharedState,
}

impl GuiToLsp for LspServer {
    async fn register(self, _: tarpc::context::Context, addr: SocketAddr) -> () {
        *self.state.gui_client.lock().await = Some({
            let mut transport = tarpc::serde_transport::tcp::connect(addr, Json::default);
            transport.config_mut().max_frame_length(usize::MAX);

            LspToGuiClient::new(tarpc::client::Config::default(), transport.await.unwrap()).spawn()
        });
    }
}
