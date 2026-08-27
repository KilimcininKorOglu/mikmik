//! What the host and the runner say to each other.
//!
//! One JSON object per line, in both directions. The shapes live here rather
//! than inline so a test can build and read them without a socket.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A message the runner sends the host.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromRunner {
    /// First line after connecting, carrying the token this session was
    /// started with. A connection that says anything else first is dropped.
    Hello { token: String },
    /// The running code called a host function.
    Host {
        id: u64,
        op: String,
        #[serde(default)]
        args: Value,
    },
    /// The call finished.
    Done {
        id: u64,
        ok: bool,
        #[serde(default)]
        output: String,
        #[serde(default)]
        value: Value,
        #[serde(default)]
        error: Option<String>,
    },
}

/// A message the host sends the runner.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToRunner {
    /// Run this code.
    Run {
        id: u64,
        code: String,
        #[serde(rename = "readOnly")]
        read_only: bool,
    },
    /// The answer to one host call.
    HostResult {
        id: u64,
        ok: bool,
        #[serde(default)]
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl ToRunner {
    /// A successful answer to host call `id`.
    pub fn ok(id: u64, value: Value) -> Self {
        Self::HostResult {
            id,
            ok: true,
            value,
            error: None,
        }
    }

    /// A refusal or a failure, which the runner turns into a thrown error.
    pub fn failed(id: u64, error: impl Into<String>) -> Self {
        Self::HostResult {
            id,
            ok: false,
            value: Value::Null,
            error: Some(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_host_call_decodes_with_its_arguments() {
        let line = r#"{"type":"host","id":3,"op":"click","args":{"x":10,"y":20}}"#;
        let decoded: FromRunner = match serde_json::from_str(line) {
            Ok(decoded) => decoded,
            Err(error) => panic!("the line should decode: {error}"),
        };

        assert_eq!(
            decoded,
            FromRunner::Host {
                id: 3,
                op: "click".to_string(),
                args: json!({"x": 10, "y": 20}),
            }
        );
    }

    #[test]
    fn a_finished_call_carries_what_it_printed() {
        let line = r#"{"type":"done","id":1,"ok":true,"output":"hello","value":42}"#;
        let decoded: FromRunner = match serde_json::from_str(line) {
            Ok(decoded) => decoded,
            Err(error) => panic!("the line should decode: {error}"),
        };

        match decoded {
            FromRunner::Done {
                ok, output, value, ..
            } => {
                assert!(ok);
                assert_eq!(output, "hello");
                assert_eq!(value, json!(42));
            }
            other => panic!("expected a done, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_names_its_reason() {
        let json = match serde_json::to_value(ToRunner::failed(7, "read_only is set")) {
            Ok(json) => json,
            Err(error) => panic!("the message should serialise: {error}"),
        };

        assert_eq!(json["type"], "host_result");
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "read_only is set");
    }

    #[test]
    fn a_run_message_names_the_read_only_flag_the_runner_reads() {
        // The runner reads `readOnly`; a snake_case field here would leave it
        // undefined there, which reads as false and opens every write.
        let json = match serde_json::to_value(ToRunner::Run {
            id: 1,
            code: "1".to_string(),
            read_only: true,
        }) {
            Ok(json) => json,
            Err(error) => panic!("the message should serialise: {error}"),
        };

        assert_eq!(json["readOnly"], true);
    }
}
