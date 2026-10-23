//! tarpc's request messages, in a form that can be built without a clock.
//!
//! tarpc stamps each request with an `Instant` deadline, and the browser has
//! no `Instant`. These types encode to the same postcard bytes as tarpc's own,
//! carrying the deadline as the time remaining, which is how tarpc sends it.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tarpc::trace;

/// How long the peer has to answer a request.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// [`tarpc::ClientMessage`].
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage<T> {
    Request(Request<T>),
    Cancel {
        trace_context: trace::Context,
        request_id: u64,
    },
}

/// [`tarpc::Request`].
#[derive(Debug, Serialize, Deserialize)]
pub struct Request<T> {
    pub context: Context,
    pub id: u64,
    pub message: T,
}

/// [`tarpc::context::Context`], with the deadline as the time remaining.
#[derive(Debug, Serialize, Deserialize)]
pub struct Context {
    pub deadline: Duration,
    pub trace_context: trace::Context,
}

impl<T> ClientMessage<T> {
    /// The request `message` with id `id`, due within [`DEADLINE`].
    pub fn request(id: u64, message: T) -> Self {
        Self::Request(Request {
            context: Context {
                deadline: DEADLINE,
                trace_context: trace::Context::default(),
            },
            id,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{GuiRequest, LangServerRequest};

    #[test]
    fn requests_decode_as_tarpc_requests() {
        let bytes = postcard::to_stdvec(&ClientMessage::request(
            7,
            LangServerRequest::OpenCell {
                cell: "top()".to_owned(),
            },
        ))
        .unwrap();
        let tarpc::ClientMessage::Request(request) =
            postcard::from_bytes::<tarpc::ClientMessage<LangServerRequest>>(&bytes).unwrap()
        else {
            panic!("expected a request");
        };
        assert_eq!(request.id, 7);
        assert!(matches!(
            request.message,
            LangServerRequest::OpenCell { cell } if cell == "top()"
        ));
        let remaining = request
            .context
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        assert!(remaining > DEADLINE - Duration::from_secs(1) && remaining <= DEADLINE);
    }

    #[test]
    fn tarpc_requests_and_cancellations_decode() {
        let request = tarpc::ClientMessage::Request(tarpc::Request {
            context: tarpc::context::current(),
            id: 3,
            message: GuiRequest::WorkspaceModified { modified: true },
        });
        let decoded: ClientMessage<GuiRequest> =
            postcard::from_bytes(&postcard::to_stdvec(&request).unwrap()).unwrap();
        let ClientMessage::Request(Request {
            id: 3,
            message: GuiRequest::WorkspaceModified { modified: true },
            ..
        }) = decoded
        else {
            panic!("decoded {decoded:?}");
        };

        let cancel = tarpc::ClientMessage::<GuiRequest>::Cancel {
            trace_context: trace::Context::default(),
            request_id: 9,
        };
        let decoded: ClientMessage<GuiRequest> =
            postcard::from_bytes(&postcard::to_stdvec(&cancel).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            ClientMessage::Cancel { request_id: 9, .. }
        ));
    }
}
