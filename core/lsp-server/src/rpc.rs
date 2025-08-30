use std::{collections::VecDeque, path::PathBuf, sync::Arc};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use cfgrammar::Span;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{Mutex, oneshot::Sender},
};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use futures::prelude::*;
use tarpc::{
    client, context,
    server::{self, Channel},
};

use crate::SharedState;

#[tarpc::service]
pub trait GuiToLsp {
    async fn hello(name: String) -> String;
}

#[derive(Clone)]
pub struct LspServer {
    pub state: SharedState,
}

impl GuiToLsp for LspServer {
    async fn hello(self, _: context::Context, name: String) -> String {
        format!("Hello, {name}!")
    }
}
