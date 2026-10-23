//! The analyzer connection from a browser, relayed by the page that hosts the GUI.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

use anyhow::{Result, anyhow};
use futures::{
    FutureExt, StreamExt,
    channel::{mpsc::UnboundedReceiver, oneshot},
};
use gpui::{AsyncApp, Task};
use protocol::{
    rpc::{GuiRequest, LangServerRequest, LangServerResponse},
    wire,
};

use super::{CallError, GuiServer, LANG_SERVER_CLIENT_TIMEOUT};
use crate::web::host::{self, Event};

/// The page that relays connections to the analyzer.
#[derive(Clone, Debug)]
pub(crate) struct Endpoint;

type Reply = oneshot::Sender<Result<LangServerResponse, CallError>>;

/// A connection that sends requests to the analyzer.
#[derive(Clone)]
pub(crate) struct Channel(Rc<ChannelState>);

struct ChannelState {
    connection: Connection,
    next_request: Cell<u64>,
    pending: RefCell<HashMap<u64, Reply>>,
    closed: Cell<bool>,
    _reader: RefCell<Option<Task<()>>>,
}

/// A relayed connection, which the page closes when this is dropped.
struct Connection(u32);

impl Drop for Connection {
    fn drop(&mut self) {
        host::close(self.0);
    }
}

/// Asks the page for a connection as `role` and waits until it is open.
async fn open(role: &str) -> Result<(u32, UnboundedReceiver<Event>)> {
    let (id, mut events) = host::connect(role);
    match events.next().await {
        Some(Event::Opened) => Ok((id, events)),
        Some(Event::Closed(reason)) => Err(anyhow!(reason)),
        Some(Event::Frame(_)) => Err(anyhow!("the analyzer sent a frame before connecting")),
        None => Err(anyhow!("the page dropped the connection")),
    }
}

impl Channel {
    pub(crate) async fn connect(app: &AsyncApp, _: &Endpoint) -> Result<Self> {
        let (id, mut events) = open("gui").await?;
        let state = Rc::new(ChannelState {
            connection: Connection(id),
            next_request: Cell::new(0),
            pending: RefCell::default(),
            closed: Cell::new(false),
            _reader: RefCell::default(),
        });
        let weak = Rc::downgrade(&state);
        let reader = app.spawn(async move |_| {
            while let Some(event) = events.next().await {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                match event {
                    Event::Frame(frame) => state.answer(&frame),
                    Event::Closed(reason) => {
                        state.close(&reason);
                        return;
                    }
                    Event::Opened => {}
                }
            }
            if let Some(state) = Weak::upgrade(&weak) {
                state.close("the page dropped the connection");
            }
        });
        *state._reader.borrow_mut() = Some(reader);
        Ok(Self(state))
    }

    pub(crate) async fn call(
        &self,
        app: &AsyncApp,
        request: LangServerRequest,
    ) -> Result<LangServerResponse, CallError> {
        let state = &self.0;
        if state.closed.get() {
            return Err(CallError::Disconnected(anyhow!("the connection is closed")));
        }
        let id = state.next_request.get();
        state.next_request.set(id + 1);
        let frame = postcard::to_stdvec(&wire::ClientMessage::request(id, request))
            .map_err(|error| CallError::Failed(error.into()))?;
        let (reply, response) = oneshot::channel();
        state.pending.borrow_mut().insert(id, reply);
        host::send(state.connection.0, &frame);
        let timeout = app
            .background_executor()
            .timer(LANG_SERVER_CLIENT_TIMEOUT)
            .fuse();
        futures::pin_mut!(timeout);
        let mut response = response.fuse();
        futures::select! {
            response = response => response.unwrap_or_else(|_| {
                Err(CallError::Disconnected(anyhow!("the connection closed")))
            }),
            _ = timeout => {
                state.pending.borrow_mut().remove(&id);
                Err(CallError::Timeout)
            }
        }
    }

    /// Opens the connection the analyzer calls the GUI back over, returning
    /// the task that serves it.
    pub(crate) async fn serve_callbacks(
        app: &AsyncApp,
        _: &Endpoint,
        server: GuiServer,
    ) -> Result<Task<()>> {
        let (id, mut events) = open("callback").await?;
        let connection = Connection(id);
        Ok(app.spawn(async move |cx| {
            let _connection = connection;
            while let Some(event) = events.next().await {
                let frame = match event {
                    Event::Frame(frame) => frame,
                    Event::Closed(reason) => {
                        tracing::info!("the analyzer closed the callback connection: {reason}");
                        return;
                    }
                    Event::Opened => continue,
                };
                let request = match postcard::from_bytes::<wire::ClientMessage<GuiRequest>>(&frame)
                {
                    Ok(wire::ClientMessage::Request(request)) => request,
                    Ok(wire::ClientMessage::Cancel { .. }) => continue,
                    Err(error) => {
                        tracing::error!("could not decode a request from the analyzer: {error}");
                        continue;
                    }
                };
                let server = server.clone();
                cx.spawn(async move |_| {
                    let response = tarpc::Response {
                        request_id: request.id,
                        message: Ok(server.handle(request.message).await),
                    };
                    match postcard::to_stdvec(&response) {
                        Ok(frame) => host::send(id, &frame),
                        Err(error) => tracing::error!("could not encode a response: {error}"),
                    }
                })
                .detach();
            }
        }))
    }
}

impl ChannelState {
    /// Completes the request that `frame` answers.
    fn answer(&self, frame: &[u8]) {
        let response = match postcard::from_bytes::<tarpc::Response<LangServerResponse>>(frame) {
            Ok(response) => response,
            Err(error) => {
                tracing::error!("could not decode a response from the analyzer: {error}");
                return;
            }
        };
        let Some(reply) = self.pending.borrow_mut().remove(&response.request_id) else {
            return;
        };
        let _ = reply.send(
            response
                .message
                .map_err(|error| CallError::Failed(anyhow!("{}", error.detail))),
        );
    }

    fn close(&self, reason: &str) {
        self.closed.set(true);
        for (_, reply) in self.pending.borrow_mut().drain() {
            let _ = reply.send(Err(CallError::Disconnected(anyhow!("{reason}"))));
        }
    }
}
