//! providers/cursor/transport.rs — full-duplex Connect stream over HTTP/2.
//!
//! Cursor's `AgentService/Run` is one bidirectional stream: the client keeps
//! writing Connect frames (exec results, interaction responses, KV blob
//! replies, heartbeats) for as long as it keeps reading server frames. reqwest
//! carries this with a streaming request body fed from an mpsc channel while the
//! response body streams back concurrently — an ordinary buffered request would
//! deadlock, because the server does not finish its response until the client
//! has answered every tool it asked for.
//!
//! HTTP/2 is negotiated by ALPN against `api2.cursor.sh`; the fixed headers are
//! the ones the Cursor CLI sends. Frames are uncompressed on write, and the
//! shared `ConnectDecoder` reassembles server frames as bytes arrive.

use std::pin::Pin;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use mikmik_core::provider_id::ProviderId;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::protocol::connect::{encode_frame, ConnectDecoder, ConnectFrame};
use crate::provider_error::ProviderError;

/// The Cursor agent API host.
pub const CURSOR_API_URL: &str = "https://api2.cursor.sh";

/// The single Run RPC path that carries the whole agent stream.
pub const CURSOR_RUN_PATH: &str = "/agent.v1.AgentService/Run";

/// The client version the Cursor CLI advertises.
pub const CURSOR_CLIENT_VERSION: &str = "cli-2026.07.23-e383d2b";

type FrameItem = Result<Bytes, std::io::Error>;

/// A live bidirectional Cursor Run stream.
pub struct CursorConnection {
    id: ProviderId,
    sender: mpsc::UnboundedSender<FrameItem>,
    incoming: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    decoder: ConnectDecoder,
}

impl CursorConnection {
    /// Open the stream and send the first client frame (the run request).
    ///
    /// The response is returned once its headers arrive, while the request body
    /// stays open for further client frames — the full-duplex contract the
    /// exec channel depends on.
    pub async fn open(
        client: &reqwest::Client,
        id: ProviderId,
        token: &str,
        first_frame: &[u8],
    ) -> Result<Self, ProviderError> {
        let (sender, rx) = mpsc::unbounded_channel::<FrameItem>();
        // Seed the run request so the server can begin before any further frame.
        let seeded = sender.send(Ok(Bytes::from(encode_frame(0, first_frame))));
        if seeded.is_err() {
            return Err(ProviderError::ServerError {
                provider: id,
                status: None,
                message: "Cursor request body channel closed before send".to_string(),
                is_retryable: false,
            });
        }
        let body = reqwest::Body::wrap_stream(UnboundedReceiverStream::new(rx));

        let resp = client
            .post(format!("{CURSOR_API_URL}{CURSOR_RUN_PATH}"))
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("te", "trailers")
            .header("authorization", format!("Bearer {token}"))
            .header("x-ghost-mode", "true")
            .header("x-cursor-client-version", CURSOR_CLIENT_VERSION)
            .header("x-cursor-client-type", "cli")
            .header("x-request-id", uuid::Uuid::new_v4().to_string())
            .body(body)
            .send()
            .await
            .map_err(|e| ProviderError::ServerError {
                provider: id.clone(),
                status: None,
                message: e.to_string(),
                is_retryable: true,
            })?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other {
                provider: id,
                message: format!("Cursor Run error {status}"),
                status: Some(status),
                body: Some(text),
            });
        }

        Ok(Self {
            id,
            sender,
            incoming: Box::pin(resp.bytes_stream()),
            decoder: ConnectDecoder::new(),
        })
    }

    /// Queue one client frame (uncompressed) onto the request body.
    ///
    /// Returns whether the frame was accepted; a closed channel means the
    /// stream has ended and the caller should stop.
    pub fn send_frame(&self, payload: &[u8]) -> bool {
        self.sender
            .send(Ok(Bytes::from(encode_frame(0, payload))))
            .is_ok()
    }

    /// Pull the next complete server frame, reading more bytes as needed.
    ///
    /// `Ok(None)` marks a clean end of the server stream.
    pub async fn next_frame(&mut self) -> Result<Option<ConnectFrame>, ProviderError> {
        loop {
            if let Some(frame) = self
                .decoder
                .next_frame()
                .map_err(|e| self.stream_error(e.to_string()))?
            {
                return Ok(Some(frame));
            }
            match self.incoming.next().await {
                Some(Ok(bytes)) => self.decoder.push(&bytes),
                Some(Err(e)) => return Err(self.stream_error(e.to_string())),
                None => return Ok(None),
            }
        }
    }

    fn stream_error(&self, message: String) -> ProviderError {
        ProviderError::StreamError {
            provider: self.id.clone(),
            message,
            partial_response: None,
        }
    }
}
