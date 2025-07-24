use bytes::{Buf, BufMut, BytesMut};
use cfgrammar::Span;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuiToLspMessage {
    SelectedRect(SelectedRectMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedRectMessage {
    pub rect: u64,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LspToGuiMessage {
    ByeBye,
}

#[derive(Debug)]
pub struct LspToGui<T> {
    io: FramedWrite<T, LengthDelimitedCodec>,
}

#[derive(Debug)]
pub struct LspFromGui<T> {
    io: FramedRead<T, LengthDelimitedCodec>,
}

impl<T: AsyncWrite + Unpin> LspToGui<T> {
    pub fn new(io: T) -> Self {
        let io = FramedWrite::new(io, LengthDelimitedCodec::new());
        Self { io }
    }

    pub async fn send(&mut self, msg: LspToGuiMessage) {
        let mut b = BytesMut::new();
        let mut writer = b.writer();
        serde_json::to_writer(&mut writer, &msg).unwrap();
        let msg = writer.into_inner().freeze();
        self.io.send(msg).await.unwrap();
    }
}

impl<T: AsyncRead + Unpin> LspFromGui<T> {
    pub fn new(io: T) -> Self {
        let io = FramedRead::new(io, LengthDelimitedCodec::new());
        Self { io }
    }

    pub async fn read(&mut self) -> GuiToLspMessage {
        let x = self.io.next().await.unwrap().unwrap();
        serde_json::from_reader::<_, GuiToLspMessage>(x.freeze().reader()).unwrap()
    }
}
