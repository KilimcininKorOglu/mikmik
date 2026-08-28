//! A small Chrome DevTools Protocol client.
//!
//! CDP is request/response over one WebSocket, correlated by an integer `id`,
//! interleaved with unsolicited events that carry a `method` and no `id`. This
//! client sends one command at a time and reads frames until the matching `id`
//! comes back, dropping events and stray ids on the way.
//!
//! The framing is split from the socket on purpose: [`command_frame`] and
//! [`match_response`] are pure and carry the protocol's rules, so they are
//! tested against fixtures without a live browser, and [`CdpConnection`] is the
//! thin I/O layer over them.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// How long one command waits for its response before giving up.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A CDP failure, kept apart from the tool's own errors so the caller can tell
/// a transport fault from a command the browser refused.
#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    /// The socket could not be opened or a frame could not be sent or read.
    #[error("browser transport error: {0}")]
    Transport(String),
    /// A frame arrived that was not the JSON object the protocol requires.
    #[error("malformed CDP frame: {0}")]
    Protocol(String),
    /// The browser answered the command with an `error` object.
    #[error("browser refused the command: {message} (code {code})")]
    Remote { code: i64, message: String },
    /// No response came back inside [`COMMAND_TIMEOUT`].
    #[error("browser did not answer within the timeout")]
    Timeout,
}

/// Build the JSON envelope for one command.
///
/// In flatten mode a page command carries the page's `sessionId` beside the
/// `id`; a browser-level command carries none.
pub fn command_frame(id: u64, method: &str, params: Value, session_id: Option<&str>) -> Value {
    let mut frame = json!({ "id": id, "method": method, "params": params });
    if let Some(session_id) = session_id {
        frame["sessionId"] = Value::String(session_id.to_string());
    }
    frame
}

/// Decide what an incoming frame is, relative to the command awaiting `id`.
///
/// `None` means the frame is not this command's response — an event, or another
/// command's answer — and the reader must keep waiting. `Some(Ok)` is the
/// command's result, `Some(Err)` the error the browser returned for it.
pub fn match_response(frame: &Value, id: u64) -> Option<Result<Value, CdpError>> {
    let frame_id = frame.get("id").and_then(Value::as_u64)?;
    if frame_id != id {
        return None;
    }
    if let Some(error) = frame.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        return Some(Err(CdpError::Remote { code, message }));
    }
    Some(Ok(frame.get("result").cloned().unwrap_or(Value::Null)))
}

/// Read the browser-level WebSocket URL out of a `/json/version` body.
pub fn ws_url_from_version(body: &str) -> Result<String, CdpError> {
    let parsed: Value = serde_json::from_str(body)
        .map_err(|error| CdpError::Protocol(format!("/json/version was not JSON: {error}")))?;
    parsed
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CdpError::Protocol("/json/version had no webSocketDebuggerUrl".to_string()))
}

/// A live CDP connection: one WebSocket and the id it will stamp next.
pub struct CdpConnection {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl CdpConnection {
    /// Open a connection to a browser- or page-level WebSocket URL.
    pub async fn connect(ws_url: &str) -> Result<Self, CdpError> {
        let (socket, _response) = connect_async(ws_url)
            .await
            .map_err(|error| CdpError::Transport(error.to_string()))?;
        Ok(Self { socket, next_id: 1 })
    }

    /// Send one command and return its result, dropping events and stray ids.
    pub async fn send(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        let id = self.next_id;
        self.next_id += 1;

        let frame = command_frame(id, method, params, session_id);
        let text =
            serde_json::to_string(&frame).map_err(|error| CdpError::Protocol(error.to_string()))?;
        self.socket
            .send(Message::Text(text))
            .await
            .map_err(|error| CdpError::Transport(error.to_string()))?;

        tokio::time::timeout(COMMAND_TIMEOUT, self.read_until(id))
            .await
            .map_err(|_| CdpError::Timeout)?
    }

    /// Read frames until the one answering `id` arrives.
    async fn read_until(&mut self, id: u64) -> Result<Value, CdpError> {
        loop {
            let message = self
                .socket
                .next()
                .await
                .ok_or_else(|| {
                    CdpError::Transport("the browser closed the connection".to_string())
                })?
                .map_err(|error| CdpError::Transport(error.to_string()))?;

            let text = match message {
                Message::Text(text) => text,
                // A control frame carries no command result; keep reading.
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
                    continue
                }
                Message::Close(_) => {
                    return Err(CdpError::Transport(
                        "the browser closed the connection".to_string(),
                    ))
                }
            };

            let parsed: Value = serde_json::from_str(&text)
                .map_err(|error| CdpError::Protocol(error.to_string()))?;
            if let Some(result) = match_response(&parsed, id) {
                return result;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_command_carries_no_session_id() {
        let frame = command_frame(
            7,
            "Target.createTarget",
            json!({ "url": "about:blank" }),
            None,
        );
        assert_eq!(frame["id"], 7);
        assert_eq!(frame["method"], "Target.createTarget");
        assert_eq!(frame["params"]["url"], "about:blank");
        assert!(frame.get("sessionId").is_none());
    }

    #[test]
    fn a_page_command_carries_the_session_id() {
        let frame = command_frame(9, "Runtime.evaluate", json!({}), Some("SID-42"));
        assert_eq!(frame["sessionId"], "SID-42");
    }

    #[test]
    fn a_matching_result_is_returned() {
        let frame = json!({ "id": 3, "result": { "targetId": "T1" } });
        let matched = match_response(&frame, 3).expect("id 3 is this command's answer");
        let value = matched.expect("a result, not an error");
        assert_eq!(value["targetId"], "T1");
    }

    #[test]
    fn a_remote_error_is_surfaced_not_swallowed() {
        let frame = json!({ "id": 3, "error": { "code": -32000, "message": "no such target" } });
        let matched = match_response(&frame, 3).expect("id 3 is this command's answer");
        match matched {
            Err(CdpError::Remote { code, message }) => {
                assert_eq!(code, -32000);
                assert_eq!(message, "no such target");
            }
            other => panic!("expected a remote error, got {other:?}"),
        }
    }

    #[test]
    fn an_event_is_not_taken_as_a_response() {
        // Events carry a method and no id; the reader must keep waiting.
        let event = json!({ "method": "Page.loadEventFired", "params": {} });
        assert!(match_response(&event, 3).is_none());
    }

    #[test]
    fn another_commands_answer_is_skipped() {
        let frame = json!({ "id": 4, "result": {} });
        assert!(match_response(&frame, 3).is_none());
    }

    #[test]
    fn the_ws_url_is_read_from_the_version_body() {
        let body = r#"{"Browser":"Chrome/120","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/browser/abc"}"#;
        assert_eq!(
            ws_url_from_version(body).expect("a ws url"),
            "ws://127.0.0.1:9222/devtools/browser/abc"
        );
    }

    #[test]
    fn a_version_body_without_the_url_is_an_error() {
        let body = r#"{"Browser":"Chrome/120"}"#;
        assert!(ws_url_from_version(body).is_err());
    }
}
