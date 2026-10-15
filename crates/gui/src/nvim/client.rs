//! A msgpack-RPC client for Neovim's `--embed` protocol.

use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, BufWriter, Write},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use futures::channel::{
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
    oneshot,
};
use rmpv::Value;

use super::redraw::{RedrawEvent, parse_redraw};

const REQUEST: u64 = 0;
const RESPONSE: u64 = 1;
const NOTIFICATION: u64 = 2;

/// What the reader thread reports from Neovim.
#[derive(Debug)]
pub(crate) enum NvimEvent {
    /// The events of one redraw, ending with `flush`.
    Frame(Vec<RedrawEvent>),
    Exited(ExitStatus),
}

type Reply = Result<Value, Value>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;

fn lock(pending: &Pending) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<Reply>>> {
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Shared {
    writer: mpsc::Sender<Vec<u8>>,
    next_id: AtomicU64,
    pending: Pending,
}

/// Sends requests and notifications to one Neovim process. Dropping every
/// clone closes Neovim's stdin.
#[derive(Clone)]
pub(crate) struct NvimClient(Arc<Shared>);

/// Starts `command` with piped stdio and the threads that serve it.
pub(crate) fn spawn(
    mut command: Command,
) -> io::Result<(NvimClient, UnboundedReceiver<NvimEvent>)> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (writer, messages) = mpsc::channel();
    let pending = Pending::default();
    let shared = Arc::new(Shared {
        writer,
        next_id: AtomicU64::new(1),
        pending: pending.clone(),
    });
    let (events, receiver) = unbounded();
    let weak = Arc::downgrade(&shared);
    thread::spawn(move || write_messages(stdin, messages));
    thread::spawn(move || read_messages(child, stdout, pending, weak, events));
    thread::spawn(move || forward_stderr(stderr));
    Ok((NvimClient(shared), receiver))
}

fn write_messages(stdin: ChildStdin, messages: mpsc::Receiver<Vec<u8>>) {
    let mut writer = BufWriter::new(stdin);
    while let Ok(message) = messages.recv() {
        let mut result = writer.write_all(&message);
        while result.is_ok()
            && let Ok(message) = messages.try_recv()
        {
            result = writer.write_all(&message);
        }
        if let Err(error) = result.and_then(|()| writer.flush()) {
            tracing::warn!("could not write to Neovim: {error}");
            return;
        }
    }
}

fn forward_stderr(stderr: ChildStderr) {
    for line in BufReader::new(stderr).lines() {
        match line {
            Ok(line) => tracing::warn!("nvim: {line}"),
            Err(_) => return,
        }
    }
}

fn is_eof(error: &rmpv::decode::Error) -> bool {
    match error {
        rmpv::decode::Error::InvalidMarkerRead(error)
        | rmpv::decode::Error::InvalidDataRead(error) => {
            error.kind() == io::ErrorKind::UnexpectedEof
        }
        rmpv::decode::Error::DepthLimitExceeded => false,
    }
}

fn read_messages(
    mut child: Child,
    stdout: ChildStdout,
    pending: Pending,
    client: Weak<Shared>,
    events: UnboundedSender<NvimEvent>,
) {
    let mut reader = BufReader::new(stdout);
    let mut frame = Vec::new();
    loop {
        let message = match rmpv::decode::read_value(&mut reader) {
            Ok(message) => message,
            Err(error) => {
                if !is_eof(&error) {
                    tracing::error!("could not decode a message from Neovim: {error}");
                    let _ = child.kill();
                }
                break;
            }
        };
        let Value::Array(fields) = message else {
            tracing::warn!("Neovim sent a message that is not an array");
            continue;
        };
        match fields.as_slice() {
            [kind, id, method, _] if kind.as_u64() == Some(REQUEST) => {
                let method = method.as_str().unwrap_or_default();
                if let Some(client) = client.upgrade() {
                    NvimClient(client).send(&Value::Array(vec![
                        RESPONSE.into(),
                        id.clone(),
                        format!("argone: unknown method {method}").into(),
                        Value::Nil,
                    ]));
                }
            }
            [kind, id, error, result] if kind.as_u64() == Some(RESPONSE) => {
                let sender = id.as_u64().and_then(|id| lock(&pending).remove(&id));
                if let Some(sender) = sender {
                    let _ = sender.send(if error.is_nil() {
                        Ok(result.clone())
                    } else {
                        Err(error.clone())
                    });
                }
            }
            [kind, method, params] if kind.as_u64() == Some(NOTIFICATION) => {
                let params = params.as_array().map(Vec::as_slice).unwrap_or_default();
                match method.as_str() {
                    Some("redraw") => {
                        for event in parse_redraw(params) {
                            let flush = event == RedrawEvent::Flush;
                            frame.push(event);
                            if flush {
                                let _ = events
                                    .unbounded_send(NvimEvent::Frame(std::mem::take(&mut frame)));
                            }
                        }
                    }
                    Some("nvim_error_event") => {
                        let message = params.get(1).and_then(Value::as_str).unwrap_or_default();
                        tracing::error!("Neovim: {message}");
                    }
                    _ => {}
                }
            }
            _ => tracing::warn!("Neovim sent a malformed message"),
        }
    }
    lock(&pending).clear();
    match child.wait() {
        Ok(status) => {
            let _ = events.unbounded_send(NvimEvent::Exited(status));
        }
        Err(error) => tracing::error!("could not wait for Neovim: {error}"),
    }
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(key, value)| (Value::from(key), value))
            .collect(),
    )
}

impl NvimClient {
    /// A client whose messages arrive, encoded, on the returned receiver.
    #[cfg(test)]
    pub(crate) fn for_test() -> (NvimClient, mpsc::Receiver<Vec<u8>>) {
        let (writer, messages) = mpsc::channel();
        let shared = Shared {
            writer,
            next_id: AtomicU64::new(1),
            pending: Pending::default(),
        };
        (NvimClient(Arc::new(shared)), messages)
    }

    fn send(&self, message: &Value) {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, message).expect("encoding into a Vec cannot fail");
        let _ = self.0.writer.send(bytes);
    }

    pub(crate) fn notify(&self, method: &str, args: Vec<Value>) {
        self.send(&Value::Array(vec![
            NOTIFICATION.into(),
            method.into(),
            Value::Array(args),
        ]));
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the pane sends only notifications")
    )]
    pub(crate) fn request(&self, method: &str, args: Vec<Value>) -> oneshot::Receiver<Reply> {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        lock(&self.0.pending).insert(id, sender);
        self.send(&Value::Array(vec![
            REQUEST.into(),
            id.into(),
            method.into(),
            Value::Array(args),
        ]));
        receiver
    }

    /// Identifies this client, then attaches it as a UI of `cols` × `rows`.
    pub(crate) fn ui_attach(&self, cols: u16, rows: u16) {
        self.notify(
            "nvim_set_client_info",
            vec![
                "argone".into(),
                map(vec![]),
                "ui".into(),
                map(vec![]),
                map(vec![]),
            ],
        );
        self.notify(
            "nvim_ui_attach",
            vec![
                cols.into(),
                rows.into(),
                map(vec![("rgb", true.into()), ("ext_linegrid", true.into())]),
            ],
        );
    }

    pub(crate) fn ui_try_resize(&self, cols: u16, rows: u16) {
        self.notify("nvim_ui_try_resize", vec![cols.into(), rows.into()]);
    }

    /// Sends keys in Neovim's key notation.
    pub(crate) fn input(&self, keys: &str) {
        self.notify("nvim_input", vec![keys.into()]);
    }

    pub(crate) fn input_mouse(
        &self,
        button: &str,
        action: &str,
        modifiers: &str,
        row: u16,
        col: u16,
    ) {
        self.notify(
            "nvim_input_mouse",
            vec![
                button.into(),
                action.into(),
                modifiers.into(),
                0.into(),
                row.into(),
                col.into(),
            ],
        );
    }

    /// Pastes `text` in a single call, as a terminal's bracketed paste does.
    pub(crate) fn paste(&self, text: &str) {
        self.notify("nvim_paste", vec![text.into(), true.into(), (-1).into()]);
    }

    pub(crate) fn ui_set_focus(&self, focused: bool) {
        self.notify("nvim_ui_set_focus", vec![focused.into()]);
    }

    pub(crate) fn set_var(&self, name: &str, value: Value) {
        self.notify("nvim_set_var", vec![name.into(), value]);
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the pane sends commands as keys")
    )]
    pub(crate) fn command(&self, command: &str) {
        self.notify("nvim_command", vec![command.into()]);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::nvim::ui::UiState;

    const WAIT: Duration = Duration::from_secs(10);

    /// Waits for the next event, or panics after [`WAIT`].
    fn next_event(events: &mut UnboundedReceiver<NvimEvent>, waiting_for: &str) -> NvimEvent {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(event) = events.try_recv() {
                return event;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {waiting_for}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn nvim_command(directory: &std::path::Path) -> Command {
        let mut command = Command::new("nvim");
        command
            .arg("--embed")
            .arg("--clean")
            .arg("-n")
            .current_dir(directory)
            .env("NVIM_LOG_FILE", directory.join("nvim.log"))
            .env("XDG_CONFIG_HOME", directory.join("config"))
            .env("XDG_DATA_HOME", directory.join("data"))
            .env("XDG_STATE_HOME", directory.join("state"));
        command
    }

    #[test]
    fn drives_an_embedded_neovim() {
        let directory = tempfile::tempdir().unwrap();
        let (client, mut events) = spawn(nvim_command(directory.path())).unwrap();

        let mode = client.request("nvim_get_mode", vec![]);
        client.ui_attach(40, 10);
        client.input("ihello<Esc>");
        let mut ui = UiState::default();
        while !(ui.height() > 0 && ui.row_text(0).starts_with("hello")) {
            match next_event(&mut events, "`hello` on row 0") {
                NvimEvent::Frame(frame) => frame.into_iter().for_each(|event| ui.apply(event)),
                NvimEvent::Exited(status) => panic!("Neovim exited early ({status})"),
            }
        }
        assert_eq!((ui.width(), ui.height()), (40, 10));
        let mode = futures::executor::block_on(mode).unwrap().unwrap();
        assert!(mode.as_map().is_some(), "unexpected reply {mode}");

        client.ui_try_resize(30, 8);
        loop {
            match next_event(&mut events, "the resize") {
                NvimEvent::Frame(frame)
                    if frame.contains(&RedrawEvent::GridResize {
                        width: 30,
                        height: 8,
                    }) =>
                {
                    break;
                }
                NvimEvent::Frame(_) => {}
                NvimEvent::Exited(status) => panic!("Neovim exited early ({status})"),
            }
        }

        client.command("qa!");
        loop {
            if let NvimEvent::Exited(status) = next_event(&mut events, "Neovim to exit") {
                assert!(status.success(), "Neovim exited with {status}");
                break;
            }
        }
    }

    #[test]
    fn requests_from_neovim_get_an_error_instead_of_hanging() {
        let directory = tempfile::tempdir().unwrap();
        let (client, mut events) = spawn(nvim_command(directory.path())).unwrap();
        let reply = client.request(
            "nvim_exec_lua",
            vec![
                "for _, channel in ipairs(vim.api.nvim_list_chans()) do \
                   if channel.stream == 'stdio' then \
                     local _, err = pcall(vim.rpcrequest, channel.id, 'anything'); return err \
                   end \
                 end"
                .into(),
                Value::Array(vec![]),
            ],
        );
        let reply = futures::executor::block_on(reply).unwrap().unwrap();
        assert!(
            reply
                .as_str()
                .is_some_and(|error| error.contains("argone: unknown method anything")),
            "unexpected reply {reply}"
        );
        drop(client);
        loop {
            if let NvimEvent::Exited(_) = next_event(&mut events, "Neovim to exit") {
                break;
            }
        }
    }
}
