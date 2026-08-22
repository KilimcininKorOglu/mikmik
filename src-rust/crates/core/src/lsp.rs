//! Language Server Protocol client.
//!
//! Implements the client side of the LSP JSON-RPC protocol over the LSP
//! server's stdin/stdout.  Each [`LspClient`] manages one server process;
//! [`LspManager`] tracks a collection of clients keyed by server name.
//!
//! # Protocol overview
//! Messages are framed with a `Content-Length` HTTP-style header:
//! ```text
//! Content-Length: <N>\r\n
//! \r\n
//! <N bytes of UTF-8 JSON>
//! ```
//! The server sends the same framing back on its stdout.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Optional server-specific features a caller may switch on.
///
/// Every field names a request outside the LSP specification, so a server that
/// does not implement it answers "method not found". They are opt-in for that
/// reason. `rust-analyzer` is the only server in the built-in catalogue that
/// implements any of them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LspServerCapabilities {
    /// `rust-analyzer/runFlycheck`, an on-demand `cargo check`.
    #[serde(default)]
    pub flycheck: bool,
    /// `experimental/ssr`, structural search and replace.
    #[serde(default)]
    pub ssr: bool,
    /// `rust-analyzer/expandMacro`.
    #[serde(default)]
    pub expand_macro: bool,
    /// `experimental/runnables`, the tests and binaries a file offers.
    #[serde(default)]
    pub runnables: bool,
    /// `rust-analyzer/relatedTests`.
    #[serde(default)]
    pub related_tests: bool,
}

/// How long to wait for a server to finish loading the project.
///
/// A project-aware server answers a navigation request with nothing until it
/// has indexed the workspace, so a caller that asks too early gets a wrong
/// empty answer rather than an error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceReadyTimings {
    /// Give up waiting after this long and send the request anyway.
    pub timeout_ms: u64,
    /// How often to re-check readiness.
    pub poll_ms: u64,
    /// How long the server must stay idle before it counts as ready.
    pub settle_ms: u64,
    /// Budget for one server-status request.
    pub status_request_timeout_ms: u64,
}

impl Default for WorkspaceReadyTimings {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            poll_ms: 250,
            settle_ms: 2_000,
            status_request_timeout_ms: 2_000,
        }
    }
}

/// Configuration for a single LSP server process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerConfig {
    /// Display name, e.g. "rust-analyzer"
    pub name: String,
    /// Path or name of the server binary, e.g. "rust-analyzer"
    pub command: String,
    /// Command-line arguments passed to the server binary
    pub args: Vec<String>,
    /// Glob patterns that activate this server, e.g. `["*.rs", "*.toml"]`
    pub file_patterns: Vec<String>,
    /// Optional server-specific initialization options (passed in LSP `initialize`)
    pub initialization_options: Option<serde_json::Value>,
    /// Map of file extension (e.g. `.rs`) to LSP language identifier (e.g.
    /// `rust`).  Used to supply `textDocument/didOpen::languageId` and to
    /// route files to the right server.
    #[serde(default)]
    pub extension_to_language: HashMap<String, String>,
    /// Optional extra environment variables for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Files or directories that mark a project this server can serve, e.g.
    /// `["Cargo.toml"]`. A one-level wildcard such as `*.cabal` matches an
    /// entry directly inside the directory.
    ///
    /// Empty means the server is never auto-detected; it still runs when the
    /// user names it explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_markers: Vec<String>,
    /// Switch the server off without deleting its entry.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// Settings pushed with `workspace/didChangeConfiguration` after the
    /// handshake, and again on reload.
    ///
    /// Distinct from `initialization_options`, which the server reads once and
    /// cannot be changed afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<serde_json::Value>,
    /// This server only reports problems; it answers no navigation request.
    ///
    /// A linter is asked for diagnostics and is kept out of hover, definition,
    /// references, symbols and rename.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_linter: bool,
    /// One language id for every file this server handles, when the server
    /// serves a single language. Takes precedence over
    /// `extension_to_language`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    /// Budget for this server's `initialize` handshake. Defaults to
    /// [`DEFAULT_WARMUP_TIMEOUT_MS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_timeout_ms: Option<u64>,
    /// Budget for one request to this server. Defaults to
    /// [`DEFAULT_REQUEST_TIMEOUT_MS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// Optional non-standard features this server implements.
    #[serde(default, skip_serializing_if = "LspServerCapabilities::is_empty")]
    pub capabilities: LspServerCapabilities,
    /// Overrides for the project-load wait. Only a project-aware server needs
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_ready_timings: Option<WorkspaceReadyTimings>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl LspServerCapabilities {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Budget for one `initialize` handshake.
pub const DEFAULT_WARMUP_TIMEOUT_MS: u64 = 5_000;
/// Budget for one ordinary request.
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// The LSP language identifier for a file extension.
///
/// A server that serves one language rarely needs an extension map, and a
/// wrong `languageId` makes some servers refuse the document outright, so the
/// common extensions are answered here rather than left to every config.
pub fn language_id_for_extension(ext: &str) -> Option<&'static str> {
    let ext = ext.trim_start_matches('.');
    Some(match ext {
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "scala" | "sc" => "scala",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "cs" => "csharp",
        "lua" => "lua",
        "zig" => "zig",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "gleam" => "gleam",
        "dart" => "dart",
        "odin" => "odin",
        "nix" => "nix",
        "vim" => "vim",
        "sh" | "bash" => "shellscript",
        "zsh" => "zsh",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "json" => "json",
        "jsonc" => "jsonc",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "tex" => "latex",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "graphql" | "gql" => "graphql",
        "prisma" => "prisma",
        "tf" | "tfvars" => "terraform",
        "sql" => "sql",
        "xml" => "xml",
        "tla" => "tlaplus",
        _ => return None,
    })
}

impl LspServerConfig {
    /// Look up the LSP language identifier for `file_path`.
    ///
    /// The explicit `language_id` wins, then the per-server extension map,
    /// then the built-in table. `"plaintext"` is the last resort, and a server
    /// that receives it usually ignores the document.
    pub fn language_for_file(&self, file_path: &str) -> String {
        if let Some(id) = &self.language_id {
            return id.clone();
        }
        let ext = Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))
            .unwrap_or_default();
        if let Some(mapped) = self.extension_to_language.get(&ext) {
            return mapped.clone();
        }
        language_id_for_extension(&ext)
            .unwrap_or("plaintext")
            .to_string()
    }

    /// Budget for this server's handshake.
    pub fn warmup_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.warmup_timeout_ms.unwrap_or(DEFAULT_WARMUP_TIMEOUT_MS),
        )
    }

    /// Budget for one request to this server.
    pub fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.request_timeout_ms
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
        )
    }

    /// The project-load wait for this server.
    pub fn ready_timings(&self) -> WorkspaceReadyTimings {
        self.workspace_ready_timings.clone().unwrap_or_default()
    }

    /// Every file extension this server handles, normalised to `.ext`.
    ///
    /// Reads both the extension map and the `*.ext` glob patterns, because a
    /// config may carry either.
    pub fn extensions(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .extension_to_language
            .keys()
            .map(|e| e.to_lowercase())
            .collect();
        for pattern in &self.file_patterns {
            if let Some(ext) = pattern.strip_prefix("*.") {
                let normalized = format!(".{}", ext.to_lowercase());
                if !out.contains(&normalized) {
                    out.push(normalized);
                }
            }
        }
        out
    }

    /// Whether this server handles `file_path`.
    ///
    /// Matches the extension, and also the whole file name, so a config can
    /// name a file that has no extension such as `Dockerfile`.
    pub fn handles_file(&self, file_path: &str) -> bool {
        let path = Path::new(file_path);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()));
        if let Some(ext) = ext {
            if self.extensions().contains(&ext) {
                return true;
            }
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_lowercase();
        self.file_patterns
            .iter()
            .any(|p| !p.starts_with("*.") && p.to_lowercase() == name)
    }
}

// ---------------------------------------------------------------------------
// Discovery: finding the binary and recognising the project
// ---------------------------------------------------------------------------

/// A project-local directory that holds installed executables, and the files
/// that say the project uses it.
struct LocalBinDir {
    markers: &'static [&'static str],
    bin_dir: &'static str,
}

/// Where a language server is installed by the project's own package manager.
///
/// A project pins its tooling, and the pinned copy is the one that matches the
/// project's configuration. Searching `PATH` first would run a different
/// version, or none at all when nothing is installed globally.
const LOCAL_BIN_DIRS: &[LocalBinDir] = &[
    LocalBinDir {
        markers: &[
            "package.json",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "bun.lockb",
        ],
        bin_dir: "node_modules/.bin",
    },
    LocalBinDir {
        markers: PYTHON_ROOT_MARKERS,
        bin_dir: ".venv/bin",
    },
    LocalBinDir {
        markers: PYTHON_ROOT_MARKERS,
        bin_dir: ".venv/Scripts",
    },
    LocalBinDir {
        markers: PYTHON_ROOT_MARKERS,
        bin_dir: "venv/bin",
    },
    LocalBinDir {
        markers: &["Gemfile", "Gemfile.lock"],
        bin_dir: "bin",
    },
    LocalBinDir {
        markers: &["Gemfile", "Gemfile.lock"],
        bin_dir: "vendor/bundle/bin",
    },
    LocalBinDir {
        markers: &["go.mod", "go.work"],
        bin_dir: "bin",
    },
];

const PYTHON_ROOT_MARKERS: &[&str] = &[
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "Pipfile",
];

/// Executable suffixes to try on Windows, where a package manager writes a
/// launcher rather than the program itself.
#[cfg(windows)]
const EXECUTABLE_SUFFIXES: &[&str] = &["", ".exe", ".cmd", ".bat", ".ps1"];
#[cfg(not(windows))]
const EXECUTABLE_SUFFIXES: &[&str] = &[""];

/// Resolve `command` to an executable, project-local copies first.
///
/// An absolute or relative path is taken as given. A bare name is looked for in
/// the project's own bin directories, then on `PATH`. `None` means the server
/// is not installed, which is the ordinary case for a catalogue entry that does
/// not apply to this machine.
pub fn resolve_command(command: &str, cwd: &Path) -> Option<std::path::PathBuf> {
    if command.contains('/') || command.contains('\\') {
        let path = if Path::new(command).is_absolute() {
            std::path::PathBuf::from(command)
        } else {
            cwd.join(command)
        };
        return path.is_file().then_some(path);
    }

    for local in LOCAL_BIN_DIRS {
        if !local.markers.iter().any(|m| cwd.join(m).exists()) {
            continue;
        }
        let dir = cwd.join(local.bin_dir);
        for suffix in EXECUTABLE_SUFFIXES {
            let candidate = dir.join(format!("{command}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    which::which(command).ok()
}

/// The server definitions that ship with the binary.
const BUNDLED_SERVERS: &str = include_str!("../assets/lsp-servers.json");

/// Every server the catalogue knows.
///
/// A malformed catalogue is a build-time mistake rather than a user's, and a
/// session must still start, so a parse failure yields an empty list and says
/// so in the log.
pub fn builtin_servers() -> &'static [LspServerConfig] {
    static SERVERS: once_cell::sync::Lazy<Vec<LspServerConfig>> =
        once_cell::sync::Lazy::new(|| match serde_json::from_str(BUNDLED_SERVERS) {
            Ok(servers) => servers,
            Err(e) => {
                tracing::error!("bundled LSP server catalogue is malformed: {e}");
                Vec::new()
            }
        });
    &SERVERS
}

/// The catalogue servers that suit the project in `cwd`.
///
/// A server is detected when the directory carries one of its root markers and
/// its binary resolves. Both conditions matter: the marker says the project
/// uses the language, and the binary says the machine can serve it. Detecting
/// on the marker alone would fill the list with servers that fail to start.
///
/// The search is the working directory only. A marker in a parent says the
/// parent is a project, not this directory.
pub fn detect_servers(cwd: &Path) -> Vec<LspServerConfig> {
    builtin_servers()
        .iter()
        .filter(|server| !server.root_markers.is_empty())
        .filter(|server| has_root_markers(cwd, &server.root_markers))
        .filter(|server| resolve_command(&server.command, cwd).is_some())
        .cloned()
        .collect()
}

/// Whether `dir` holds at least one of `markers`.
///
/// A marker may carry a one-level wildcard, `*.cabal`, which matches an entry
/// directly inside `dir`. The search never walks into a subdirectory: a marker
/// found three levels down says nothing about the directory the session opened.
pub fn has_root_markers(dir: &Path, markers: &[String]) -> bool {
    markers.iter().any(|marker| {
        if let Some(suffix) = marker.strip_prefix("*.") {
            let suffix = format!(".{}", suffix.to_lowercase());
            std::fs::read_dir(dir)
                .map(|entries| {
                    entries.flatten().any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .to_lowercase()
                            .ends_with(&suffix)
                    })
                })
                .unwrap_or(false)
        } else {
            dir.join(marker).exists()
        }
    })
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// A single diagnostic emitted by an LSP server.
#[derive(Debug, Clone)]
pub struct LspDiagnostic {
    /// Workspace-relative or absolute file path
    pub file: String,
    /// 1-based line number
    pub line: u32,
    /// 1-based column number
    pub column: u32,
    pub severity: DiagnosticSeverity,
    pub message: String,
    /// The LSP server that produced this diagnostic (e.g. "rust-analyzer")
    pub source: Option<String>,
    /// Diagnostic code (e.g. "E0308"), if provided by the server
    pub code: Option<String>,
}

/// Severity level of a diagnostic, matching the LSP spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

impl DiagnosticSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }

    fn from_lsp_int(n: u64) -> Self {
        match n {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Information,
            _ => Self::Hint,
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC framing helpers
// ---------------------------------------------------------------------------

/// The server's stdin, behind a trait object.
///
/// Boxed rather than typed as `ChildStdin` so a test can drive the client over
/// an in-memory pipe. Without it the protocol could only be exercised by
/// spawning a real language server, which no test can rely on being installed.
type BoxedWriter = BufWriter<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>;
/// The server's stdout, behind a trait object, for the same reason.
type BoxedReader = BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;

async fn send_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    body: &str,
) -> anyhow::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> anyhow::Result<serde_json::Value> {
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow::anyhow!("LSP server closed stdout"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length: ") {
            content_length = val.trim().parse()?;
        }
    }
    if content_length == 0 {
        return Err(anyhow::anyhow!("LSP message missing Content-Length header"));
    }
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

// ---------------------------------------------------------------------------
// LspClient
// ---------------------------------------------------------------------------

/// How many stderr lines to keep, so a server that dies can say why.
const STDERR_TAIL_LINES: usize = 20;

/// State the caller and the reader task both reach.
///
/// The reader has to answer the server, not only listen to it: a server that
/// asks for its configuration and gets silence waits, and some refuse to
/// finish starting. So the writer lives here rather than on the client alone.
struct ClientShared {
    server_name: String,
    config: LspServerConfig,
    /// `None` once the client has shut down and the pipe is closed.
    writer: Mutex<Option<BoxedWriter>>,
    pending: DashMap<u64, oneshot::Sender<serde_json::Value>>,
    diagnostics: DashMap<String, Vec<LspDiagnostic>>,
    /// Bumped every time the server publishes for a URI, so a caller can tell
    /// a fresh answer from the one it already had.
    diagnostic_versions: DashMap<String, u64>,
    /// What the server said it supports, from the `initialize` response.
    server_capabilities: parking_lot::RwLock<serde_json::Value>,
    /// Work-done progress tokens the server has open.
    active_progress: DashMap<String, ()>,
    /// When the last progress notification arrived.
    last_progress: parking_lot::Mutex<Option<std::time::Instant>>,
    /// When the handshake finished.
    initialized_at: parking_lot::Mutex<Option<std::time::Instant>>,
    /// The workspace the server was started for.
    root_uri: parking_lot::RwLock<String>,
    /// The last lines the server wrote to stderr.
    stderr_tail: parking_lot::Mutex<std::collections::VecDeque<String>>,
    /// Open documents and the version last sent for each.
    open_versions: DashMap<String, i64>,
}

impl ClientShared {
    /// Send one framed message, or fail if the pipe is closed.
    async fn send(&self, body: &str) -> anyhow::Result<()> {
        let mut guard = self.writer.lock().await;
        let writer = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("LSP client already shut down"))?;
        send_message(writer, body).await
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> anyhow::Result<()> {
        let body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))?;
        self.send(&body).await
    }

    /// Answer a request the server sent us.
    async fn respond(&self, id: &serde_json::Value, result: serde_json::Value) {
        let body = match serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })) {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!("could not encode response to {}: {e}", self.server_name);
                return;
            }
        };
        if let Err(e) = self.send(&body).await {
            tracing::debug!("could not answer {}: {e}", self.server_name);
        }
    }

    /// Tell the server we do not implement what it asked for.
    ///
    /// The specification requires an answer either way, and a server left
    /// waiting on an unanswered id can stall.
    async fn respond_method_not_found(&self, id: &serde_json::Value, method: &str) {
        let body = match serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("{method} is not implemented") },
        })) {
            Ok(body) => body,
            Err(_) => return,
        };
        let _ = self.send(&body).await;
    }

    /// Fail every request still waiting, naming what happened.
    ///
    /// Dropping the senders instead would surface as "channel closed", which
    /// says nothing about a server that died on a missing shared library.
    fn fail_pending(&self, reason: &str) {
        let ids: Vec<u64> = self.pending.iter().map(|e| *e.key()).collect();
        for id in ids {
            if let Some((_, tx)) = self.pending.remove(&id) {
                let _ = tx.send(json!({
                    "error": { "code": -32000, "message": reason },
                }));
            }
        }
    }

    fn stderr_tail_text(&self) -> String {
        let tail = self.stderr_tail.lock();
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// A running LSP client connected to a single server process.
pub struct LspClient {
    pub server_name: String,
    pub server_config: LspServerConfig,
    /// The child process handle; `None` after shutdown.
    process: Option<Child>,
    request_id: Arc<AtomicU64>,
    is_initialized: bool,
    shared: Arc<ClientShared>,
}

impl LspClient {
    /// Spawn the server process and return a connected client.  The I/O pump
    /// task is started in the background.
    pub async fn start(config: LspServerConfig) -> anyhow::Result<Self> {
        Self::start_in(config, &std::env::current_dir()?).await
    }

    /// Spawn the server for a specific working directory.
    ///
    /// The directory decides both where the server runs and which copy of the
    /// binary is used, because a project's own bin directory comes first.
    pub async fn start_in(config: LspServerConfig, cwd: &Path) -> anyhow::Result<Self> {
        let program = resolve_command(&config.command, cwd).ok_or_else(|| {
            anyhow::anyhow!(
                "language server '{}' is not installed ({} not found)",
                config.name,
                config.command
            )
        })?;

        let mut cmd = Command::new(&program);
        cmd.args(&config.args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Inject environment variables
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        // On Windows, suppress the console window (CREATE_NO_WINDOW = 0x0800_0000).
        // tokio::process::Command exposes creation_flags() directly on Windows.
        #[cfg(target_os = "windows")]
        {
            cmd.creation_flags(0x0800_0000u32);
        }

        crate::process_tree::spawn_in_own_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("Failed to start LSP server '{}': {}", config.command, e)
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("LSP server stdin not available"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("LSP server stdout not available"))?;

        let mut client = Self::connect(config, Box::new(stdout), Box::new(stdin));

        // Consume stderr in the background so the OS pipe buffer never fills
        // up, and keep the last lines: they are usually the only explanation a
        // server that dies during startup ever gives.
        if let Some(stderr) = child.stderr.take() {
            let shared = client.shared.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("[LSP SERVER {}] {}", shared.server_name, line);
                    let mut tail = shared.stderr_tail.lock();
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }

        client.process = Some(child);
        Ok(client)
    }

    /// Build a client over an already-connected pair of streams.
    ///
    /// The process-spawning path uses it, and so does any caller that already
    /// has a transport, a test included.
    pub fn connect(
        config: LspServerConfig,
        reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        writer: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    ) -> Self {
        let server_name = config.name.clone();
        let shared = Arc::new(ClientShared {
            server_name: server_name.clone(),
            config: config.clone(),
            writer: Mutex::new(Some(BufWriter::new(writer))),
            pending: DashMap::new(),
            diagnostics: DashMap::new(),
            diagnostic_versions: DashMap::new(),
            server_capabilities: parking_lot::RwLock::new(serde_json::Value::Null),
            active_progress: DashMap::new(),
            last_progress: parking_lot::Mutex::new(None),
            initialized_at: parking_lot::Mutex::new(None),
            root_uri: parking_lot::RwLock::new(String::new()),
            stderr_tail: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            open_versions: DashMap::new(),
        });

        // I/O pump: reads messages from the server, resolves pending requests,
        // stores diagnostics, and answers what the server asks.
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut reader: BoxedReader = BufReader::new(reader);
                let reason = loop {
                    match read_message(&mut reader).await {
                        Ok(msg) => dispatch_incoming(msg, &shared).await,
                        Err(e) => break e.to_string(),
                    }
                };
                let tail = shared.stderr_tail_text();
                let detail = if tail.is_empty() {
                    reason.clone()
                } else {
                    format!("{reason}; last output:\n{tail}")
                };
                tracing::debug!("LSP server {} reader exited: {detail}", shared.server_name);
                shared.fail_pending(&format!(
                    "language server '{}' stopped: {detail}",
                    shared.server_name
                ));
            });
        }

        Self {
            server_name,
            server_config: config,
            process: None,
            request_id: Arc::new(AtomicU64::new(1)),
            is_initialized: false,
            shared,
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Diagnostics indexed by URI.
    pub fn diagnostics(&self) -> &DashMap<String, Vec<LspDiagnostic>> {
        &self.shared.diagnostics
    }

    /// How many times the server has published diagnostics for `uri`.
    ///
    /// A caller captures this before an edit and waits for it to change, which
    /// is the only way to tell a fresh answer from the previous one.
    pub fn diagnostic_version(&self, uri: &str) -> u64 {
        self.shared
            .diagnostic_versions
            .get(uri)
            .map(|v| *v)
            .unwrap_or(0)
    }

    /// What the server said it supports.
    pub fn server_capabilities(&self) -> serde_json::Value {
        self.shared.server_capabilities.read().clone()
    }

    /// Whether the server advertises support for `capability`.
    ///
    /// A dotted path walks nested objects, e.g. `"renameProvider"` or
    /// `"workspace.fileOperations.willRename"`. A server that does not
    /// advertise a request usually answers "method not found", and asking
    /// anyway turns a clean "not supported" into an error.
    pub fn supports(&self, capability: &str) -> bool {
        let caps = self.shared.server_capabilities.read();
        let mut node = &*caps;
        for part in capability.split('.') {
            match node.get(part) {
                Some(next) => node = next,
                None => return false,
            }
        }
        !matches!(
            node,
            serde_json::Value::Null | serde_json::Value::Bool(false)
        )
    }

    /// Send a JSON-RPC request and wait for the matching response.
    async fn send_request_inner(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.send_request_with_timeout(method, params, self.server_config.request_timeout())
            .await
    }

    async fn send_request_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id();
        let body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;

        let (tx, rx) = oneshot::channel();
        self.shared.pending.insert(id, tx);
        if let Err(e) = self.shared.send(&body).await {
            self.shared.pending.remove(&id);
            return Err(e);
        }

        let response = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                return Err(anyhow::anyhow!(
                    "LSP request '{}' was dropped (server: {})",
                    method,
                    self.server_name
                ))
            }
            Err(_) => {
                // Tell the server to stop working on it. Without this the
                // server keeps computing an answer nobody will read, which on
                // a large project is the difference between one wasted request
                // and a server that never catches up.
                self.shared.pending.remove(&id);
                let _ = self
                    .shared
                    .notify("$/cancelRequest", json!({ "id": id }))
                    .await;
                return Err(anyhow::anyhow!(
                    "LSP request '{}' timed out after {}ms (server: {})",
                    method,
                    timeout.as_millis(),
                    self.server_name
                ));
            }
        };

        if let Some(err) = response.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default();
            return Err(anyhow::anyhow!(
                "LSP error from {} on {}: {}",
                self.server_name,
                method,
                if message.is_empty() {
                    err.to_string()
                } else {
                    message.to_string()
                }
            ));
        }
        Ok(response["result"].clone())
    }

    /// Send a JSON-RPC notification (fire-and-forget, no response expected).
    async fn send_notification_inner(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.shared.notify(method, params).await
    }

    /// Perform the LSP `initialize` / `initialized` handshake.
    pub async fn initialize(&mut self, root_uri: &str) -> anyhow::Result<()> {
        *self.shared.root_uri.write() = root_uri.to_string();
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "mikmik", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": root_uri,
            "workspaceFolders": [{
                "uri": root_uri,
                "name": uri_to_path(root_uri)
                    .rsplit(std::path::MAIN_SEPARATOR)
                    .next()
                    .unwrap_or("workspace")
                    .to_string(),
            }],
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {
                        "relatedInformation": true,
                        "versionSupport": true,
                        "codeDescriptionSupport": false
                    },
                    "synchronization": {
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                        "didSave": true
                    },
                    "definition": { "linkSupport": true },
                    "typeDefinition": { "linkSupport": true },
                    "implementation": { "linkSupport": true },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "rename": { "prepareSupport": false },
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": { "valueSet": [] }
                        },
                        "resolveSupport": { "properties": ["edit"] }
                    }
                },
                "workspace": {
                    // Answered by the reader task. Claiming it while ignoring
                    // the request would leave the server waiting.
                    "configuration": true,
                    "workspaceFolders": true,
                    "applyEdit": true,
                    "didChangeConfiguration": { "dynamicRegistration": false },
                    "didChangeWatchedFiles": { "dynamicRegistration": true },
                    "symbol": { "dynamicRegistration": false },
                    "workspaceEdit": {
                        "documentChanges": true,
                        "resourceOperations": ["create", "rename", "delete"]
                    },
                    "fileOperations": {
                        "willRename": true,
                        "didRename": true
                    }
                },
                "window": { "workDoneProgress": true }
            },
            "initializationOptions": self.server_config.initialization_options,
        });

        // The handshake gets its own budget: a server that never answers it is
        // broken, and waiting a full request timeout to learn that delays every
        // caller behind it.
        let result = self
            .send_request_with_timeout("initialize", params, self.server_config.warmup_timeout())
            .await?;
        *self.shared.server_capabilities.write() = result
            .get("capabilities")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // Send the `initialized` notification to complete the handshake
        self.send_notification_inner("initialized", json!({}))
            .await?;
        *self.shared.initialized_at.lock() = Some(std::time::Instant::now());

        // Settings, unlike `initializationOptions`, can be pushed again later.
        self.push_settings().await;

        self.is_initialized = true;
        tracing::debug!("LSP server '{}' initialized", self.server_name);
        Ok(())
    }

    /// Send the configured settings with `workspace/didChangeConfiguration`.
    ///
    /// A failure is logged rather than returned: the server is usable without
    /// its settings, and refusing to start over a rejected setting would be
    /// worse than running with the defaults.
    pub async fn push_settings(&self) {
        let Some(settings) = self.server_config.settings.clone() else {
            return;
        };
        if let Err(e) = self
            .send_notification_inner(
                "workspace/didChangeConfiguration",
                json!({ "settings": settings }),
            )
            .await
        {
            tracing::debug!("could not push settings to {}: {e}", self.server_name);
        }
    }

    /// Wait until the server has finished loading the project.
    ///
    /// A project-aware server answers navigation with an empty result while it
    /// is still indexing, which reads as "no definition" rather than "not
    /// ready". Waiting is bounded, and a server that reports no progress at all
    /// is not waited for beyond the settle window after its handshake.
    pub async fn wait_for_project_loaded(&self) {
        let timings = self.server_config.ready_timings();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timings.timeout_ms);
        let settle = std::time::Duration::from_millis(timings.settle_ms);
        let poll = std::time::Duration::from_millis(timings.poll_ms.max(10));

        while std::time::Instant::now() < deadline {
            let busy = !self.shared.active_progress.is_empty();
            if !busy {
                // Quiet now, but the server may not have started reporting
                // yet. The reference point is the last thing that happened:
                // a progress notification if there was one, the handshake
                // otherwise.
                let since = {
                    let last = *self.shared.last_progress.lock();
                    let started = *self.shared.initialized_at.lock();
                    last.or(started)
                };
                match since {
                    Some(at) if at.elapsed() >= settle => return,
                    None => return,
                    _ => {}
                }
            }
            tokio::time::sleep(poll).await;
        }
        tracing::debug!(
            "{} did not report the project loaded within {}ms",
            self.server_name,
            timings.timeout_ms
        );
    }

    /// Whether the server is loading the project right now.
    pub fn is_loading_project(&self) -> bool {
        !self.shared.active_progress.is_empty()
    }

    /// Notify the server that a document has been opened.
    pub async fn open_document(
        &mut self,
        uri: &str,
        language_id: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        self.shared.open_versions.insert(uri.to_string(), 1);
        self.send_notification_inner(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": content,
                }
            }),
        )
        .await
    }

    /// Whether this client has the document open.
    pub fn has_open(&self, uri: &str) -> bool {
        self.shared.open_versions.contains_key(uri)
    }

    /// Notify the server that a document has been changed.
    pub async fn change_document(
        &mut self,
        uri: &str,
        content: &str,
        version: i64,
    ) -> anyhow::Result<()> {
        self.shared.open_versions.insert(uri.to_string(), version);
        self.send_notification_inner(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": content }],
            }),
        )
        .await
    }

    /// Send the current content of a document, opening it if needed.
    ///
    /// The server answers against the copy it holds, so a document opened once
    /// and edited afterwards would be answered from the text as it was at
    /// open time.
    pub async fn sync_document(
        &mut self,
        uri: &str,
        language_id: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        match self.shared.open_versions.get(uri).map(|v| *v) {
            Some(version) => self.change_document(uri, content, version + 1).await,
            None => self.open_document(uri, language_id, content).await,
        }
    }

    /// Notify the server that a document has been saved.
    pub async fn save_document(&mut self, uri: &str) -> anyhow::Result<()> {
        self.send_notification_inner(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await
    }

    /// Tell the server that files changed on disk outside the editor.
    ///
    /// `changes` pairs a URI with an LSP `FileChangeType`: 1 created,
    /// 2 changed, 3 deleted.
    pub async fn notify_watched_files(&self, changes: &[(String, u8)]) -> anyhow::Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let changes: Vec<serde_json::Value> = changes
            .iter()
            .map(|(uri, kind)| json!({ "uri": uri, "type": kind }))
            .collect();
        self.send_notification_inner(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": changes }),
        )
        .await
    }

    /// Notify the server that a document has been closed.
    pub async fn close_document(&mut self, uri: &str) -> anyhow::Result<()> {
        self.send_notification_inner(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await
    }

    /// Get hover information at a position (1-based line/column).
    pub async fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Option<String>> {
        // LSP protocol is 0-based
        let result = self
            .send_request_inner(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": {
                        "line": line.saturating_sub(1),
                        "character": character.saturating_sub(1),
                    }
                }),
            )
            .await?;

        if result.is_null() {
            return Ok(None);
        }

        // The result can be { contents: MarkupContent | MarkedString | MarkedString[] }
        let contents = &result["contents"];
        let text = if let Some(value) = contents.get("value").and_then(|v| v.as_str()) {
            // MarkupContent { kind, value }
            value.to_string()
        } else if let Some(s) = contents.as_str() {
            // Plain string
            s.to_string()
        } else if let Some(arr) = contents.as_array() {
            // Array of MarkedStrings
            arr.iter()
                .filter_map(|item| {
                    item.get("value")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.as_str())
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        } else {
            return Ok(None);
        };

        if text.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(text))
        }
    }

    /// Get definition locations for a position (1-based line/column).
    /// Returns a list of `"file_path:line"` strings.
    pub async fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let result = self
            .send_request_inner(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri },
                    "position": {
                        "line": line.saturating_sub(1),
                        "character": character.saturating_sub(1),
                    }
                }),
            )
            .await?;

        Ok(extract_locations(&result))
    }

    /// Get all references for a symbol at a position (1-based line/column).
    pub async fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let result = self
            .send_request_inner(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri },
                    "position": {
                        "line": line.saturating_sub(1),
                        "character": character.saturating_sub(1),
                    },
                    "context": { "includeDeclaration": true }
                }),
            )
            .await?;

        Ok(extract_locations(&result))
    }

    /// List document symbols for a file.
    pub async fn document_symbols(&self, uri: &str) -> anyhow::Result<Vec<String>> {
        let result = self
            .send_request_inner(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await?;

        let mut symbols = Vec::new();
        if let serde_json::Value::Array(arr) = &result {
            for sym in arr {
                collect_symbol(sym, 0, &mut symbols);
            }
        }
        Ok(symbols)
    }

    /// Get cached diagnostics for `file_path`.
    pub fn get_diagnostics(&self, file_path: &str) -> Vec<LspDiagnostic> {
        let uri = path_to_uri(file_path);
        self.shared
            .diagnostics
            .get(&uri)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Get all cached diagnostics across every file.
    pub fn all_diagnostics(&self) -> Vec<LspDiagnostic> {
        self.shared
            .diagnostics
            .iter()
            .flat_map(|entry| entry.value().clone())
            .collect()
    }

    /// Returns `true` if `initialize` has completed successfully.
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }

    /// Gracefully shut down the server.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        if !self.is_initialized {
            return Ok(());
        }
        // Attempt graceful shutdown; ignore errors since we kill anyway. Its
        // own short budget: a server that ignores `shutdown` must not hold the
        // session open for a full request timeout.
        let _ = self
            .send_request_with_timeout(
                "shutdown",
                json!(null),
                std::time::Duration::from_millis(2_000),
            )
            .await;
        let _ = self.send_notification_inner("exit", json!(null)).await;

        // Drop the writer so the pipe closes cleanly before we wait.
        self.shared.writer.lock().await.take();

        if let Some(mut child) = self.process.take() {
            let pid = child.id();
            // Give the process a moment to exit cleanly.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            // The tree first: killing the server orphans whatever it started,
            // and a language server that spawns a compiler or a watcher would
            // otherwise leave it behind.
            if let Some(pid) = pid {
                crate::process_tree::kill_tree(pid);
            }
            let _ = child.kill().await;
        }
        self.is_initialized = false;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Incoming message dispatch
// ---------------------------------------------------------------------------

async fn dispatch_incoming(msg: serde_json::Value, shared: &Arc<ClientShared>) {
    let method = msg
        .get("method")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // A message carrying an id and no method is a response to us. One carrying
    // both is a request from the server, which the specification says we must
    // answer even when the answer is "not implemented".
    match (msg.get("id"), method.as_deref()) {
        (Some(id), None) => {
            if let Some(id) = id.as_u64() {
                if let Some((_, tx)) = shared.pending.remove(&id) {
                    let _ = tx.send(msg);
                }
            }
        }
        (Some(id), Some(method)) => {
            let id = id.clone();
            handle_server_request(shared, &id, method, &msg["params"]).await;
        }
        (None, Some(method)) => handle_notification(shared, method, &msg["params"]),
        (None, None) => {}
    }
}

/// Answer a request the server sent us.
async fn handle_server_request(
    shared: &Arc<ClientShared>,
    id: &serde_json::Value,
    method: &str,
    params: &serde_json::Value,
) {
    match method {
        "workspace/configuration" => {
            // One entry per requested section, in the order asked. A server
            // that gets a shorter array than it asked for reads the wrong
            // section for the rest.
            let items = params.get("items").and_then(|i| i.as_array());
            let settings = shared.config.settings.clone();
            let answer: Vec<serde_json::Value> = items
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            let section = item.get("section").and_then(|s| s.as_str());
                            settings_section(settings.as_ref(), section)
                        })
                        .collect()
                })
                .unwrap_or_default();
            shared.respond(id, serde_json::Value::Array(answer)).await;
        }
        "workspace/workspaceFolders" => {
            let uri = shared.root_uri.read().clone();
            let answer = if uri.is_empty() {
                serde_json::Value::Null
            } else {
                let name = uri_to_path(&uri)
                    .rsplit(std::path::MAIN_SEPARATOR)
                    .next()
                    .unwrap_or("workspace")
                    .to_string();
                json!([{ "uri": uri, "name": name }])
            };
            shared.respond(id, answer).await;
        }
        "window/workDoneProgress/create" => {
            if let Some(token) = progress_token(params) {
                shared.active_progress.insert(token, ());
            }
            shared.respond(id, serde_json::Value::Null).await;
        }
        "client/registerCapability" | "client/unregisterCapability" => {
            // Accepted without acting on it: everything this client sends is
            // decided by its own configuration, not by what the server
            // registers at runtime. The answer still has to arrive.
            shared.respond(id, serde_json::Value::Null).await;
        }
        "window/showMessageRequest" => {
            // No user is watching this server's messages, so no action is
            // picked. Null is the specified answer for "dismissed".
            if let Some(message) = params.get("message").and_then(|m| m.as_str()) {
                tracing::debug!("[LSP {}] {message}", shared.server_name);
            }
            shared.respond(id, serde_json::Value::Null).await;
        }
        "window/showDocument" => {
            // Nothing here can bring a document to the front.
            shared.respond(id, json!({ "success": false })).await;
        }
        "workspace/applyEdit" => {
            match apply_workspace_edit(params.get("edit").unwrap_or(&serde_json::Value::Null)) {
                Ok(summary) => {
                    tracing::debug!(
                        "applied a server-initiated edit from {}: {} file(s)",
                        shared.server_name,
                        summary.len()
                    );
                    shared.respond(id, json!({ "applied": true })).await;
                }
                Err(e) => {
                    shared
                        .respond(
                            id,
                            json!({ "applied": false, "failureReason": e.to_string() }),
                        )
                        .await;
                }
            }
        }
        other => {
            tracing::debug!("[LSP {}] unhandled request '{other}'", shared.server_name);
            shared.respond_method_not_found(id, other).await;
        }
    }
}

/// The settings a server asked for, by dotted section name.
///
/// A server asks for `rust-analyzer` and expects the object stored under that
/// key, not the whole settings blob.
fn settings_section(
    settings: Option<&serde_json::Value>,
    section: Option<&str>,
) -> serde_json::Value {
    let Some(settings) = settings else {
        return serde_json::Value::Null;
    };
    let Some(section) = section.filter(|s| !s.is_empty()) else {
        return settings.clone();
    };
    let mut node = settings;
    for part in section.split('.') {
        match node.get(part) {
            Some(next) => node = next,
            None => return serde_json::Value::Null,
        }
    }
    node.clone()
}

fn progress_token(params: &serde_json::Value) -> Option<String> {
    params.get("token").map(|t| match t {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn handle_notification(shared: &Arc<ClientShared>, method: &str, params: &serde_json::Value) {
    match method {
        "textDocument/publishDiagnostics" => {
            handle_publish_diagnostics(params, shared);
        }
        "$/progress" => {
            *shared.last_progress.lock() = Some(std::time::Instant::now());
            let Some(token) = progress_token(params) else {
                return;
            };
            match params.pointer("/value/kind").and_then(|k| k.as_str()) {
                Some("begin") => {
                    shared.active_progress.insert(token, ());
                }
                Some("end") => {
                    shared.active_progress.remove(&token);
                }
                _ => {}
            }
        }
        "window/logMessage" | "window/showMessage" => {
            if let Some(message) = params.get("message").and_then(|m| m.as_str()) {
                tracing::debug!("[LSP {}] {message}", shared.server_name);
            }
        }
        "telemetry/event" => {}
        other => {
            tracing::trace!(
                "LSP server {}: unhandled notification '{other}'",
                shared.server_name
            );
        }
    }
}

fn handle_publish_diagnostics(params: &serde_json::Value, shared: &Arc<ClientShared>) {
    let server_name = shared.server_name.as_str();
    let diagnostics = &shared.diagnostics;
    let uri = match params.get("uri").and_then(|v| v.as_str()) {
        Some(u) => u.to_string(),
        None => return,
    };

    // Bumped whether or not the list is empty. "The file is clean now" is an
    // answer, and a caller waiting for a fresh publish has to see it.
    *shared.diagnostic_versions.entry(uri.clone()).or_insert(0) += 1;

    let raw_diags = match params.get("diagnostics").and_then(|v| v.as_array()) {
        Some(d) => d,
        None => {
            diagnostics.insert(uri, Vec::new());
            return;
        }
    };

    // Convert the URI back to a file path for storage
    let file_path = uri_to_path(&uri);

    let parsed: Vec<LspDiagnostic> = raw_diags
        .iter()
        .filter_map(|d| parse_diagnostic(d, &file_path, server_name))
        .collect();

    tracing::debug!(
        "LSP server {}: {} diagnostics for {}",
        server_name,
        parsed.len(),
        file_path
    );

    diagnostics.insert(uri, parsed);
}

fn parse_diagnostic(
    d: &serde_json::Value,
    file_path: &str,
    server_name: &str,
) -> Option<LspDiagnostic> {
    let range = d.get("range")?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as u32 + 1; // LSP is 0-based
    let column = start.get("character")?.as_u64()? as u32 + 1;
    let message = d.get("message")?.as_str()?.to_string();

    let severity = d
        .get("severity")
        .and_then(|v| v.as_u64())
        .map(DiagnosticSeverity::from_lsp_int)
        .unwrap_or(DiagnosticSeverity::Error);

    let source = d
        .get("source")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| Some(server_name.to_string()));

    let code = d.get("code").map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    });

    Some(LspDiagnostic {
        file: file_path.to_string(),
        line,
        column,
        severity,
        message,
        source,
        code,
    })
}

// ---------------------------------------------------------------------------
// Applying edits
// ---------------------------------------------------------------------------

/// One text replacement, in LSP coordinates (0-based line and character).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub new_text: String,
}

impl TextEdit {
    fn from_json(value: &serde_json::Value) -> Option<Self> {
        let range = value.get("range")?;
        Some(Self {
            start_line: range.pointer("/start/line")?.as_u64()? as u32,
            start_character: range.pointer("/start/character")?.as_u64()? as u32,
            end_line: range.pointer("/end/line")?.as_u64()? as u32,
            end_character: range.pointer("/end/character")?.as_u64()? as u32,
            new_text: value.get("newText")?.as_str()?.to_string(),
        })
    }

    fn starts_before(&self, other: &Self) -> std::cmp::Ordering {
        (self.start_line, self.start_character).cmp(&(other.start_line, other.start_character))
    }

    fn overlaps(&self, other: &Self) -> bool {
        let self_end = (self.end_line, self.end_character);
        let other_start = (other.start_line, other.start_character);
        let other_end = (other.end_line, other.end_character);
        let self_start = (self.start_line, self.start_character);
        self_start < other_end && other_start < self_end
    }
}

/// Apply `edits` to `content` and return the result.
///
/// Edits arrive in an unspecified order and all address the original text, so
/// they are applied from the end backwards: an earlier edit would otherwise
/// move every later position. Two edits that overlap describe two different
/// results for the same characters, which is a server bug rather than
/// something to resolve silently.
pub fn apply_text_edits(content: &str, edits: &[TextEdit]) -> anyhow::Result<String> {
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by(|a, b| a.starts_before(b));
    for pair in sorted.windows(2) {
        if pair[0].overlaps(pair[1]) {
            return Err(anyhow::anyhow!(
                "the server returned two edits that overlap at line {}",
                pair[0].start_line + 1
            ));
        }
    }

    // Byte offset of the first character of each line, plus the end of the
    // text, so an edit that ends at the last line has somewhere to point.
    let mut line_starts: Vec<usize> = vec![0];
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            line_starts.push(index + 1);
        }
    }

    // A character offset in LSP is a UTF-16 code unit offset.
    let offset_of = |line: u32, character: u32| -> anyhow::Result<usize> {
        let start = *line_starts.get(line as usize).ok_or_else(|| {
            anyhow::anyhow!(
                "the server named line {}, past the end of the file",
                line + 1
            )
        })?;
        let rest = &content[start..];
        let line_text = rest.split('\n').next().unwrap_or("");
        let mut utf16 = 0u32;
        for (byte_index, ch) in line_text.char_indices() {
            if utf16 >= character {
                return Ok(start + byte_index);
            }
            utf16 += ch.len_utf16() as u32;
        }
        Ok(start + line_text.len())
    };

    let mut result = content.to_string();
    for edit in sorted.into_iter().rev() {
        let from = offset_of(edit.start_line, edit.start_character)?;
        let to = offset_of(edit.end_line, edit.end_character)?;
        if from > to || to > result.len() {
            return Err(anyhow::anyhow!(
                "the server returned an edit that runs backwards"
            ));
        }
        result.replace_range(from..to, &edit.new_text);
    }
    Ok(result)
}

/// Every file a `WorkspaceEdit` touches, and the edits for each.
///
/// Reads both shapes the specification allows: the `changes` map and the
/// `documentChanges` array. A server picks one, and which one it picks depends
/// on what the client said it supports.
pub fn workspace_edit_files(edit: &serde_json::Value) -> Vec<(String, Vec<TextEdit>)> {
    let mut out: Vec<(String, Vec<TextEdit>)> = Vec::new();
    let mut push = |uri: String, edits: Vec<TextEdit>| {
        if edits.is_empty() {
            return;
        }
        match out.iter_mut().find(|(existing, _)| *existing == uri) {
            Some((_, existing)) => existing.extend(edits),
            None => out.push((uri, edits)),
        }
    };

    if let Some(changes) = edit.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in changes {
            let parsed = edits
                .as_array()
                .map(|a| a.iter().filter_map(TextEdit::from_json).collect())
                .unwrap_or_default();
            push(uri.clone(), parsed);
        }
    }

    if let Some(document_changes) = edit.get("documentChanges").and_then(|c| c.as_array()) {
        for change in document_changes {
            // A resource operation (create, rename, delete) carries `kind`
            // instead of `edits`. Applying one silently would delete or move a
            // file the caller never heard about.
            if change.get("kind").is_some() {
                continue;
            }
            let Some(uri) = change.pointer("/textDocument/uri").and_then(|u| u.as_str()) else {
                continue;
            };
            let parsed = change
                .get("edits")
                .and_then(|e| e.as_array())
                .map(|a| a.iter().filter_map(TextEdit::from_json).collect())
                .unwrap_or_default();
            push(uri.to_string(), parsed);
        }
    }

    out
}

/// The resource operations a `WorkspaceEdit` asks for, as human-readable text.
///
/// They are reported rather than performed: creating, renaming or deleting a
/// file is not something a hover or a rename request should do behind the
/// caller's back.
pub fn workspace_edit_resource_operations(edit: &serde_json::Value) -> Vec<String> {
    edit.get("documentChanges")
        .and_then(|c| c.as_array())
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| {
                    let kind = change.get("kind")?.as_str()?;
                    let target = change
                        .get("uri")
                        .or_else(|| change.get("newUri"))
                        .and_then(|u| u.as_str())
                        .map(uri_to_path)
                        .unwrap_or_default();
                    Some(format!("{kind} {target}"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Apply a `WorkspaceEdit` to the files on disk.
///
/// Returns one line per file, naming how many edits landed. Every file is read
/// and written once, so a file edited in several places is not rewritten per
/// edit.
pub fn apply_workspace_edit(edit: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let files = workspace_edit_files(edit);
    if files.is_empty() {
        return Ok(Vec::new());
    }

    // Everything is computed before anything is written, so a failure on the
    // third file does not leave the first two changed.
    let mut planned: Vec<(std::path::PathBuf, String, usize)> = Vec::new();
    for (uri, edits) in &files {
        let path = std::path::PathBuf::from(uri_to_path(uri));
        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        let updated = apply_text_edits(&content, edits)?;
        planned.push((path, updated, edits.len()));
    }

    let mut applied = Vec::new();
    for (path, content, count) in planned {
        std::fs::write(&path, content)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
        applied.push(format!("{}: {count} edit(s)", path.display()));
    }
    Ok(applied)
}

// ---------------------------------------------------------------------------
// Location / symbol helpers
// ---------------------------------------------------------------------------

/// A place in a file, as a server reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocation {
    pub file: String,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number.
    pub column: u32,
}

impl std::fmt::Display for LspLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

/// Read the locations out of a navigation result.
///
/// The specification allows four shapes: one `Location`, an array of them, one
/// `LocationLink`, or an array of those. A `LocationLink` names the file in
/// `targetUri` and the position in `targetSelectionRange`, so a reader that
/// only knows `uri` returns nothing at all for a server that sends links, and
/// this client asks for link support in its handshake.
pub fn parse_locations(result: &serde_json::Value) -> Vec<LspLocation> {
    let items: Vec<&serde_json::Value> = if let Some(arr) = result.as_array() {
        arr.iter().collect()
    } else if result.is_object() {
        vec![result]
    } else {
        return Vec::new();
    };

    items
        .into_iter()
        .filter_map(|loc| {
            let (uri, range) = match loc.get("targetUri").and_then(|u| u.as_str()) {
                Some(uri) => {
                    // The selection range points at the name itself; the target
                    // range covers the whole declaration.
                    let range = loc
                        .get("targetSelectionRange")
                        .or_else(|| loc.get("targetRange"))?;
                    (uri, range)
                }
                None => (loc.get("uri")?.as_str()?, loc.get("range")?),
            };
            Some(LspLocation {
                file: uri_to_path(uri),
                line: range
                    .pointer("/start/line")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32
                    + 1,
                column: range
                    .pointer("/start/character")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32
                    + 1,
            })
        })
        .collect()
}

/// The same locations, formatted as `path:line:column`.
fn extract_locations(result: &serde_json::Value) -> Vec<String> {
    parse_locations(result)
        .into_iter()
        .map(|loc| loc.to_string())
        .collect()
}

/// Recursively collect symbol names from a DocumentSymbol or SymbolInformation node.
fn collect_symbol(sym: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let name = sym
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("<unnamed>");
    let kind = sym.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
    let kind_str = symbol_kind_name(kind);
    out.push(format!("{}{} ({})", indent, name, kind_str));

    // DocumentSymbol may have nested children
    if let Some(children) = sym.get("children").and_then(|c| c.as_array()) {
        for child in children {
            collect_symbol(child, depth + 1, out);
        }
    }
}

fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum-member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type-parameter",
        _ => "symbol",
    }
}

// ---------------------------------------------------------------------------
// URI helpers
// ---------------------------------------------------------------------------

/// Characters a path may hold that a URI may not carry unescaped.
///
/// Only the ones that actually appear in file names are escaped. Escaping more
/// would be harmless for a compliant server, but several answer with the URI
/// spelled the way the client sent it, and an over-escaped URI then fails to
/// match the one the client remembers.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '%' => out.push_str("%25"),
            other => out.push(other),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or_default();
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `file:` URI for a path.
///
/// A path that is already a URI is returned unchanged, because a caller that
/// read a URI out of a server response must be able to pass it straight back.
pub fn path_to_uri(path: &str) -> String {
    if path.starts_with("file://") {
        return path.to_string();
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path));
    let text = canonical.to_string_lossy();
    // Windows hands back `\\?\C:\...` for a canonical path, which no server
    // understands.
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let slashed = text.replace('\\', "/");
    let encoded = percent_encode_path(&slashed);
    if encoded.starts_with('/') {
        // Unix: the path already carries the root slash, and the authority is
        // empty, so `file://` plus `/home/...` is the right three slashes.
        format!("file://{encoded}")
    } else {
        // Windows: a drive letter needs the slash the path does not have.
        format!("file:///{encoded}")
    }
}

/// The filesystem path a `file:` URI names.
pub fn uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    // Skip the authority, which is empty for a local file.
    let rest = match rest.find('/') {
        Some(0) => rest,
        Some(index) => &rest[index..],
        None => rest,
    };
    let decoded = percent_decode(rest);

    #[cfg(windows)]
    {
        // `/C:/src` is the drive-letter form; the leading slash is not part of
        // the path.
        let trimmed = decoded
            .strip_prefix('/')
            .filter(|rest| {
                let bytes = rest.as_bytes();
                bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
            })
            .unwrap_or(&decoded);
        trimmed.replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        decoded
    }
}

// ---------------------------------------------------------------------------
// Diagnostic formatting (shared utility)
// ---------------------------------------------------------------------------

impl LspManager {
    /// Format a slice of diagnostics into a human-readable multi-line string
    /// suitable for inclusion in tool output or TUI display.
    pub fn format_diagnostics(diagnostics: &[LspDiagnostic]) -> String {
        if diagnostics.is_empty() {
            return "No diagnostics.".to_string();
        }
        diagnostics
            .iter()
            .map(|d| {
                format!(
                    "[{}] {}:{}:{} - {}{}{}",
                    d.severity.as_str().to_uppercase(),
                    d.file,
                    d.line,
                    d.column,
                    d.message,
                    d.source
                        .as_deref()
                        .map(|s| format!(" ({})", s))
                        .unwrap_or_default(),
                    d.code
                        .as_deref()
                        .map(|c| format!(" [{}]", c))
                        .unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ---------------------------------------------------------------------------
// LspManager — registry and multi-server coordination
// ---------------------------------------------------------------------------

/// Manages a collection of [`LspClient`] instances, routing file operations
/// to the correct server based on extension mappings.
pub struct LspManager {
    /// Registered configs (used for lookup before a client is started)
    configs: Vec<LspServerConfig>,
    /// Running clients keyed by server name
    clients: HashMap<String, LspClient>,
    /// Set of file URIs that have been opened on a specific server (URI → server name)
    opened_files: HashMap<String, String>,
    /// Directories already scanned for catalogue servers.
    detected_roots: std::collections::HashSet<std::path::PathBuf>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            configs: Vec::new(),
            clients: HashMap::new(),
            opened_files: HashMap::new(),
            detected_roots: std::collections::HashSet::new(),
        }
    }

    /// Register an LSP server configuration.
    ///
    /// A second registration of the same name replaces the first, so a
    /// project's entry overrides the user's rather than joining it.
    pub fn register_server(&mut self, config: LspServerConfig) {
        match self.configs.iter_mut().find(|c| c.name == config.name) {
            Some(existing) => *existing = config,
            None => self.configs.push(config),
        }
    }

    /// Return all registered server configurations.
    pub fn servers(&self) -> &[LspServerConfig] {
        &self.configs
    }

    /// Look up a server configuration by name.
    pub fn server_by_name(&self, name: &str) -> Option<&LspServerConfig> {
        self.configs.iter().find(|s| s.name == name)
    }

    /// Public wrapper: find the first server name that handles `file_path`.
    /// Returns `None` when no server is configured for the file.
    pub fn server_name_for_file_pub(&self, file_path: &str) -> Option<&str> {
        self.server_name_for_file(file_path)
    }

    /// Every enabled server that handles `file_path`, primary servers first.
    ///
    /// A linter reports problems and nothing else, so it sorts last and a
    /// navigation request never reaches it.
    pub fn servers_for_file(&self, file_path: &str) -> Vec<&LspServerConfig> {
        let mut matched: Vec<&LspServerConfig> = self
            .configs
            .iter()
            .filter(|c| !c.disabled && c.handles_file(file_path))
            .collect();
        matched.sort_by_key(|c| c.is_linter);
        matched
    }

    /// The one server that answers navigation for `file_path`.
    pub fn primary_server_for_file(&self, file_path: &str) -> Option<&LspServerConfig> {
        self.servers_for_file(file_path)
            .into_iter()
            .find(|c| !c.is_linter)
    }

    /// Find the first server name that handles `file_path`.
    fn server_name_for_file(&self, file_path: &str) -> Option<&str> {
        self.primary_server_for_file(file_path)
            .map(|c| c.name.as_str())
    }

    /// Spawn and initialize the server for `file_path` if it is not already
    /// running.  Returns `None` when no server is configured for this file type.
    async fn ensure_started(
        &mut self,
        file_path: &str,
        root_dir: &Path,
    ) -> anyhow::Result<Option<&mut LspClient>> {
        let server_name = match self.server_name_for_file(file_path) {
            Some(n) => n.to_string(),
            None => return Ok(None),
        };

        if !self.clients.contains_key(&server_name) {
            let config = match self.configs.iter().find(|c| c.name == server_name) {
                Some(c) => c.clone(),
                None => return Ok(None),
            };
            match LspClient::start(config).await {
                Ok(mut client) => {
                    let root_uri = path_to_uri(&root_dir.to_string_lossy());
                    if let Err(e) = client.initialize(&root_uri).await {
                        tracing::warn!("Failed to initialize LSP server '{}': {}", server_name, e);
                        // Don't insert — allow retry on next call
                        return Ok(None);
                    }
                    self.clients.insert(server_name.clone(), client);
                }
                Err(e) => {
                    tracing::warn!("Failed to start LSP server '{}': {}", server_name, e);
                    return Ok(None);
                }
            }
        }

        Ok(self.clients.get_mut(&server_name))
    }

    /// Spawn and initialize servers for all registered configurations.
    pub async fn start_servers(&mut self, root_dir: &Path) {
        let configs: Vec<LspServerConfig> = self
            .configs
            .iter()
            .filter(|c| !c.disabled)
            .cloned()
            .collect();
        for config in configs {
            let name = config.name.clone();
            if self.clients.contains_key(&name) {
                continue;
            }
            match LspClient::start(config).await {
                Ok(mut client) => {
                    let root_uri = path_to_uri(&root_dir.to_string_lossy());
                    if let Err(e) = client.initialize(&root_uri).await {
                        tracing::warn!("Failed to initialize LSP server '{}': {}", name, e);
                        continue;
                    }
                    self.clients.insert(name.clone(), client);
                    tracing::info!("LSP server '{}' started", name);
                }
                Err(e) => {
                    tracing::warn!("Failed to start LSP server '{}': {}", name, e);
                }
            }
        }
    }

    /// Open a file on the appropriate LSP server.
    pub async fn open_file(&mut self, file_path: &str, root_dir: &Path) -> anyhow::Result<()> {
        let uri = path_to_uri(file_path);
        let server_name = match self.server_name_for_file(file_path) {
            Some(n) => n.to_string(),
            None => return Ok(()),
        };

        // Skip if already opened on this server
        if self.opened_files.get(&uri).map(|s| s.as_str()) == Some(server_name.as_str()) {
            return Ok(());
        }

        let content = match tokio::fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Cannot read '{}' for LSP: {}",
                    file_path,
                    e
                ))
            }
        };

        // Ensure the server is running first (borrows self mutably, so must
        // finish before we borrow opened_files).
        self.ensure_started(file_path, root_dir).await?;

        if let Some(client) = self.clients.get_mut(&server_name) {
            let lang = client.server_config.language_for_file(file_path);
            client.open_document(&uri, &lang, &content).await?;
            self.opened_files.insert(uri, server_name);
        }
        Ok(())
    }

    /// Register every server in `configs`, replacing an entry of the same name.
    ///
    /// Called before each tool use with the session's merged configuration, so
    /// an edit to `settings.json` reaches a manager that is already populated.
    pub fn seed_from_config(&mut self, configs: &[LspServerConfig]) {
        for cfg in configs {
            self.register_server(cfg.clone());
        }
    }

    /// Register the catalogue servers that suit `cwd`.
    ///
    /// Scans a directory once, because the scan reads the directory and looks
    /// for a binary, and neither answer changes inside a session.
    ///
    /// Call this **before** [`Self::seed_from_config`]: a user entry of the
    /// same name has to replace the catalogue's, not the other way round.
    pub fn seed_detected(&mut self, cwd: &Path) {
        if !self.detected_roots.insert(cwd.to_path_buf()) {
            return;
        }
        for server in detect_servers(cwd) {
            tracing::debug!(
                "detected language server '{}' for {}",
                server.name,
                cwd.display()
            );
            self.register_server(server);
        }
    }

    /// Forget which directories were scanned, so the next call re-detects.
    ///
    /// A project gains a marker or the user installs a server mid-session, and
    /// nothing else would notice.
    pub fn forget_detection(&mut self) {
        self.detected_roots.clear();
    }

    /// Get hover information for `file_path` at the given 1-based position.
    pub async fn hover(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Option<String>> {
        let uri = path_to_uri(file_path);
        let server_name = self
            .server_name_for_file(file_path)
            .ok_or_else(|| anyhow::anyhow!("No LSP server configured for '{}'", file_path))?
            .to_string();
        self.ensure_started(file_path, root_dir).await?;
        let client = self
            .clients
            .get(&server_name)
            .ok_or_else(|| anyhow::anyhow!("LSP server '{}' not running", server_name))?;
        client.hover(&uri, line, character).await
    }

    /// Get definition locations for `file_path` at the given 1-based position.
    pub async fn definition(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let uri = path_to_uri(file_path);
        let server_name = self
            .server_name_for_file(file_path)
            .ok_or_else(|| anyhow::anyhow!("No LSP server configured for '{}'", file_path))?
            .to_string();
        self.ensure_started(file_path, root_dir).await?;
        let client = self
            .clients
            .get(&server_name)
            .ok_or_else(|| anyhow::anyhow!("LSP server '{}' not running", server_name))?;
        client.definition(&uri, line, character).await
    }

    /// Get references for a symbol in `file_path` at the given 1-based position.
    pub async fn references(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let uri = path_to_uri(file_path);
        let server_name = self
            .server_name_for_file(file_path)
            .ok_or_else(|| anyhow::anyhow!("No LSP server configured for '{}'", file_path))?
            .to_string();
        self.ensure_started(file_path, root_dir).await?;
        let client = self
            .clients
            .get(&server_name)
            .ok_or_else(|| anyhow::anyhow!("LSP server '{}' not running", server_name))?;
        client.references(&uri, line, character).await
    }

    /// List document symbols for `file_path`.
    pub async fn document_symbols(
        &mut self,
        file_path: &str,
        root_dir: &Path,
    ) -> anyhow::Result<Vec<String>> {
        let uri = path_to_uri(file_path);
        let server_name = self
            .server_name_for_file(file_path)
            .ok_or_else(|| anyhow::anyhow!("No LSP server configured for '{}'", file_path))?
            .to_string();
        self.ensure_started(file_path, root_dir).await?;
        let client = self
            .clients
            .get(&server_name)
            .ok_or_else(|| anyhow::anyhow!("LSP server '{}' not running", server_name))?;
        client.document_symbols(&uri).await
    }

    /// Get cached diagnostics for `file_path` across all running servers.
    pub fn get_diagnostics_for_file(&self, file_path: &str) -> Vec<LspDiagnostic> {
        self.clients
            .values()
            .flat_map(|c| c.get_diagnostics(file_path))
            .collect()
    }

    /// Get all cached diagnostics from all running servers.
    pub fn all_diagnostics(&self) -> Vec<LspDiagnostic> {
        self.clients
            .values()
            .flat_map(|c| c.all_diagnostics())
            .collect()
    }

    /// Shut down all running servers.
    pub async fn shutdown_all(&mut self) {
        let names: Vec<String> = self.clients.keys().cloned().collect();
        for name in names {
            if let Some(mut client) = self.clients.remove(&name) {
                if let Err(e) = client.shutdown().await {
                    tracing::warn!("Error shutting down LSP server '{}': {}", name, e);
                }
            }
        }
        self.opened_files.clear();
    }

    /// Get a legacy-compatible async diagnostic query (returns cached results).
    pub async fn get_diagnostics(&self, file: &str) -> Vec<LspDiagnostic> {
        self.get_diagnostics_for_file(file)
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Global singleton
// ---------------------------------------------------------------------------

use once_cell::sync::Lazy;

static GLOBAL_LSP_MANAGER: Lazy<Arc<tokio::sync::Mutex<LspManager>>> =
    Lazy::new(|| Arc::new(tokio::sync::Mutex::new(LspManager::new())));

/// Access the global [`LspManager`] instance.
pub fn global_lsp_manager() -> Arc<tokio::sync::Mutex<LspManager>> {
    GLOBAL_LSP_MANAGER.clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(name: &str) -> LspServerConfig {
        LspServerConfig {
            name: name.to_string(),
            command: name.to_string(),
            args: vec![],
            file_patterns: vec!["*.rs".to_string()],
            initialization_options: None,
            extension_to_language: {
                let mut m = HashMap::new();
                m.insert(".rs".to_string(), "rust".to_string());
                m
            },
            env: HashMap::new(),
            root_markers: vec![],
            disabled: false,
            settings: None,
            is_linter: false,
            language_id: None,
            warmup_timeout_ms: None,
            request_timeout_ms: None,
            capabilities: LspServerCapabilities::default(),
            workspace_ready_timings: None,
        }
    }

    fn make_diagnostic(
        file: &str,
        line: u32,
        col: u32,
        severity: DiagnosticSeverity,
        message: &str,
    ) -> LspDiagnostic {
        LspDiagnostic {
            file: file.to_string(),
            line,
            column: col,
            severity,
            message: message.to_string(),
            source: None,
            code: None,
        }
    }

    #[test]
    fn test_new_manager_empty() {
        let mgr = LspManager::new();
        assert!(mgr.servers().is_empty());
    }

    #[test]
    fn test_register_server() {
        let mut mgr = LspManager::new();
        mgr.register_server(make_config("rust-analyzer"));
        assert_eq!(mgr.servers().len(), 1);
        assert_eq!(mgr.servers()[0].name, "rust-analyzer");
    }

    #[test]
    fn test_register_multiple_servers() {
        let mut mgr = LspManager::new();
        mgr.register_server(make_config("rust-analyzer"));
        mgr.register_server(make_config("pyright"));
        assert_eq!(mgr.servers().len(), 2);
    }

    #[test]
    fn registering_a_name_twice_replaces_the_first() {
        // Two entries of one name would both match the file, and which one
        // won would depend on their order.
        let mut mgr = LspManager::new();
        mgr.register_server(make_config("rust-analyzer"));
        let mut second = make_config("rust-analyzer");
        second.command = "rust-analyzer-nightly".to_string();
        mgr.register_server(second);
        assert_eq!(mgr.servers().len(), 1);
        assert_eq!(mgr.servers()[0].command, "rust-analyzer-nightly");
    }

    #[test]
    fn a_disabled_server_never_answers() {
        let mut mgr = LspManager::new();
        let mut cfg = make_config("rust-analyzer");
        cfg.disabled = true;
        mgr.register_server(cfg);
        assert!(mgr.primary_server_for_file("src/main.rs").is_none());
        // It stays listed, because the user has to see what they switched off.
        assert_eq!(mgr.servers().len(), 1);
    }

    #[test]
    fn a_linter_never_answers_navigation() {
        let mut mgr = LspManager::new();
        let mut linter = make_config("clippy-ls");
        linter.is_linter = true;
        mgr.register_server(linter);
        mgr.register_server(make_config("rust-analyzer"));

        assert_eq!(
            mgr.primary_server_for_file("src/main.rs").map(|c| &c.name),
            Some(&"rust-analyzer".to_string()),
        );
        // Diagnostics still reach both, and the linter sorts last.
        let names: Vec<&str> = mgr
            .servers_for_file("src/main.rs")
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["rust-analyzer", "clippy-ls"]);
    }

    #[test]
    fn a_server_matches_a_file_name_without_an_extension() {
        let mut mgr = LspManager::new();
        let mut cfg = make_config("dockerls");
        cfg.file_patterns = vec!["Dockerfile".to_string()];
        cfg.extension_to_language.clear();
        mgr.register_server(cfg);
        assert!(mgr.primary_server_for_file("build/Dockerfile").is_some());
        assert!(mgr.primary_server_for_file("build/main.rs").is_none());
    }

    #[test]
    fn an_explicit_language_id_wins_over_the_table() {
        let mut cfg = make_config("ls");
        cfg.language_id = Some("rustnext".to_string());
        assert_eq!(cfg.language_for_file("src/main.rs"), "rustnext");
    }

    #[test]
    fn the_built_in_table_answers_an_unmapped_extension() {
        // Without the table a server that serves one language had to spell out
        // every extension, and a missing entry sent "plaintext", which some
        // servers refuse.
        let mut cfg = make_config("gopls");
        cfg.extension_to_language.clear();
        cfg.file_patterns = vec!["*.go".to_string()];
        assert_eq!(cfg.language_for_file("cmd/main.go"), "go");
        assert_eq!(cfg.language_for_file("notes.unknown"), "plaintext");
    }

    #[test]
    fn the_timeouts_fall_back_to_the_defaults() {
        let mut cfg = make_config("ls");
        assert_eq!(
            cfg.warmup_timeout().as_millis() as u64,
            DEFAULT_WARMUP_TIMEOUT_MS
        );
        assert_eq!(
            cfg.request_timeout().as_millis() as u64,
            DEFAULT_REQUEST_TIMEOUT_MS
        );
        cfg.warmup_timeout_ms = Some(1234);
        assert_eq!(cfg.warmup_timeout().as_millis() as u64, 1234);
    }

    #[test]
    fn an_old_config_still_parses() {
        // Every field added later carries `serde(default)`, so a settings file
        // written before them keeps working.
        let json = r#"{
            "name": "rust-analyzer",
            "command": "rust-analyzer",
            "args": [],
            "file_patterns": ["*.rs"],
            "initialization_options": null
        }"#;
        let cfg: LspServerConfig = serde_json::from_str(json).expect("parse");
        assert!(!cfg.disabled);
        assert!(!cfg.is_linter);
        assert!(cfg.root_markers.is_empty());
        assert!(cfg.settings.is_none());
    }

    #[test]
    fn an_unset_field_is_not_written_back() {
        // `save()` rewrites the whole settings file, so a default that
        // serialized would bury the user's file in noise.
        let cfg = make_config("ls");
        let json = serde_json::to_string(&cfg).expect("serialize");
        assert!(!json.contains("disabled"), "json = {json}");
        assert!(!json.contains("is_linter"), "json = {json}");
        assert!(!json.contains("capabilities"), "json = {json}");
    }

    #[test]
    fn test_server_by_name_found() {
        let mut mgr = LspManager::new();
        mgr.register_server(make_config("rust-analyzer"));
        mgr.register_server(make_config("pyright"));
        let s = mgr.server_by_name("pyright");
        assert!(s.is_some());
        assert_eq!(s.unwrap().name, "pyright");
    }

    #[test]
    fn test_server_by_name_not_found() {
        let mgr = LspManager::new();
        assert!(mgr.server_by_name("missing").is_none());
    }

    #[tokio::test]
    async fn test_get_diagnostics_empty_when_no_servers() {
        let mgr = LspManager::new();
        let diags = mgr.get_diagnostics("src/main.rs").await;
        assert!(diags.is_empty());
    }

    #[test]
    fn test_format_diagnostics_empty() {
        let result = LspManager::format_diagnostics(&[]);
        assert_eq!(result, "No diagnostics.");
    }

    #[test]
    fn test_format_diagnostics_single_error() {
        let diags = vec![make_diagnostic(
            "src/lib.rs",
            10,
            5,
            DiagnosticSeverity::Error,
            "type mismatch",
        )];
        let result = LspManager::format_diagnostics(&diags);
        assert!(result.contains("[ERROR]"));
        assert!(result.contains("src/lib.rs"));
        assert!(result.contains("10:5"));
        assert!(result.contains("type mismatch"));
    }

    #[test]
    fn test_format_diagnostics_multiple() {
        let diags = vec![
            make_diagnostic("a.rs", 1, 1, DiagnosticSeverity::Error, "err1"),
            make_diagnostic("b.rs", 2, 3, DiagnosticSeverity::Warning, "warn1"),
        ];
        let result = LspManager::format_diagnostics(&diags);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("[ERROR]"));
        assert!(lines[1].contains("[WARNING]"));
    }

    #[test]
    fn test_format_diagnostics_with_source_and_code() {
        let mut d = make_diagnostic(
            "main.rs",
            5,
            1,
            DiagnosticSeverity::Error,
            "mismatched types",
        );
        d.source = Some("rust-analyzer".to_string());
        d.code = Some("E0308".to_string());
        let result = LspManager::format_diagnostics(&[d]);
        assert!(result.contains("(rust-analyzer)"), "result = {}", result);
        assert!(result.contains("[E0308]"), "result = {}", result);
    }

    #[test]
    fn test_diagnostic_severity_ordering() {
        assert!(DiagnosticSeverity::Error < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Information);
        assert!(DiagnosticSeverity::Information < DiagnosticSeverity::Hint);
    }

    #[test]
    fn test_diagnostic_severity_as_str() {
        assert_eq!(DiagnosticSeverity::Error.as_str(), "error");
        assert_eq!(DiagnosticSeverity::Warning.as_str(), "warning");
        assert_eq!(DiagnosticSeverity::Information.as_str(), "info");
        assert_eq!(DiagnosticSeverity::Hint.as_str(), "hint");
    }

    #[test]
    fn test_lsp_server_config_serialization() {
        let cfg = make_config("rust-analyzer");
        let json = serde_json::to_string(&cfg).unwrap();
        let back: LspServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "rust-analyzer");
    }

    #[test]
    fn test_default_trait() {
        let mgr = LspManager::default();
        assert!(mgr.servers().is_empty());
    }

    #[test]
    fn test_extension_routing() {
        let mut mgr = LspManager::new();
        mgr.register_server(make_config("rust-analyzer"));
        // .rs maps to rust-analyzer
        assert_eq!(
            mgr.server_name_for_file("src/main.rs"),
            Some("rust-analyzer")
        );
        // .py has no mapping
        assert_eq!(mgr.server_name_for_file("app.py"), None);
    }

    #[test]
    fn test_path_to_uri_roundtrip() {
        // The round trip used to lose the leading slash: `file:///a/b` was
        // stripped of `file:///`, which left the relative path `a/b`, so every
        // diagnostic and every location named a file that does not exist.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "").expect("write");

        let uri = path_to_uri(&file.to_string_lossy());
        assert!(
            uri.starts_with("file://"),
            "expected file:// URI, got {uri}"
        );
        let back = uri_to_path(&uri);
        assert_eq!(
            std::fs::canonicalize(&back).expect("the path must exist"),
            std::fs::canonicalize(&file).expect("canonical"),
        );
    }

    #[test]
    fn a_uri_carries_a_space_in_a_file_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("my notes.md");
        std::fs::write(&file, "").expect("write");

        let uri = path_to_uri(&file.to_string_lossy());
        assert!(uri.contains("%20"), "the space was not escaped: {uri}");
        assert_eq!(
            std::fs::canonicalize(uri_to_path(&uri)).expect("the path must exist"),
            std::fs::canonicalize(&file).expect("canonical"),
        );
    }

    #[test]
    fn a_location_link_is_read_too() {
        // The client asks for link support in its handshake, so a server may
        // answer with links. A reader that only knows `uri` returns nothing,
        // which reads as "no definition found".
        let link = json!([{
            "targetUri": "file:///tmp/a.rs",
            "targetRange": {
                "start": { "line": 9, "character": 0 },
                "end": { "line": 12, "character": 1 }
            },
            "targetSelectionRange": {
                "start": { "line": 9, "character": 3 },
                "end": { "line": 9, "character": 7 }
            }
        }]);
        let locations = parse_locations(&link);
        assert_eq!(locations.len(), 1);
        // The selection range wins: it points at the name, not at the whole
        // declaration.
        assert_eq!(locations[0].line, 10);
        assert_eq!(locations[0].column, 4);
        assert_eq!(locations[0].file, "/tmp/a.rs");
    }

    #[test]
    fn a_plain_location_still_works() {
        let plain = json!({
            "uri": "file:///tmp/a.rs",
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 4 }
            }
        });
        let locations = parse_locations(&plain);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].to_string(), "/tmp/a.rs:1:1");
    }

    #[test]
    fn a_uri_is_passed_back_unchanged() {
        // A caller reading a URI out of a server response has to be able to
        // hand it straight back.
        assert_eq!(path_to_uri("file:///tmp/a.rs"), "file:///tmp/a.rs");
    }

    #[test]
    fn test_language_for_file() {
        let cfg = make_config("rust-analyzer");
        assert_eq!(cfg.language_for_file("src/main.rs"), "rust");
        // The extension map does not carry `.md`, so the built-in table
        // answers. Only an extension neither knows falls back to plaintext.
        assert_eq!(cfg.language_for_file("README.md"), "markdown");
        assert_eq!(cfg.language_for_file("data.qqq"), "plaintext");
    }

    #[test]
    fn test_severity_from_lsp_int() {
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(1),
            DiagnosticSeverity::Error
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(2),
            DiagnosticSeverity::Warning
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(3),
            DiagnosticSeverity::Information
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(4),
            DiagnosticSeverity::Hint
        );
        assert_eq!(
            DiagnosticSeverity::from_lsp_int(99),
            DiagnosticSeverity::Hint
        );
    }

    #[test]
    fn test_global_lsp_manager_consistent() {
        let m1 = global_lsp_manager();
        let m2 = global_lsp_manager();
        assert!(Arc::ptr_eq(&m1, &m2));
    }

    #[test]
    fn test_parse_diagnostic_valid() {
        let raw = serde_json::json!({
            "range": {
                "start": { "line": 4, "character": 2 },
                "end":   { "line": 4, "character": 10 }
            },
            "severity": 1,
            "message": "type mismatch",
            "source": "rust-analyzer",
            "code": "E0308"
        });
        let d = parse_diagnostic(&raw, "src/main.rs", "rust-analyzer").unwrap();
        assert_eq!(d.line, 5); // 0-based → 1-based
        assert_eq!(d.column, 3);
        assert_eq!(d.message, "type mismatch");
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        assert_eq!(d.code.as_deref(), Some("E0308"));
    }

    #[test]
    fn test_parse_diagnostic_missing_range_returns_none() {
        let raw = serde_json::json!({ "message": "oops" });
        assert!(parse_diagnostic(&raw, "f.rs", "lsp").is_none());
    }

    // ---- Discovery -------------------------------------------------------

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create dir");
        }
        std::fs::write(path, "").expect("write");
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }

    #[test]
    fn a_project_local_binary_wins_over_the_path() {
        // A project pins its tooling. Reaching for the global copy would run a
        // different version, or nothing at all.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("package.json"));
        let local = root.join("node_modules/.bin/typescript-language-server");
        touch(&local);
        #[cfg(unix)]
        make_executable(&local);

        let found = resolve_command("typescript-language-server", root).expect("resolved");
        assert_eq!(found, local);
    }

    #[test]
    fn a_local_directory_without_its_marker_is_not_searched() {
        // `node_modules/.bin` inside a directory that is not a Node project is
        // a leftover, not this project's tooling.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let local = root.join("node_modules/.bin/some-unlikely-server-name");
        touch(&local);

        assert!(resolve_command("some-unlikely-server-name", root).is_none());
    }

    #[test]
    fn a_path_is_taken_as_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let server = root.join("tools/my-ls");
        touch(&server);

        assert_eq!(
            resolve_command("tools/my-ls", root).expect("resolved"),
            server
        );
        assert!(resolve_command("tools/missing-ls", root).is_none());
    }

    #[test]
    fn a_missing_binary_is_reported_rather_than_guessed() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(resolve_command("a-server-nobody-installed", dir.path()).is_none());
    }

    #[test]
    fn a_root_marker_is_matched_in_the_directory_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("Cargo.toml"));

        assert!(has_root_markers(root, &["Cargo.toml".to_string()]));
        assert!(!has_root_markers(root, &["go.mod".to_string()]));
    }

    #[test]
    fn a_wildcard_marker_matches_one_level_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("project.cabal"));
        touch(&root.join("nested/deep.cabal"));

        assert!(has_root_markers(root, &["*.cabal".to_string()]));
        // The nested copy alone must not count, or every parent of a project
        // would look like a project.
        assert!(!has_root_markers(
            &root.join("other"),
            &["*.cabal".to_string()]
        ));
    }

    #[test]
    fn no_marker_means_no_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!has_root_markers(dir.path(), &[]));
    }

    // ---- The bundled catalogue -------------------------------------------

    #[test]
    fn the_catalogue_parses() {
        // A malformed catalogue would silently leave every project without a
        // server, because the parse failure is swallowed to keep sessions
        // starting.
        let servers = builtin_servers();
        assert!(servers.len() > 40, "only {} servers parsed", servers.len());
        assert!(servers.iter().any(|s| s.name == "rust-analyzer"));
    }

    #[test]
    fn every_catalogue_entry_can_be_routed() {
        for server in builtin_servers() {
            assert!(!server.command.is_empty(), "{} has no command", server.name);
            assert!(
                !server.file_patterns.is_empty(),
                "{} matches no file",
                server.name
            );
            assert!(
                !server.root_markers.is_empty(),
                "{} would never be detected",
                server.name
            );
            assert!(!server.disabled, "{} ships switched off", server.name);
        }
    }

    #[test]
    fn the_catalogue_names_each_server_once() {
        let mut names: Vec<&str> = builtin_servers().iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(count, names.len(), "the catalogue repeats a name");
    }

    #[test]
    fn detection_needs_the_marker_and_the_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // Neither: nothing is detected.
        assert!(detect_servers(root).is_empty());

        // The marker alone is not enough, because the server would fail to
        // start and the failure would surface as a broken tool rather than a
        // missing one.
        touch(&root.join("Cargo.toml"));
        let detected = detect_servers(root);
        let has_rust_analyzer = detected.iter().any(|s| s.name == "rust-analyzer");
        assert_eq!(
            has_rust_analyzer,
            resolve_command("rust-analyzer", root).is_some(),
            "detection disagreed with whether the binary resolves"
        );
    }

    #[test]
    fn a_local_binary_makes_a_catalogue_server_detectable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("package.json"));
        let local = root.join("node_modules/.bin/typescript-language-server");
        touch(&local);
        #[cfg(unix)]
        make_executable(&local);

        let detected = detect_servers(root);
        let server = detected
            .iter()
            .find(|s| s.name == "typescript-language-server")
            .expect("detected");
        assert!(server.handles_file("src/app.ts"));
    }

    #[test]
    fn a_user_entry_replaces_the_catalogue_entry() {
        // Both carry the name `rust-analyzer`, and the user's has to win.
        let mut mgr = LspManager::new();
        let mut catalogue = make_config("rust-analyzer");
        catalogue.command = "rust-analyzer".to_string();
        mgr.register_server(catalogue);

        let mut mine = make_config("rust-analyzer");
        mine.command = "/opt/ra/rust-analyzer".to_string();
        mgr.seed_from_config(&[mine]);

        assert_eq!(mgr.servers().len(), 1);
        assert_eq!(mgr.servers()[0].command, "/opt/ra/rust-analyzer");
    }

    // ---- Applying edits ---------------------------------------------------

    fn edit(
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        new_text: &str,
    ) -> TextEdit {
        TextEdit {
            start_line,
            start_character,
            end_line,
            end_character,
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn edits_are_applied_from_the_end_backwards() {
        // Applying the first edit first would move every later position, so
        // the second edit would land in the wrong place.
        let content = "one two\nthree four\n";
        let edits = vec![
            edit(0, 0, 0, 3, "ONE"),
            edit(1, 6, 1, 10, "FOUR"),
            edit(0, 4, 0, 7, "TWO"),
        ];
        let result = apply_text_edits(content, &edits).expect("applied");
        assert_eq!(result, "ONE TWO\nthree FOUR\n");
    }

    #[test]
    fn an_edit_counts_characters_the_way_the_protocol_does() {
        // A character offset is a UTF-16 code unit offset, so a non-ASCII
        // line would be cut mid-character by a byte offset.
        let content = "let café = 1;\n";
        let result = apply_text_edits(content, &[edit(0, 4, 0, 8, "tea")]).expect("applied");
        assert_eq!(result, "let tea = 1;\n");
    }

    #[test]
    fn two_overlapping_edits_are_refused() {
        // They describe two different results for the same characters. Picking
        // one silently would corrupt the file in a way nothing reports.
        let content = "hello world\n";
        let edits = vec![edit(0, 0, 0, 5, "a"), edit(0, 3, 0, 8, "b")];
        let error = apply_text_edits(content, &edits).expect_err("should refuse");
        assert!(error.to_string().contains("overlap"), "error = {error}");
    }

    #[test]
    fn an_edit_past_the_end_is_refused() {
        let error = apply_text_edits("one\n", &[edit(9, 0, 9, 1, "x")]).expect_err("should refuse");
        assert!(
            error.to_string().contains("past the end"),
            "error = {error}"
        );
    }

    #[test]
    fn both_workspace_edit_shapes_are_read() {
        let by_changes = json!({
            "changes": {
                "file:///tmp/a.rs": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "newText": "X"
                }]
            }
        });
        let by_document_changes = json!({
            "documentChanges": [{
                "textDocument": { "uri": "file:///tmp/a.rs", "version": 1 },
                "edits": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "newText": "X"
                }]
            }]
        });
        assert_eq!(workspace_edit_files(&by_changes).len(), 1);
        assert_eq!(workspace_edit_files(&by_document_changes).len(), 1);
    }

    #[test]
    fn a_resource_operation_is_reported_rather_than_performed() {
        // Creating, renaming or deleting a file is not something a rename
        // request should do behind the caller's back.
        let edit = json!({
            "documentChanges": [
                { "kind": "delete", "uri": "file:///tmp/gone.rs" },
                { "kind": "rename", "oldUri": "file:///tmp/a.rs", "newUri": "file:///tmp/b.rs" }
            ]
        });
        assert!(workspace_edit_files(&edit).is_empty());
        let operations = workspace_edit_resource_operations(&edit);
        assert_eq!(operations.len(), 2);
        assert!(operations[0].starts_with("delete "), "{operations:?}");
    }

    #[test]
    fn a_workspace_edit_writes_every_file_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one two\n").expect("write");
        let uri = path_to_uri(&file.to_string_lossy());

        let edit = json!({
            "changes": {
                uri: [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 3 }
                        },
                        "newText": "ONE"
                    },
                    {
                        "range": {
                            "start": { "line": 0, "character": 4 },
                            "end": { "line": 0, "character": 7 }
                        },
                        "newText": "TWO"
                    }
                ]
            }
        });

        let applied = apply_workspace_edit(&edit).expect("applied");
        assert_eq!(applied.len(), 1);
        assert!(applied[0].contains("2 edit(s)"), "{applied:?}");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "ONE TWO\n");
    }

    #[test]
    fn a_failing_workspace_edit_writes_nothing() {
        // Everything is computed before anything is written, so a bad edit on
        // the second file does not leave the first one changed.
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.txt");
        std::fs::write(&good, "keep\n").expect("write");
        let missing = dir.path().join("missing.txt");

        let edit = json!({
            "changes": {
                path_to_uri(&good.to_string_lossy()): [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 4 }
                    },
                    "newText": "changed"
                }],
                path_to_uri(&missing.to_string_lossy()): [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "newText": "x"
                }]
            }
        });

        assert!(apply_workspace_edit(&edit).is_err());
        assert_eq!(std::fs::read_to_string(&good).expect("read"), "keep\n");
    }

    // ---- A fake server, so the protocol itself can be tested -------------

    /// One side of an in-memory connection to a scripted server.
    struct FakeServer {
        reader: BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
        writer: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    }

    impl FakeServer {
        /// Read one message the client sent.
        async fn next(&mut self) -> serde_json::Value {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                read_message(&mut self.reader),
            )
            .await
            .expect("the client sent nothing")
            .expect("unreadable message")
        }

        async fn send(&mut self, message: serde_json::Value) {
            let body = serde_json::to_string(&message).expect("encode");
            send_message(&mut self.writer, &body).await.expect("send");
        }

        async fn respond(&mut self, id: &serde_json::Value, result: serde_json::Value) {
            self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                .await;
        }

        /// Answer the handshake and return the `initialized` notification.
        async fn accept_handshake(&mut self, capabilities: serde_json::Value) {
            let request = self.next().await;
            assert_eq!(request["method"], "initialize");
            let id = request["id"].clone();
            self.respond(&id, json!({ "capabilities": capabilities }))
                .await;
            let notification = self.next().await;
            assert_eq!(notification["method"], "initialized");
        }
    }

    /// A client and the scripted server on the other end of its pipes.
    fn connected(config: LspServerConfig) -> (LspClient, FakeServer) {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);
        let client = LspClient::connect(config, Box::new(client_read), Box::new(client_write));
        let server = FakeServer {
            reader: BufReader::new(Box::new(server_read)),
            writer: Box::new(server_write),
        };
        (client, server)
    }

    #[tokio::test]
    async fn the_handshake_records_what_the_server_supports() {
        // The initialize response used to be thrown away, so nothing could
        // tell whether a request was worth sending.
        let (mut client, mut server) = connected(make_config("ls"));
        let handshake = tokio::spawn(async move {
            server
                .accept_handshake(json!({
                    "renameProvider": true,
                    "codeActionProvider": false,
                    "workspace": { "fileOperations": { "willRename": { "filters": [] } } }
                }))
                .await;
            server
        });
        client
            .initialize("file:///tmp/project")
            .await
            .expect("handshake");
        let _server = handshake.await.expect("server task");

        assert!(client.supports("renameProvider"));
        assert!(!client.supports("codeActionProvider"));
        assert!(client.supports("workspace.fileOperations.willRename"));
        assert!(!client.supports("implementationProvider"));
    }

    #[tokio::test]
    async fn a_configuration_request_is_answered_by_section() {
        // A server that asks for its settings and is ignored waits, and some
        // refuse to finish starting.
        let mut config = make_config("ls");
        config.settings = Some(json!({ "ls": { "lint": true }, "other": 1 }));
        let (mut client, mut server) = connected(config);

        let task = tokio::spawn(async move {
            server.accept_handshake(json!({})).await;
            // `push_settings` fires right after the handshake.
            let pushed = server.next().await;
            assert_eq!(pushed["method"], "workspace/didChangeConfiguration");

            server
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": 900,
                    "method": "workspace/configuration",
                    "params": { "items": [{ "section": "ls" }, { "section": "missing" }] }
                }))
                .await;
            server.next().await
        });

        client
            .initialize("file:///tmp/project")
            .await
            .expect("handshake");
        let answer = task.await.expect("server task");

        assert_eq!(answer["id"], 900);
        assert_eq!(answer["result"][0]["lint"], true);
        assert!(answer["result"][1].is_null(), "answer = {answer}");
    }

    #[tokio::test]
    async fn an_unknown_request_is_refused_rather_than_ignored() {
        let (mut client, mut server) = connected(make_config("ls"));
        let task = tokio::spawn(async move {
            server.accept_handshake(json!({})).await;
            server
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "window/somethingNobodyImplements",
                    "params": {}
                }))
                .await;
            server.next().await
        });

        client
            .initialize("file:///tmp/project")
            .await
            .expect("handshake");
        let answer = task.await.expect("server task");

        assert_eq!(answer["id"], 7);
        assert_eq!(answer["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn a_timed_out_request_is_cancelled() {
        // Without the cancel the server keeps computing an answer nobody will
        // read, which on a large project is how a server falls behind.
        let mut config = make_config("ls");
        config.request_timeout_ms = Some(150);
        let (mut client, mut server) = connected(config);

        let task = tokio::spawn(async move {
            server.accept_handshake(json!({})).await;
            let request = server.next().await;
            assert_eq!(request["method"], "textDocument/hover");
            // Deliberately no answer.
            server.next().await
        });

        client
            .initialize("file:///tmp/project")
            .await
            .expect("handshake");
        let result = client.hover("file:///tmp/a.rs", 1, 1).await;
        let cancel = task.await.expect("server task");

        let error = result.expect_err("the request should have timed out");
        assert!(error.to_string().contains("timed out"), "error = {error}");
        assert_eq!(cancel["method"], "$/cancelRequest");
    }

    #[tokio::test]
    async fn a_server_that_stops_fails_the_waiting_request() {
        // A dropped channel used to surface as "channel closed", which says
        // nothing about a server that died on a missing library.
        let (mut client, mut server) = connected(make_config("ls"));
        let task = tokio::spawn(async move {
            server.accept_handshake(json!({})).await;
            let _request = server.next().await;
            // Drop the server side, closing the pipe.
        });

        client
            .initialize("file:///tmp/project")
            .await
            .expect("handshake");
        let result = client.hover("file:///tmp/a.rs", 1, 1).await;
        task.await.expect("server task");

        let error = result.expect_err("the request should have failed");
        assert!(error.to_string().contains("stopped"), "error = {error}");
    }

    #[tokio::test]
    async fn diagnostics_carry_a_version_that_moves() {
        let (mut client, mut server) = connected(make_config("ls"));
        let task = tokio::spawn(async move {
            server.accept_handshake(json!({})).await;
            server
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": "file:///tmp/a.rs",
                        "diagnostics": [{
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 1 }
                            },
                            "severity": 1,
                            "message": "boom"
                        }]
                    }
                }))
                .await;
            // A second publish saying the file is clean. The version has to
            // move for that too, or a caller waiting for a fresh answer never
            // learns the problem went away.
            server
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": { "uri": "file:///tmp/a.rs", "diagnostics": [] }
                }))
                .await;
            server
        });

        client
            .initialize("file:///tmp/project")
            .await
            .expect("handshake");
        let _server = task.await.expect("server task");

        // The reader task runs concurrently; wait for the second publish.
        for _ in 0..100 {
            if client.diagnostic_version("file:///tmp/a.rs") >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(client.diagnostic_version("file:///tmp/a.rs"), 2);
        assert!(
            client.all_diagnostics().is_empty(),
            "the clean publish did not land"
        );
    }

    #[tokio::test]
    async fn the_project_load_wait_ends_when_progress_ends() {
        let mut config = make_config("ls");
        config.workspace_ready_timings = Some(WorkspaceReadyTimings {
            timeout_ms: 5_000,
            poll_ms: 10,
            settle_ms: 20,
            status_request_timeout_ms: 1_000,
        });
        let (mut client, mut server) = connected(config);

        let task = tokio::spawn(async move {
            server.accept_handshake(json!({})).await;
            server
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": { "token": "indexing", "value": { "kind": "begin" } }
                }))
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            server
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "$/progress",
                    "params": { "token": "indexing", "value": { "kind": "end" } }
                }))
                .await;
            server
        });

        client
            .initialize("file:///tmp/project")
            .await
            .expect("handshake");
        // Let the begin arrive before the wait starts.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(client.is_loading_project(), "the begin never landed");

        let started = std::time::Instant::now();
        client.wait_for_project_loaded().await;
        let waited = started.elapsed();

        let _server = task.await.expect("server task");
        assert!(!client.is_loading_project());
        assert!(
            waited >= std::time::Duration::from_millis(80),
            "returned after {waited:?}, before the server finished indexing"
        );
    }

    #[test]
    fn a_directory_is_scanned_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("package.json"));
        let local = root.join("node_modules/.bin/typescript-language-server");
        touch(&local);
        #[cfg(unix)]
        make_executable(&local);

        let mut mgr = LspManager::new();
        mgr.seed_detected(root);
        let after_first = mgr.servers().len();
        assert!(after_first > 0, "nothing was detected");

        // A second scan must not add the same servers again. Registration
        // replaces by name, so the count is the observable part.
        mgr.seed_detected(root);
        assert_eq!(mgr.servers().len(), after_first);

        mgr.forget_detection();
        mgr.seed_detected(root);
        assert_eq!(mgr.servers().len(), after_first);
    }
}
