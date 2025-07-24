use std::io::{Read, Write};

use lsp_server::socket::{GuiToLspMessage, LspToGuiMessage};

#[derive(Debug)]
pub struct GuiToLsp<T> {
    io: T,
}

#[derive(Debug)]
pub struct GuiFromLsp<T> {
    io: T,
}

impl<T: Write> GuiToLsp<T> {
    pub fn new(io: T) -> Self {
        Self { io }
    }

    pub fn send(&mut self, msg: GuiToLspMessage) {
        let msg = serde_json::to_vec(&msg).unwrap();
        let tmp = msg.len() as u32;
        let b = tmp.to_be_bytes();
        self.io.write_all(&b).unwrap();
        self.io.write_all(&msg).unwrap();
        self.io.flush().unwrap();
    }
}

impl<T: Read> GuiFromLsp<T> {
    pub fn new(io: T) -> Self {
        Self { io }
    }

    pub fn read(&mut self) -> LspToGuiMessage {
        let mut buf = [0; 4];
        self.io.read_exact(&mut buf).unwrap();
        let len = u32::from_be_bytes(buf);
        let mut buf = vec![0; len as usize];
        self.io.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }
}
