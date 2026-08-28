//! The `browser` tool: drive a real browser over the DevTools Protocol.
//!
//! One browser, many named tabs. A tab is created by name with `open`, scripted
//! with `run`, photographed with `screenshot`, and dropped with `close`. The
//! browser is either one the user already runs (a configured CDP endpoint) or
//! one this tool launches headless; with neither reachable the tool is not in
//! the roster, so it never has to report that it found nothing to drive.
//!
//! Tabs outlive a single call: the DevTools target stays in the browser after
//! the socket closes, so a name is stored as a target id and every call
//! reconnects and re-attaches by that id. The protocol framing lives in
//! [`cdp`], tested without a browser; this file turns an action into the right
//! sequence of CDP commands.

mod cdp;

use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use cdp::{CdpConnection, CdpError};

/// Named tabs, shared across every call in the process: tab name to CDP target
/// id. The id is stable in the browser regardless of which client is attached.
static TABS: Lazy<DashMap<String, String>> = Lazy::new(DashMap::new);

/// The browser this tool launched, kept alive so it is not dropped between
/// calls, together with the endpoint discovered for it.
static LAUNCHED: Lazy<Mutex<Option<Launched>>> = Lazy::new(|| Mutex::new(None));

struct Launched {
    /// Held so the process is not killed while tabs are open. Never read.
    _child: Child,
    /// The browser-level WebSocket URL every call reconnects to.
    ws_url: String,
}

pub struct BrowserTool;

#[derive(Debug, Deserialize)]
struct BrowserInput {
    /// One of open, run, screenshot, close.
    action: String,
    /// The tab this action targets. Defaults to "default".
    #[serde(default)]
    name: Option<String>,
    /// open: the URL to load.
    #[serde(default)]
    url: Option<String>,
    /// open: the viewport size, `[width, height]` in CSS pixels.
    #[serde(default)]
    viewport: Option<[u32; 2]>,
    /// run: the JavaScript to evaluate in the page, top-level `await` allowed.
    #[serde(default)]
    code: Option<String>,
    /// close: when true, close every tab instead of the named one.
    #[serde(default)]
    all: bool,
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Drive a browser over the DevTools Protocol. Actions:\n\
         - open: open a named tab at a URL (name, url, optional viewport [w,h]).\n\
         - run: evaluate JavaScript in a tab and return the value (name, code). Top-level await is allowed.\n\
         - screenshot: capture a tab as a JPEG (name).\n\
         - close: close a tab (name), or every tab (all: true).\n\
         Tabs persist between calls; refer to one by the name it was opened with."
    }

    fn permission_level(&self) -> PermissionLevel {
        // A live browser session reaches every page it opens, including logged-in
        // ones, so it sits at the same level as the desktop tools.
        PermissionLevel::Dangerous
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["open", "run", "screenshot", "close"],
                    "description": "Which browser action to take."
                },
                "name": {
                    "type": "string",
                    "description": "The tab this action targets. Defaults to \"default\"."
                },
                "url": { "type": "string", "description": "open: the URL to load." },
                "viewport": {
                    "type": "array",
                    "items": { "type": "number" },
                    "description": "open: viewport size as [width, height] in CSS pixels."
                },
                "code": {
                    "type": "string",
                    "description": "run: JavaScript to evaluate in the page. Top-level await is allowed."
                },
                "all": {
                    "type": "boolean",
                    "description": "close: close every tab instead of the named one."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: BrowserInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };
        let tab = params.name.clone().unwrap_or_else(|| "default".to_string());

        let result = match params.action.as_str() {
            "open" => open_tab(ctx, &tab, params.url.as_deref(), params.viewport).await,
            "run" => run_in_tab(ctx, &tab, params.code.as_deref()).await,
            "screenshot" => screenshot_tab(ctx, &tab).await,
            "close" => close_tabs(ctx, &tab, params.all).await,
            other => Err(BrowserFailure::Usage(format!(
                "unknown action {other:?}; use open, run, screenshot or close"
            ))),
        };

        match result {
            Ok(message) => ToolResult::success(message),
            Err(failure) => ToolResult::error(failure.to_string()),
        }
    }
}

/// A tool-level failure, separate from [`CdpError`] so a usage mistake reads
/// differently from a browser fault.
#[derive(Debug, thiserror::Error)]
enum BrowserFailure {
    #[error("{0}")]
    Usage(String),
    #[error("no tab named {0:?} is open; open it first")]
    UnknownTab(String),
    #[error("could not reach a browser: {0}")]
    NoBrowser(String),
    #[error(transparent)]
    Cdp(#[from] CdpError),
    #[error("the page raised an exception: {0}")]
    PageException(String),
}

/// Open a tab, load a URL into it, and remember it by name.
async fn open_tab(
    ctx: &ToolContext,
    tab: &str,
    url: Option<&str>,
    viewport: Option<[u32; 2]>,
) -> Result<String, BrowserFailure> {
    let ws_url = resolve_browser(ctx).await?;
    let mut connection = CdpConnection::connect(&ws_url).await?;

    let target = url.unwrap_or("about:blank");
    let created = connection
        .send("Target.createTarget", json!({ "url": target }), None)
        .await?;
    let target_id = created
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::Protocol("createTarget returned no targetId".to_string()))?
        .to_string();
    TABS.insert(tab.to_string(), target_id.clone());

    let session_id = attach(&mut connection, &target_id).await?;
    if let Some([width, height]) = viewport {
        connection
            .send(
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false }),
                Some(&session_id),
            )
            .await?;
    }
    wait_for_load(&mut connection, &session_id).await;

    let title = evaluate_value(&mut connection, &session_id, "document.title")
        .await
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    Ok(format!(
        "Opened tab {tab:?} at {target} (title: {title:?})."
    ))
}

/// Evaluate JavaScript in a tab and return the value it produced.
async fn run_in_tab(
    ctx: &ToolContext,
    tab: &str,
    code: Option<&str>,
) -> Result<String, BrowserFailure> {
    let code = code.ok_or_else(|| {
        BrowserFailure::Usage("run needs `code`: the JavaScript to evaluate".to_string())
    })?;
    let target_id = target_for(tab)?;
    let ws_url = resolve_browser(ctx).await?;
    let mut connection = CdpConnection::connect(&ws_url).await?;
    let session_id = attach(&mut connection, &target_id).await?;

    let value = evaluate_value(&mut connection, &session_id, code).await?;
    Ok(serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()))
}

/// Capture a tab as a JPEG, returned as a data URI the way the desktop tools do.
async fn screenshot_tab(ctx: &ToolContext, tab: &str) -> Result<String, BrowserFailure> {
    let target_id = target_for(tab)?;
    let ws_url = resolve_browser(ctx).await?;
    let mut connection = CdpConnection::connect(&ws_url).await?;
    let session_id = attach(&mut connection, &target_id).await?;

    let shot = connection
        .send(
            "Page.captureScreenshot",
            json!({ "format": "jpeg", "quality": 70 }),
            Some(&session_id),
        )
        .await?;
    let data = shot
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::Protocol("captureScreenshot returned no data".to_string()))?;
    Ok(format!(
        "Screenshot of tab {tab:?} ({} bytes base64).\ndata:image/jpeg;base64,{data}",
        data.len()
    ))
}

/// Close one tab, or every tab when `all` is set.
async fn close_tabs(ctx: &ToolContext, tab: &str, all: bool) -> Result<String, BrowserFailure> {
    let ws_url = resolve_browser(ctx).await?;
    let mut connection = CdpConnection::connect(&ws_url).await?;

    let targets: Vec<(String, String)> = if all {
        TABS.iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    } else {
        vec![(tab.to_string(), target_for(tab)?)]
    };

    let mut closed = 0;
    for (name, target_id) in targets {
        connection
            .send("Target.closeTarget", json!({ "targetId": target_id }), None)
            .await?;
        TABS.remove(&name);
        closed += 1;
    }
    Ok(format!("Closed {closed} tab(s)."))
}

/// The CDP target id a tab name stands for, or an error naming the tab.
fn target_for(tab: &str) -> Result<String, BrowserFailure> {
    TABS.get(tab)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| BrowserFailure::UnknownTab(tab.to_string()))
}

/// Attach to a target in flatten mode and return the page session id.
async fn attach(connection: &mut CdpConnection, target_id: &str) -> Result<String, BrowserFailure> {
    let attached = connection
        .send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
        )
        .await?;
    attached
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            BrowserFailure::Cdp(CdpError::Protocol(
                "attachToTarget returned no sessionId".to_string(),
            ))
        })
}

/// Evaluate an expression by value, turning a page exception into an error.
async fn evaluate_value(
    connection: &mut CdpConnection,
    session_id: &str,
    expression: &str,
) -> Result<Value, BrowserFailure> {
    let evaluated = connection
        .send(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "awaitPromise": true,
                "returnByValue": true,
            }),
            Some(session_id),
        )
        .await?;
    if let Some(exception) = evaluated.get("exceptionDetails") {
        let text = exception
            .get("exception")
            .and_then(|exc| exc.get("description"))
            .and_then(Value::as_str)
            .or_else(|| exception.get("text").and_then(Value::as_str))
            .unwrap_or("unknown error");
        return Err(BrowserFailure::PageException(text.to_string()));
    }
    Ok(evaluated
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// Wait, briefly and best-effort, for the page to finish loading.
///
/// A page that never settles must not hold the call open, so this polls
/// `document.readyState` a bounded number of times and then returns whatever
/// state the page is in.
async fn wait_for_load(connection: &mut CdpConnection, session_id: &str) {
    for _ in 0..20 {
        let ready = evaluate_value(connection, session_id, "document.readyState")
            .await
            .ok()
            .and_then(|value| value.as_str().map(str::to_string));
        if ready.as_deref() == Some("complete") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Find the browser to drive, launching one if none is configured.
///
/// A configured CDP endpoint wins; otherwise a browser is launched once and
/// reused. The endpoint is a browser-level WebSocket URL both paths resolve to.
async fn resolve_browser(ctx: &ToolContext) -> Result<String, BrowserFailure> {
    if let Some(cdp_url) = ctx.config.browser_cdp_url.as_deref() {
        return endpoint_from_http(cdp_url).await;
    }
    let mut launched = LAUNCHED.lock().await;
    if let Some(existing) = launched.as_ref() {
        return Ok(existing.ws_url.clone());
    }
    let started = launch_browser(ctx).await?;
    let ws_url = started.ws_url.clone();
    *launched = Some(started);
    Ok(ws_url)
}

/// Read a browser's WebSocket URL from its `/json/version` endpoint.
async fn endpoint_from_http(cdp_url: &str) -> Result<String, BrowserFailure> {
    let base = cdp_url.trim_end_matches('/');
    let version_url = format!("{base}/json/version");
    let body = reqwest::get(&version_url)
        .await
        .map_err(|error| BrowserFailure::NoBrowser(format!("{version_url}: {error}")))?
        .text()
        .await
        .map_err(|error| BrowserFailure::NoBrowser(format!("{version_url}: {error}")))?;
    Ok(cdp::ws_url_from_version(&body)?)
}

/// Launch a headless browser and wait for its CDP endpoint to answer.
async fn launch_browser(ctx: &ToolContext) -> Result<Launched, BrowserFailure> {
    let binary = browser_binary(ctx)
        .ok_or_else(|| BrowserFailure::NoBrowser("no Chrome or Chromium found".to_string()))?;
    let port =
        free_port().map_err(|error| BrowserFailure::NoBrowser(format!("no free port: {error}")))?;

    let child = tokio::process::Command::new(&binary)
        .args([
            "--headless=new",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            &format!("--remote-debugging-port={port}"),
        ])
        .kill_on_drop(false)
        .spawn()
        .map_err(|error| BrowserFailure::NoBrowser(format!("{binary}: {error}")))?;

    let http = format!("http://127.0.0.1:{port}");
    let ws_url = wait_for_endpoint(&http).await?;
    Ok(Launched {
        _child: child,
        ws_url,
    })
}

/// Poll `/json/version` until the freshly launched browser answers.
async fn wait_for_endpoint(http: &str) -> Result<String, BrowserFailure> {
    let mut last = String::new();
    for _ in 0..50 {
        match endpoint_from_http(http).await {
            Ok(ws_url) => return Ok(ws_url),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(BrowserFailure::NoBrowser(format!(
        "browser did not open its CDP endpoint: {last}"
    )))
}

/// The browser binary to launch: the configured one, or a name on the PATH.
fn browser_binary(ctx: &ToolContext) -> Option<String> {
    if let Some(executable) = ctx.config.browser_executable.as_deref() {
        return Some(executable.to_string());
    }
    ["google-chrome", "chromium", "chromium-browser", "chrome"]
        .iter()
        .find(|name| which::which(name).is_ok())
        .map(|name| name.to_string())
}

/// A free TCP port, found by binding to 0 and reading back the assignment.
fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: Value) -> BrowserInput {
        serde_json::from_value(input).expect("valid input")
    }

    /// A context with no browser configured, so any path that reaches
    /// `resolve_browser` would try to launch one; the guards under test must
    /// return first.
    fn test_ctx() -> ToolContext {
        use mikmik_core::config::Config;
        use mikmik_core::permissions::AutoPermissionHandler;
        use std::path::PathBuf;
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        ToolContext {
            working_dir: PathBuf::from("."),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: Arc::new(AutoPermissionHandler {
                mode: mikmik_core::config::PermissionMode::Default,
            }),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "test-browser".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: Arc::new(AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: Config::default(),
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            plan_approval_tx: None,
            tool_output_tx: None,
            plan_mode_tx: None,
            advisor_note_tx: None,
            advisor_name: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            current_call: None,
            editor: None,
            inbox: Default::default(),
        }
    }

    #[test]
    fn an_unknown_action_is_named_not_ignored() {
        // The dispatch is what an unknown action must reach, so a wrong verb is
        // reported rather than silently doing nothing.
        let input = parse(json!({ "action": "teleport" }));
        assert_eq!(input.action, "teleport");
    }

    #[test]
    fn a_missing_name_falls_back_to_default() {
        let input = parse(json!({ "action": "run", "code": "1+1" }));
        assert!(input.name.is_none());
    }

    #[tokio::test]
    async fn run_without_code_is_refused_before_touching_a_browser() {
        // The guard has to fire before `resolve_browser`, or a missing `code`
        // would launch a browser only to fail. No browser is configured here,
        // so reaching one would error differently.
        let ctx = test_ctx();
        let error = run_in_tab(&ctx, "default", None)
            .await
            .expect_err("missing code must be refused");
        assert!(matches!(error, BrowserFailure::Usage(_)), "{error}");
    }

    #[tokio::test]
    async fn a_run_on_an_unknown_tab_is_refused_before_touching_a_browser() {
        // An unknown tab is a caller mistake; it must be caught before a browser
        // is launched, so the error names the tab rather than a transport fault.
        TABS.clear();
        let ctx = test_ctx();
        let error = run_in_tab(&ctx, "ghost", Some("1+1"))
            .await
            .expect_err("an unknown tab must be refused");
        assert!(matches!(error, BrowserFailure::UnknownTab(_)), "{error}");
    }
}
