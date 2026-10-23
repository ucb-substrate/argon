//! The analyzer connection over TCP, using tarpc.

use std::net::SocketAddr;

use analyzer::transport::{self, Role, SessionToken};
use anyhow::{Result, anyhow};
use async_compat::CompatExt;
use futures::prelude::*;
use gpui::{AsyncApp, Task};
use protocol::rpc::{GuiRequest, GuiResponse, LangServerRequest, LangServerResponse};
use tarpc::{
    client::RpcError,
    context,
    server::{Channel as _, Serve},
};

use super::{CallError, GuiServer, LANG_SERVER_CLIENT_TIMEOUT};

/// Where the analyzer listens, and the session token it expects.
#[derive(Clone, Debug)]
pub(crate) struct Endpoint {
    pub addr: SocketAddr,
    pub token: SessionToken,
}

#[cfg(test)]
pub(crate) type TestLangServerTransport = tarpc::transport::channel::UnboundedChannel<
    tarpc::ClientMessage<LangServerRequest>,
    tarpc::Response<LangServerResponse>,
>;

/// A connection that sends requests to the analyzer.
#[derive(Clone)]
pub(crate) struct Channel(tarpc::client::Channel<LangServerRequest, LangServerResponse>);

impl Channel {
    pub(crate) async fn connect(app: &AsyncApp, endpoint: &Endpoint) -> Result<Self> {
        let Endpoint { addr, token } = endpoint.clone();
        let channel = app
            .background_executor()
            .spawn(
                async move {
                    tokio::time::timeout(LANG_SERVER_CLIENT_TIMEOUT, async move {
                        let stream = transport::connect(addr, &token, Role::Gui).await?;
                        let transport =
                            tarpc::serde_transport::new(stream, transport::Postcard::default());
                        std::io::Result::Ok(
                            tarpc::client::new(tarpc::client::Config::default(), transport).spawn(),
                        )
                    })
                    .await?
                }
                .compat(),
            )
            .await?;
        Ok(Self(channel))
    }

    /// A channel over `transport`, for tests that answer requests themselves.
    #[cfg(test)]
    pub(crate) fn from_transport(
        app: &AsyncApp,
        transport: tarpc::transport::channel::UnboundedChannel<
            tarpc::Response<LangServerResponse>,
            tarpc::ClientMessage<LangServerRequest>,
        >,
    ) -> Self {
        let client = tarpc::client::new(tarpc::client::Config::default(), transport);
        app.background_executor()
            .spawn(client.dispatch.compat())
            .detach();
        Self(client.client)
    }

    pub(crate) async fn call(
        &self,
        app: &AsyncApp,
        request: LangServerRequest,
    ) -> Result<LangServerResponse, CallError> {
        let channel = self.0.clone();
        app.background_executor()
            .spawn(
                async move {
                    let call = channel.call(context::current(), request);
                    match tokio::time::timeout(LANG_SERVER_CLIENT_TIMEOUT, call).await {
                        Ok(Ok(response)) => Ok(response),
                        Ok(Err(error)) if is_disconnected(&error) => {
                            Err(CallError::Disconnected(error.into()))
                        }
                        Ok(Err(error)) => Err(CallError::Failed(error.into())),
                        Err(_) => Err(CallError::Timeout),
                    }
                }
                .compat(),
            )
            .await
    }

    /// Opens the connection the analyzer calls the GUI back over, returning
    /// the task that serves it.
    pub(crate) async fn serve_callbacks(
        app: &AsyncApp,
        endpoint: &Endpoint,
        server: GuiServer,
    ) -> Result<Task<()>> {
        let Endpoint { addr, token } = endpoint.clone();
        let stream = app
            .background_executor()
            .spawn(
                async move { transport::connect(addr, &token, Role::GuiCallback).await }.compat(),
            )
            .await
            .map_err(|error| anyhow!(error))?;
        let background_executor = app.background_executor().clone();
        Ok(app.background_executor().spawn(
            async move {
                let transport = tarpc::serde_transport::new(stream, transport::Postcard::default());
                tarpc::server::BaseChannel::with_defaults(transport)
                    .execute(GuiService(server))
                    .for_each(|response| background_executor.spawn(response))
                    .await;
            }
            .compat(),
        ))
    }
}

fn is_disconnected(error: &RpcError) -> bool {
    matches!(
        error,
        RpcError::Shutdown | RpcError::Send(_) | RpcError::Channel(_)
    )
}

/// Serves the analyzer's calls with a [`GuiServer`].
#[derive(Clone)]
struct GuiService(GuiServer);

impl Serve for GuiService {
    type Req = GuiRequest;
    type Resp = GuiResponse;

    async fn serve(
        self,
        _: context::Context,
        request: GuiRequest,
    ) -> Result<GuiResponse, tarpc::ServerError> {
        Ok(self.0.handle(request).await)
    }
}

#[cfg(test)]
mod tests {
    use super::is_disconnected;

    #[test]
    fn reconnects_only_for_transport_failures() {
        assert!(is_disconnected(&tarpc::client::RpcError::Shutdown));
        assert!(!is_disconnected(&tarpc::client::RpcError::DeadlineExceeded));
    }
}
