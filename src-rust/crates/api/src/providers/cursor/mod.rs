// providers/cursor — Cursor (Cursor Pro) agent-executor provider.
//
// Unlike every other provider here, Cursor is not an ordinary `LlmProvider`
// that emits tool calls for mikmik's query loop to dispatch. Cursor's server
// runs the whole agent loop over one long-lived bidirectional HTTP/2 stream and
// expects the client to execute local tools on that same stream: the server
// sends `ExecServerMessage` tool-argument frames, the client runs the matching
// mikmik tool through a `CursorExecHandlers` bridge, and writes the result back
// as an `ExecClientMessage`. Assistant text, thinking and tool activity arrive
// as `InteractionUpdate` deltas, and hosted-action gates as `InteractionQuery`.
//
// `proto` is the hand-written wire codec. The transport (full-duplex Connect
// over HTTP/2) and the dispatcher state machine build on it. The exec handler
// trait is defined here in `api`; its implementation, which binds real mikmik
// tools, lives in the `query` crate because `api` cannot depend on `tools`.

pub mod proto;
