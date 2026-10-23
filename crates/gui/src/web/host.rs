//! Messages between the GUI and the page that hosts it.
//!
//! The page relays analyzer connections, since a browser cannot open TCP
//! connections. It opens each one, authenticates it, and passes whole
//! frames in both directions.

use std::{cell::RefCell, collections::HashMap};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use js_sys::{Object, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};

/// What happens to a relayed connection.
#[derive(Debug)]
pub(crate) enum Event {
    /// The page connected and authenticated.
    Opened,
    Frame(Vec<u8>),
    Closed(String),
}

#[derive(Default)]
struct Host {
    post: Option<js_sys::Function>,
    next_connection: u32,
    connections: HashMap<u32, UnboundedSender<Event>>,
}

thread_local! {
    static HOST: RefCell<Host> = RefCell::new(Host::default());
}

/// Sets the function the GUI posts its messages to.
pub(crate) fn set_post(post: js_sys::Function) {
    HOST.with_borrow_mut(|host| host.post = Some(post));
}

fn post(fields: &[(&str, JsValue)]) {
    let message = Object::new();
    for (key, value) in fields {
        let _ = Reflect::set(&message, &JsValue::from_str(key), value);
    }
    let post = HOST.with_borrow(|host| host.post.clone());
    if let Some(post) = post
        && let Err(error) = post.call1(&JsValue::NULL, &message)
    {
        tracing::error!("could not post a message to the page: {error:?}");
    }
}

/// Asks the page to show the GUI.
pub fn reveal() {
    post(&[("type", "reveal".into())]);
}

/// Asks the page to focus the source editor.
pub fn focus_editor() {
    post(&[("type", "focusEditor".into())]);
}

/// Asks the page to open a connection to the analyzer as `role`, returning
/// its id and the events that follow.
pub(crate) fn connect(role: &str) -> (u32, UnboundedReceiver<Event>) {
    let (sender, receiver) = unbounded();
    let id = HOST.with_borrow_mut(|host| {
        let id = host.next_connection;
        host.next_connection += 1;
        host.connections.insert(id, sender);
        id
    });
    post(&[
        ("type", "connect".into()),
        ("id", id.into()),
        ("role", role.into()),
    ]);
    (id, receiver)
}

/// Sends one frame on connection `id`.
pub(crate) fn send(id: u32, frame: &[u8]) {
    post(&[
        ("type", "frame".into()),
        ("id", id.into()),
        ("data", Uint8Array::from(frame).into()),
    ]);
}

/// Closes connection `id`.
pub(crate) fn close(id: u32) {
    HOST.with_borrow_mut(|host| host.connections.remove(&id));
    post(&[("type", "close".into()), ("id", id.into())]);
}

/// Handles one message from the page.
pub(crate) fn receive(message: JsValue) {
    let field = |key: &str| Reflect::get(&message, &JsValue::from_str(key)).ok();
    let Some(kind) = field("type").and_then(|kind| kind.as_string()) else {
        tracing::warn!("ignored a message without a type from the page");
        return;
    };
    let Some(id) = field("id").and_then(|id| id.as_f64()).map(|id| id as u32) else {
        tracing::warn!("ignored a `{kind}` message without a connection id");
        return;
    };
    let event = match kind.as_str() {
        "opened" => Event::Opened,
        "frame" => match field("data").and_then(|data| data.dyn_into::<Uint8Array>().ok()) {
            Some(data) => Event::Frame(data.to_vec()),
            None => {
                tracing::warn!("ignored a frame without data");
                return;
            }
        },
        "closed" => Event::Closed(
            field("reason")
                .and_then(|reason| reason.as_string())
                .unwrap_or_else(|| "the analyzer closed the connection".to_owned()),
        ),
        _ => {
            tracing::warn!("ignored an unknown `{kind}` message from the page");
            return;
        }
    };
    let closed = matches!(event, Event::Closed(_));
    HOST.with_borrow_mut(|host| {
        if let Some(connection) = host.connections.get(&id) {
            let _ = connection.unbounded_send(event);
        }
        if closed {
            host.connections.remove(&id);
        }
    });
}
