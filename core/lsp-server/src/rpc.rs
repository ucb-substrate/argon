use std::{collections::VecDeque, net::SocketAddr, path::PathBuf, sync::Arc};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use cfgrammar::Span;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    sync::{oneshot::Sender, Mutex},
};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use futures::prelude::*;
use tarpc::{
    client, context,
    server::{self, Channel},
    tokio_serde::formats::Json,
};
use tower_lsp::lsp_types::MessageType;

use crate::SharedState;

#[tarpc::service]
pub trait GuiToLsp {
    async fn register(addr: SocketAddr);
}

#[tarpc::service]
pub trait LspToGui {
    async fn bye(name: String) -> String;
}

#[derive(Clone)]
pub struct LspServer {
    pub state: SharedState,
}

impl GuiToLsp for LspServer {
    async fn register(self, _: tarpc::context::Context, addr: SocketAddr) -> () {
        let editor_client = self.state.editor_client.clone();
        *self.state.gui_client.lock().await = Some({
            let mut transport = tarpc::serde_transport::tcp::connect(addr, Json::default);
            transport.config_mut().max_frame_length(usize::MAX);

            let client =
                LspToGuiClient::new(tarpc::client::Config::default(), transport.await.unwrap())
                    .spawn();
            let out = client
                .bye(context::current(), "world".to_string())
                .await
                .unwrap();
            editor_client.log_message(MessageType::INFO, out).await;

            client
        });
    }
}
