use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::OnceCell,
};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub struct GuiSocket<T> {
    io: Framed<T, LengthDelimitedCodec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuiToLspMessage {
    Hello,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LspToGuiMessage {
    ByeBye,
}

impl<T: AsyncRead + AsyncWrite> GuiSocket<T> {
    pub fn new(io: T) -> Self {
        let io = Framed::new(io, LengthDelimitedCodec::new());
        Self { io }
    }
    pub fn send(&mut self, msg: LspToGuiMessage) {
        // self.inner.get_mut().unwrap().write_all(src);
    }
}
