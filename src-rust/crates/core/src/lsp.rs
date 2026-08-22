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

/// The file names a language-server configuration may use, most preferred
/// first.
///
/// Both spellings of each format, because a dot-file is the convention in some
/// projects and a plain name in others, and a user who picks the wrong one
/// gets silence rather than an error.
const CONFIG_FILE_NAMES: &[&str] = &["lsp.json", ".lsp.json", "lsp.toml", ".lsp.toml"];

/// A configuration file for language servers.
///
/// The shape is the same in JSON and TOML: a map of server name to the fields
/// of [`LspServerConfig`], either at the top level or under `servers`.
#[derive(Debug, Default, Deserialize)]
struct LspConfigFile {
    #[serde(default)]
    servers: HashMap<String, serde_json::Value>,
    /// Stop a server after this long without a request.
    #[serde(default)]
    idle_timeout_ms: Option<u64>,
    /// Everything else at the top level, read as a server when `servers` is
    /// absent. The flat form is what a one-server file usually looks like.
    #[serde(flatten)]
    flat: HashMap<String, serde_json::Value>,
}

/// What one configuration file contributes.
#[derive(Debug, Default)]
pub struct LspFileConfig {
    /// Per-server overrides, by name. Each is a partial object: only the
    /// fields the file names.
    pub overrides: HashMap<String, serde_json::Value>,
    pub idle_timeout_ms: Option<u64>,
}

/// Read one configuration file.
///
/// A file that cannot be read or parsed contributes nothing and says so in the
/// log. Refusing to start over a stray comma in an optional file would be
/// worse than running without it.
fn read_lsp_config_file(path: &Path) -> Option<LspFileConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: Result<LspConfigFile, String> =
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            toml::from_str(&text).map_err(|e| e.to_string())
        } else {
            serde_json::from_str(&text).map_err(|e| e.to_string())
        };

    match parsed {
        Ok(file) => {
            let mut overrides = file.servers;
            if overrides.is_empty() {
                // The flat form. `idle_timeout_ms` is captured by its own
                // field, so whatever is left is a server.
                overrides = file
                    .flat
                    .into_iter()
                    .filter(|(_, value)| value.is_object())
                    .collect();
            }
            Some(LspFileConfig {
                overrides,
                idle_timeout_ms: file.idle_timeout_ms,
            })
        }
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", path.display());
            None
        }
    }
}

/// Where a language-server configuration file may live, least preferred first.
///
/// The order matches the settings files: the machine's own configuration
/// first, then the project's, so a repository can adjust what the user set
/// without replacing it.
pub fn lsp_config_paths(cwd: &Path) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home);
    }
    roots.push(crate::mikmik_home());
    roots.push(cwd.join(".mikmik"));
    roots.push(cwd.to_path_buf());

    roots
        .into_iter()
        .flat_map(|root| CONFIG_FILE_NAMES.iter().map(move |name| root.join(name)))
        .collect()
}

/// Read every configuration file that applies to `cwd`, in precedence order.
///
/// Merging is per field, not per server: a file that sets one argument for
/// `rust-analyzer` keeps everything else the catalogue and the lower-precedence
/// files said. That is only possible on the JSON level, before the values
/// become typed, which is why the overrides are carried as raw objects.
pub fn load_lsp_config_files(cwd: &Path) -> LspFileConfig {
    let mut merged = LspFileConfig::default();
    for path in lsp_config_paths(cwd) {
        let Some(file) = read_lsp_config_file(&path) else {
            continue;
        };
        tracing::debug!("read language server config {}", path.display());
        if let Some(idle) = file.idle_timeout_ms {
            merged.idle_timeout_ms = Some(idle);
        }
        for (name, value) in file.overrides {
            match merged.overrides.get_mut(&name) {
                Some(existing) => merge_json_objects(existing, value),
                None => {
                    merged.overrides.insert(name, value);
                }
            }
        }
    }
    merged
}

/// Copy the fields of `patch` over `base`, one level deep.
///
/// Shallow on purpose: `settings` and `initialization_options` are whole
/// documents a server reads as a unit, and merging them field by field would
/// leave a server holding half of one configuration and half of another.
fn merge_json_objects(base: &mut serde_json::Value, patch: serde_json::Value) {
    let (Some(base), Some(patch)) = (base.as_object_mut(), patch.as_object()) else {
        *base = patch;
        return;
    };
    for (key, value) in patch {
        base.insert(key.clone(), value.clone());
    }
}

/// Apply the overrides from the configuration files to a server list.
///
/// A name that matches an existing server patches it; a name that does not is
/// a new server, which needs enough fields to be usable and is otherwise
/// reported and dropped.
pub fn apply_config_overrides(
    servers: &mut Vec<LspServerConfig>,
    overrides: &HashMap<String, serde_json::Value>,
) {
    for (name, patch) in overrides {
        match servers.iter_mut().find(|s| s.name == *name) {
            Some(existing) => {
                let Ok(mut value) = serde_json::to_value(&*existing) else {
                    continue;
                };
                merge_json_objects(&mut value, patch.clone());
                match serde_json::from_value::<LspServerConfig>(value) {
                    Ok(updated) => *existing = updated,
                    Err(e) => tracing::warn!("ignoring the override for '{name}': {e}"),
                }
            }
            None => {
                let mut value = patch.clone();
                if let Some(object) = value.as_object_mut() {
                    object
                        .entry("name")
                        .or_insert_with(|| serde_json::Value::String(name.clone()));
                }
                match serde_json::from_value::<LspServerConfig>(value) {
                    Ok(server) => servers.push(server),
                    Err(e) => tracing::warn!(
                        "ignoring '{name}': a new server needs a command and file patterns ({e})"
                    ),
                }
            }
        }
    }
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

    /// The type of the symbol at a position.
    pub async fn type_definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let result = self
            .send_request_inner(
                "textDocument/typeDefinition",
                position_params(uri, line, character),
            )
            .await?;
        Ok(extract_locations(&result))
    }

    /// What implements the interface or trait at a position.
    pub async fn implementation(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let result = self
            .send_request_inner(
                "textDocument/implementation",
                position_params(uri, line, character),
            )
            .await?;
        Ok(extract_locations(&result))
    }

    /// Symbols matching `query` anywhere in the workspace.
    pub async fn workspace_symbols(&self, query: &str) -> anyhow::Result<Vec<WorkspaceSymbol>> {
        let result = self
            .send_request_inner("workspace/symbol", json!({ "query": query }))
            .await?;
        Ok(parse_workspace_symbols(&result))
    }

    /// The edits that renaming the symbol at a position would make.
    ///
    /// The edit is returned rather than applied, because whether to apply it is
    /// the caller's decision.
    pub async fn rename(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let mut params = position_params(uri, line, character);
        params["newName"] = json!(new_name);
        self.send_request_inner("textDocument/rename", params).await
    }

    /// The code actions offered at a position.
    ///
    /// `only` filters by kind, server-side. The diagnostics the server already
    /// published for the file are passed as context, because most quick fixes
    /// are offered for a specific diagnostic and a server given none offers
    /// only the refactorings.
    pub async fn code_actions(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        only: Option<&str>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let diagnostics = self
            .shared
            .diagnostics
            .get(uri)
            .map(|entry| {
                entry
                    .value()
                    .iter()
                    .map(|d| {
                        json!({
                            "range": {
                                "start": {
                                    "line": d.line.saturating_sub(1),
                                    "character": d.column.saturating_sub(1)
                                },
                                "end": {
                                    "line": d.line.saturating_sub(1),
                                    "character": d.column.saturating_sub(1)
                                }
                            },
                            "severity": d.severity as u8,
                            "message": d.message,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut context = json!({ "diagnostics": diagnostics });
        if let Some(kind) = only.filter(|k| !k.is_empty()) {
            context["only"] = json!([kind]);
        }

        let zero_based_line = line.saturating_sub(1);
        let zero_based_character = character.saturating_sub(1);
        let result = self
            .send_request_inner(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": zero_based_line, "character": zero_based_character },
                        "end": { "line": zero_based_line, "character": zero_based_character }
                    },
                    "context": context,
                }),
            )
            .await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    /// Ask the server to fill in the parts of a code action it left out.
    ///
    /// A server is allowed to send a title and compute the edit only if the
    /// action is chosen, which is why applying one means asking again.
    pub async fn resolve_code_action(
        &self,
        action: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.send_request_inner("codeAction/resolve", action.clone())
            .await
    }

    /// Run a command the server offers.
    pub async fn execute_command(
        &self,
        command: &str,
        arguments: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.send_request_inner(
            "workspace/executeCommand",
            json!({ "command": command, "arguments": arguments }),
        )
        .await
    }

    /// Ask the server what edits a rename of these files would need.
    pub async fn will_rename_files(
        &self,
        pairs: &[(String, String)],
    ) -> anyhow::Result<serde_json::Value> {
        let files: Vec<serde_json::Value> = pairs
            .iter()
            .map(|(old, new)| json!({ "oldUri": old, "newUri": new }))
            .collect();
        self.send_request_inner("workspace/willRenameFiles", json!({ "files": files }))
            .await
    }

    /// Tell the server the files have been renamed.
    pub async fn did_rename_files(&self, pairs: &[(String, String)]) -> anyhow::Result<()> {
        let files: Vec<serde_json::Value> = pairs
            .iter()
            .map(|(old, new)| json!({ "oldUri": old, "newUri": new }))
            .collect();
        self.send_notification_inner("workspace/didRenameFiles", json!({ "files": files }))
            .await
    }

    /// Ask the server for a file's diagnostics, rather than waiting to be told.
    ///
    /// The newer half of the protocol: a server that advertises
    /// `diagnosticProvider` answers on request and may never publish at all,
    /// so waiting for a notification from one of those waits forever.
    pub async fn pull_diagnostics(
        &self,
        uri: &str,
        file_path: &str,
    ) -> anyhow::Result<Vec<LspDiagnostic>> {
        let result = self
            .send_request_inner(
                "textDocument/diagnostic",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await?;
        let items = result
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|d| parse_diagnostic(d, file_path, &self.server_name))
            .collect())
    }

    /// Whether this server answers a diagnostics request.
    pub fn supports_pull_diagnostics(&self) -> bool {
        self.supports("diagnosticProvider")
    }

    /// The edits that formatting the whole document would make.
    pub async fn format_document(
        &self,
        uri: &str,
        options: &FormatOptions,
    ) -> anyhow::Result<Vec<TextEdit>> {
        let result = self
            .send_request_inner(
                "textDocument/formatting",
                json!({
                    "textDocument": { "uri": uri },
                    "options": {
                        "tabSize": options.tab_size,
                        "insertSpaces": options.insert_spaces,
                        "trimTrailingWhitespace": true,
                        "insertFinalNewline": true,
                    }
                }),
            )
            .await?;
        Ok(result
            .as_array()
            .map(|edits| edits.iter().filter_map(TextEdit::from_json).collect())
            .unwrap_or_default())
    }

    /// Send a request this client has no method for.
    ///
    /// The escape hatch for a server-specific request, and for reaching a part
    /// of the protocol nothing here wraps yet.
    pub async fn raw_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.send_request_inner(method, params).await
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

// ---------------------------------------------------------------------------
// Whole-project diagnostics
// ---------------------------------------------------------------------------

/// A build or type check that reports the problems of a whole project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCheck {
    /// What is being checked, for the caller to show.
    pub description: String,
    pub command: String,
    pub args: Vec<String>,
}

/// The checks that suit the project in `cwd`.
///
/// A language server reports the file it was asked about, and usually only the
/// files that are open. A change that breaks a different file is invisible
/// until something opens it, which is what a project-wide check is for.
///
/// Detection is by marker file and does not look at what is installed: a
/// missing tool is reported when it fails to start, which says more than
/// silently checking nothing.
pub fn detect_project_checks(cwd: &Path) -> Vec<ProjectCheck> {
    let mut checks = Vec::new();
    let has = |name: &str| cwd.join(name).exists();

    if has("Cargo.toml") {
        checks.push(ProjectCheck {
            description: "Rust (cargo check)".to_string(),
            command: "cargo".to_string(),
            args: vec![
                "check".to_string(),
                "--message-format=short".to_string(),
                "--all-targets".to_string(),
            ],
        });
    }
    if has("tsconfig.json") {
        checks.push(ProjectCheck {
            description: "TypeScript (tsc --noEmit)".to_string(),
            command: "npx".to_string(),
            args: vec![
                "--no-install".to_string(),
                "tsc".to_string(),
                "--noEmit".to_string(),
            ],
        });
    }
    if has("go.work") {
        checks.push(ProjectCheck {
            description: "Go workspace (go build)".to_string(),
            command: "go".to_string(),
            args: vec!["build".to_string(), "./...".to_string()],
        });
    } else if has("go.mod") {
        checks.push(ProjectCheck {
            description: "Go module (go build)".to_string(),
            command: "go".to_string(),
            args: vec!["build".to_string(), "./...".to_string()],
        });
    }
    if has("pyrightconfig.json") || has("pyproject.toml") {
        checks.push(ProjectCheck {
            description: "Python (pyright)".to_string(),
            command: "pyright".to_string(),
            args: vec!["--outputjson".to_string()],
        });
    }
    checks
}

/// How long a project-wide check may run.
const PROJECT_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How many output lines a project-wide check reports.
const PROJECT_CHECK_OUTPUT_LINES: usize = 50;

/// Run one project-wide check and return what it said.
///
/// Both streams are captured, because a compiler writes its diagnostics to
/// standard error and its progress to standard output, and which one carries
/// the answer differs per tool.
pub async fn run_project_check(check: &ProjectCheck, cwd: &Path) -> anyhow::Result<String> {
    let mut cmd = Command::new(&check.command);
    cmd.args(&check.args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::process_tree::spawn_in_own_group(&mut cmd);

    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run `{}`: {e}", check.command))?;
    let pid = child.id();

    let output = match tokio::time::timeout(PROJECT_CHECK_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => {
            // The check owns a process tree: a build spawns compilers.
            if let Some(pid) = pid {
                crate::process_tree::kill_tree(pid);
            }
            return Err(anyhow::anyhow!(
                "`{}` did not finish within {}s",
                check.command,
                PROJECT_CHECK_TIMEOUT.as_secs()
            ));
        }
    };

    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&output.stdout).into_owned();
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Ok(if output.status.success() {
            "no problems".to_string()
        } else {
            format!("failed with {} and said nothing", output.status)
        });
    }
    let shown = lines.len().min(PROJECT_CHECK_OUTPUT_LINES);
    let mut report = lines[..shown].join("\n");
    if lines.len() > shown {
        report.push_str(&format!("\n... and {} more lines", lines.len() - shown));
    }
    Ok(report)
}

/// What a formatting request tells the server about the file's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOptions {
    pub tab_size: u32,
    pub insert_spaces: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            tab_size: 4,
            insert_spaces: true,
        }
    }
}

/// Work out how a file is indented.
///
/// A formatting request has to say what the file uses, and a wrong answer
/// reformats the whole file on the first save. The file itself is the only
/// reliable source: an editor setting says what the user prefers, not what
/// this file already does.
pub fn detect_indent(content: &str) -> FormatOptions {
    let mut tabs = 0usize;
    let mut space_widths: Vec<u32> = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let spaces = line.chars().take_while(|c| *c == ' ').count() as u32;
        if spaces > 0 {
            space_widths.push(spaces);
        }
    }

    if tabs > space_widths.len() {
        return FormatOptions {
            tab_size: 4,
            insert_spaces: false,
        };
    }
    if space_widths.is_empty() {
        return FormatOptions::default();
    }

    // The smallest step between indent levels, which is the unit. Taking the
    // smallest indent instead would read a continuation line as the unit.
    space_widths.sort_unstable();
    space_widths.dedup();
    let mut smallest_step = space_widths[0];
    for pair in space_widths.windows(2) {
        smallest_step = smallest_step.min(pair[1] - pair[0]);
    }
    FormatOptions {
        tab_size: smallest_step.clamp(1, 8),
        insert_spaces: true,
    }
}

/// How many files one rename may move.
///
/// A directory rename that walks a build output would otherwise send a request
/// naming a hundred thousand files, which no server survives.
pub const MAX_RENAME_PAIRS: usize = 1_000;

/// Every file a move touches, as (old path, new path).
///
/// A file gives one pair. A directory gives one pair per file inside it,
/// because the servers are told about files, not directories.
fn enumerate_rename_pairs(
    from: &Path,
    to: &Path,
) -> anyhow::Result<Vec<(std::path::PathBuf, std::path::PathBuf)>> {
    if from.is_file() {
        return Ok(vec![(from.to_path_buf(), to.to_path_buf())]);
    }

    let mut pairs = Vec::new();
    let mut stack = vec![from.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path.strip_prefix(from).unwrap_or(&path);
            pairs.push((path.clone(), to.join(relative)));
            if pairs.len() > MAX_RENAME_PAIRS {
                return Err(anyhow::anyhow!(
                    "'{}' holds more than {MAX_RENAME_PAIRS} files; move it in smaller pieces",
                    from.display()
                ));
            }
        }
    }
    Ok(pairs)
}

/// Find the column of `symbol` on `line` of `file_path`.
///
/// Counting columns by hand is the commonest way a position request lands on
/// the wrong token and answers nothing, so a caller may name the symbol
/// instead. `name#2` selects the second occurrence on that line; without the
/// suffix the first is used.
///
/// With no symbol the first non-whitespace column is used, which is right for
/// a line whose only interesting token is at its start and wrong otherwise, so
/// callers that need precision ask for one.
///
/// `line` and the result are both 1-based.
pub fn resolve_symbol_column(
    file_path: &str,
    line: u32,
    symbol: Option<&str>,
) -> anyhow::Result<u32> {
    let content = std::fs::read_to_string(file_path)
        .map_err(|e| anyhow::anyhow!("cannot read '{file_path}': {e}"))?;
    let text = content
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "'{file_path}' has {} lines, so line {line} does not exist",
                content.lines().count()
            )
        })?;

    let Some(spec) = symbol.filter(|s| !s.is_empty()) else {
        let column = text
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map(|(index, _)| utf16_column(text, index))
            .unwrap_or(0);
        return Ok(column + 1);
    };

    let (name, occurrence) = match spec.rsplit_once('#') {
        Some((name, count)) => {
            let nth: usize = count.parse().map_err(|_| {
                anyhow::anyhow!("'{spec}' does not name an occurrence; write `name#2`")
            })?;
            if nth == 0 {
                return Err(anyhow::anyhow!("occurrences are counted from 1"));
            }
            (name, nth)
        }
        None => (spec, 1),
    };

    // Exact first, then case-insensitively: a caller who typed the name in the
    // wrong case meant the symbol that is there.
    let found = nth_match(text, name, occurrence).or_else(|| {
        let lowered = text.to_lowercase();
        nth_match(&lowered, &name.to_lowercase(), occurrence)
    });

    match found {
        Some(index) => Ok(utf16_column(text, index) + 1),
        None => Err(anyhow::anyhow!(
            "'{name}' does not appear on line {line} of '{file_path}'{}",
            if occurrence > 1 {
                format!(" {occurrence} times")
            } else {
                String::new()
            }
        )),
    }
}

/// The byte index of the `nth` occurrence of `needle` in `haystack`.
fn nth_match(haystack: &str, needle: &str, nth: usize) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack.match_indices(needle).nth(nth - 1).map(|(i, _)| i)
}

/// The UTF-16 column of a byte index, which is what the protocol counts.
fn utf16_column(text: &str, byte_index: usize) -> u32 {
    text[..byte_index]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// The lines around a location, for showing a result in context.
///
/// A bare `file:line:column` makes the reader open the file to learn anything.
/// Each line is prefixed with its number.
pub fn read_location_context(file_path: &str, line: u32, around: u32) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(file_path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let target = line.saturating_sub(1) as usize;
    let first = target.saturating_sub(around as usize);
    let last = (target + around as usize).min(lines.len().saturating_sub(1));
    if first > last || lines.is_empty() {
        return Vec::new();
    }
    (first..=last)
        .map(|index| format!("{:>6} | {}", index + 1, lines[index]))
        .collect()
}

/// The parameters every position-based request shares.
///
/// The protocol counts from zero and every caller here counts from one, so the
/// conversion lives in one place.
fn position_params(uri: &str, line: u32, character: u32) -> serde_json::Value {
    json!({
        "textDocument": { "uri": uri },
        "position": {
            "line": line.saturating_sub(1),
            "character": character.saturating_sub(1),
        }
    })
}

/// A symbol found anywhere in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: String,
    /// The class or module the symbol belongs to, when the server says.
    pub container: Option<String>,
    pub location: LspLocation,
}

/// Read a `workspace/symbol` answer.
///
/// A server may answer with `SymbolInformation`, which carries `location`, or
/// with the newer `WorkspaceSymbol`, whose location may be only a URI while
/// the range is filled in on request. The URI-only form is read as line one.
pub fn parse_workspace_symbols(result: &serde_json::Value) -> Vec<WorkspaceSymbol> {
    result
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?.to_string();
                    let kind =
                        symbol_kind_name(item.get("kind").and_then(|k| k.as_u64()).unwrap_or(0))
                            .to_string();
                    let location = item.get("location")?;
                    let uri = location.get("uri")?.as_str()?;
                    Some(WorkspaceSymbol {
                        name,
                        kind,
                        container: item
                            .get("containerName")
                            .and_then(|c| c.as_str())
                            .filter(|c| !c.is_empty())
                            .map(|c| c.to_string()),
                        location: LspLocation {
                            file: uri_to_path(uri),
                            line: location
                                .pointer("/range/start/line")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32
                                + 1,
                            column: location
                                .pointer("/range/start/character")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32
                                + 1,
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

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
/// Render one symbol and everything under it.
///
/// A server answers with one of two shapes. A `DocumentSymbol` nests its
/// children and carries a `range`; a `SymbolInformation` is flat and carries a
/// `location`. Both are read, and the line number comes from whichever one is
/// present: a symbol list without line numbers makes the reader search the
/// file for every entry.
fn collect_symbol(sym: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let name = sym
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("<unnamed>");
    let kind = sym.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
    let kind_str = symbol_kind_name(kind);
    let line = sym
        .pointer("/selectionRange/start/line")
        .or_else(|| sym.pointer("/range/start/line"))
        .or_else(|| sym.pointer("/location/range/start/line"))
        .and_then(|v| v.as_u64())
        .map(|line| format!(":{}", line + 1))
        .unwrap_or_default();
    let deprecated = sym
        .get("deprecated")
        .and_then(|d| d.as_bool())
        .unwrap_or(false)
        || sym
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|tags| tags.iter().any(|tag| tag.as_u64() == Some(1)))
            .unwrap_or(false);
    let mark = if deprecated { " [deprecated]" } else { "" };
    out.push(format!("{indent}{name} ({kind_str}){line}{mark}"));

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

/// What a diagnostic says, without where it says it.
///
/// A line inserted above a problem moves it without changing it, and reporting
/// the same message again because its line number moved is noise.
fn diagnostic_identity(diagnostic: &LspDiagnostic) -> String {
    format!("{}\u{1}{}", diagnostic.file, diagnostic.message)
}

/// Remembers which problems have already been reported, per file.
///
/// A model that is told about the same error after every edit spends its
/// attention re-reading it. Only what is new is worth an interruption.
#[derive(Debug, Default)]
pub struct DiagnosticsLedger {
    seen: HashMap<String, std::collections::HashSet<String>>,
}

impl DiagnosticsLedger {
    /// Keep only the diagnostics this file has not reported before.
    ///
    /// The record is replaced rather than added to, so a problem that goes
    /// away and comes back is reported again, which is right: it is news the
    /// second time too.
    pub fn only_new(&mut self, file: &str, diagnostics: Vec<LspDiagnostic>) -> Vec<LspDiagnostic> {
        let identities: std::collections::HashSet<String> =
            diagnostics.iter().map(diagnostic_identity).collect();
        let previous = self.seen.insert(file.to_string(), identities);
        match previous {
            Some(previous) => diagnostics
                .into_iter()
                .filter(|d| !previous.contains(&diagnostic_identity(d)))
                .collect(),
            None => diagnostics,
        }
    }

    /// Forget a file, so its next report starts fresh.
    pub fn forget(&mut self, file: &str) {
        self.seen.remove(file);
    }
}

/// Drop repeats and put the worst first.
///
/// Two servers watching one file report the same compiler error twice, and
/// showing it twice wastes the reader's attention and the model's context.
/// Sorting by severity puts the errors above the hints, because a file with
/// both is usually broken for the first reason.
fn dedupe_and_sort(diagnostics: &mut Vec<LspDiagnostic>) {
    let mut seen: std::collections::HashSet<(String, u32, u32, String)> =
        std::collections::HashSet::new();
    diagnostics.retain(|d| seen.insert((d.file.clone(), d.line, d.column, d.message.clone())));
    diagnostics.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then(a.file.cmp(&b.file))
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });
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

        // One file: the path on every line is noise, because the reader
        // already knows which file they asked about. Several files: the path
        // is the only thing telling them apart, so each file gets a heading
        // and the lines under it are positions.
        let mut files: Vec<&str> = diagnostics.iter().map(|d| d.file.as_str()).collect();
        files.sort_unstable();
        files.dedup();

        let describe = |d: &LspDiagnostic, with_file: bool| {
            let where_ = if with_file {
                format!("{}:{}:{}", d.file, d.line, d.column)
            } else {
                format!("{}:{}", d.line, d.column)
            };
            format!(
                "[{}] {} - {}{}{}",
                d.severity.as_str().to_uppercase(),
                where_,
                d.message,
                d.source
                    .as_deref()
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default(),
                d.code
                    .as_deref()
                    .map(|c| format!(" [{c}]"))
                    .unwrap_or_default(),
            )
        };

        if files.len() == 1 {
            let mut lines = vec![format!("{}:", files[0])];
            lines.extend(diagnostics.iter().map(|d| describe(d, false)));
            return lines.join("\n");
        }

        let mut lines = Vec::new();
        for file in files {
            lines.push(format!("{file}:"));
            lines.extend(
                diagnostics
                    .iter()
                    .filter(|d| d.file == file)
                    .map(|d| describe(d, false)),
            );
        }
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// LspManager — registry and multi-server coordination
// ---------------------------------------------------------------------------

/// Manages a collection of [`LspClient`] instances, routing file operations
/// to the correct server based on extension mappings.
/// How long a server that failed to start is left alone.
pub const INIT_FAILURE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(180);

/// How many times an empty reference list is asked for again.
pub const REFERENCES_RETRY_COUNT: usize = 2;
/// How long to wait between those attempts.
pub const REFERENCES_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// One running server: its name and the workspace root it was started for.
///
/// The root is part of the key because a server is initialized for one root
/// and answers against it. Keyed by name alone, a second directory would reuse
/// a client that indexed the first one.
type ClientKey = (String, std::path::PathBuf);

pub struct LspManager {
    /// Registered configs (used for lookup before a client is started)
    configs: Vec<LspServerConfig>,
    /// Running clients.
    clients: HashMap<ClientKey, LspClient>,
    /// When each client was last asked for something.
    last_used: HashMap<ClientKey, std::time::Instant>,
    /// When each client last failed to start.
    init_failures: HashMap<ClientKey, std::time::Instant>,
    /// Directories already scanned for catalogue servers.
    detected_roots: std::collections::HashSet<std::path::PathBuf>,
    /// Directories whose `lsp.json` / `lsp.toml` have been read.
    configured_roots: std::collections::HashSet<std::path::PathBuf>,
    /// The idle timeout a configuration file asked for.
    file_idle_timeout: Option<std::time::Duration>,
    /// Shut a server down after this long without a request. `None` keeps
    /// every server until the session ends.
    idle_timeout: Option<std::time::Duration>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            configs: Vec::new(),
            clients: HashMap::new(),
            last_used: HashMap::new(),
            init_failures: HashMap::new(),
            detected_roots: std::collections::HashSet::new(),
            configured_roots: std::collections::HashSet::new(),
            file_idle_timeout: None,
            idle_timeout: None,
        }
    }

    /// Shut a server down after this long without a request.
    ///
    /// A language server holds the whole project in memory, and a session that
    /// touched one Rust file keeps paying for it. Off by default, because
    /// stopping a server means the next request pays for indexing again.
    pub fn set_idle_timeout(&mut self, timeout: Option<std::time::Duration>) {
        self.idle_timeout = timeout;
    }

    /// Stop every server that has been idle past the timeout.
    pub async fn sweep_idle(&mut self) {
        let Some(timeout) = self.idle_timeout else {
            return;
        };
        let stale: Vec<ClientKey> = self
            .last_used
            .iter()
            .filter(|(_, last)| last.elapsed() >= timeout)
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(mut client) = self.clients.remove(&key) {
                tracing::debug!("stopping idle language server '{}'", key.0);
                let _ = client.shutdown().await;
            }
            self.last_used.remove(&key);
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
    /// Start and initialize `server_name` for `root_dir`, or return the
    /// running one.
    ///
    /// A server that failed to start is not retried for
    /// [`INIT_FAILURE_BACKOFF`]. Without that, every request pays the same
    /// startup timeout again, which turns one missing binary into a session
    /// where each call is slow.
    pub async fn ensure_client(
        &mut self,
        server_name: &str,
        root_dir: &Path,
    ) -> anyhow::Result<&mut LspClient> {
        let key = (server_name.to_string(), root_dir.to_path_buf());

        if !self.clients.contains_key(&key) {
            if let Some(failed_at) = self.init_failures.get(&key) {
                if failed_at.elapsed() < INIT_FAILURE_BACKOFF {
                    return Err(anyhow::anyhow!(
                        "language server '{server_name}' failed to start recently; \
                         not retried for another {}s",
                        (INIT_FAILURE_BACKOFF - failed_at.elapsed()).as_secs()
                    ));
                }
                self.init_failures.remove(&key);
            }

            let config = self
                .configs
                .iter()
                .find(|c| c.name == server_name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no language server named '{server_name}'"))?;
            if config.disabled {
                return Err(anyhow::anyhow!(
                    "language server '{server_name}' is switched off"
                ));
            }

            let started = match LspClient::start_in(config, root_dir).await {
                Ok(mut client) => {
                    let root_uri = path_to_uri(&root_dir.to_string_lossy());
                    match client.initialize(&root_uri).await {
                        Ok(()) => Ok(client),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };

            match started {
                Ok(client) => {
                    tracing::info!("language server '{server_name}' started");
                    self.clients.insert(key.clone(), client);
                }
                Err(e) => {
                    self.init_failures.insert(key, std::time::Instant::now());
                    return Err(anyhow::anyhow!(
                        "could not start language server '{server_name}': {e}"
                    ));
                }
            }
        }

        self.last_used
            .insert(key.clone(), std::time::Instant::now());
        self.clients
            .get_mut(&key)
            .ok_or_else(|| anyhow::anyhow!("language server '{server_name}' is not running"))
    }

    /// Forget that `server_name` failed to start, so the next call retries.
    pub fn clear_failure(&mut self, server_name: &str, root_dir: &Path) {
        self.init_failures
            .remove(&(server_name.to_string(), root_dir.to_path_buf()));
    }

    /// Spawn and initialize servers for all registered configurations.
    pub async fn start_servers(&mut self, root_dir: &Path) {
        let names: Vec<String> = self
            .configs
            .iter()
            .filter(|c| !c.disabled)
            .map(|c| c.name.clone())
            .collect();
        for name in names {
            if let Err(e) = self.ensure_client(&name, root_dir).await {
                tracing::warn!("{e}");
            }
        }
    }

    /// Send the current content of `file_path` to every server that handles it.
    ///
    /// The server answers against the copy it holds, so a file opened once and
    /// edited afterwards would be answered from the text as it was when it was
    /// opened. Reading from disk each time is what keeps the two in step.
    pub async fn sync_file(&mut self, file_path: &str, root_dir: &Path) -> anyhow::Result<()> {
        let uri = path_to_uri(file_path);
        let names: Vec<String> = self
            .servers_for_file(file_path)
            .iter()
            .map(|c| c.name.clone())
            .collect();
        if names.is_empty() {
            return Ok(());
        }

        let content = tokio::fs::read_to_string(file_path).await.map_err(|e| {
            anyhow::anyhow!("cannot read '{file_path}' for the language server: {e}")
        })?;

        let mut last_error = None;
        for name in names {
            let client = match self.ensure_client(&name, root_dir).await {
                Ok(client) => client,
                Err(e) => {
                    // One server failing must not hide the answer another one
                    // would have given.
                    tracing::debug!("{e}");
                    last_error = Some(e);
                    continue;
                }
            };
            let language = client.server_config.language_for_file(file_path);
            if let Err(e) = client.sync_document(&uri, &language, &content).await {
                tracing::debug!("could not send '{file_path}' to {name}: {e}");
                last_error = Some(e);
            }
        }

        // Only report a failure when nothing at all is running for the file.
        if self.running_for_file(file_path, root_dir).is_empty() {
            if let Some(e) = last_error {
                return Err(e);
            }
        }
        Ok(())
    }

    /// The servers currently running for `file_path` under `root_dir`.
    fn running_for_file(&self, file_path: &str, root_dir: &Path) -> Vec<String> {
        self.servers_for_file(file_path)
            .iter()
            .map(|c| c.name.clone())
            .filter(|name| {
                self.clients
                    .contains_key(&(name.clone(), root_dir.to_path_buf()))
            })
            .collect()
    }

    /// Open a file on the appropriate LSP server.
    ///
    /// Kept as the name callers already use; it sends the current content, so
    /// calling it again after an edit updates the server rather than doing
    /// nothing.
    pub async fn open_file(&mut self, file_path: &str, root_dir: &Path) -> anyhow::Result<()> {
        self.sync_file(file_path, root_dir).await
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

    /// Read `lsp.json` / `lsp.toml` for `cwd` and apply what they say.
    ///
    /// Runs once per directory. Returns the idle timeout a file asked for, so
    /// the caller can apply it alongside the one from the settings file.
    ///
    /// Call this **after** [`Self::seed_detected`] and **before**
    /// [`Self::seed_from_config`]: a file overrides the catalogue, and
    /// `settings.json` overrides the file.
    pub fn apply_file_config(&mut self, cwd: &Path) -> Option<std::time::Duration> {
        if !self.configured_roots.insert(cwd.to_path_buf()) {
            return self.file_idle_timeout;
        }
        let file_config = load_lsp_config_files(cwd);
        apply_config_overrides(&mut self.configs, &file_config.overrides);
        self.file_idle_timeout = file_config
            .idle_timeout_ms
            .filter(|ms| *ms > 0)
            .map(std::time::Duration::from_millis);
        self.file_idle_timeout
    }

    /// Forget which directories were scanned, so the next call re-detects.
    ///
    /// A project gains a marker or the user installs a server mid-session, and
    /// nothing else would notice.
    pub fn forget_detection(&mut self) {
        self.detected_roots.clear();
        self.configured_roots.clear();
    }

    /// The server that answers navigation for `file_path`, started and ready.
    ///
    /// "Ready" is the part that matters: a project-aware server answers with
    /// nothing while it is still indexing, and nothing reads as "not found".
    async fn navigation_client(
        &mut self,
        file_path: &str,
        root_dir: &Path,
    ) -> anyhow::Result<&mut LspClient> {
        let server_name = self
            .primary_server_for_file(file_path)
            .map(|c| c.name.clone())
            .ok_or_else(|| anyhow::anyhow!("no language server handles '{file_path}'"))?;
        let client = self.ensure_client(&server_name, root_dir).await?;
        client.wait_for_project_loaded().await;
        Ok(client)
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
        let client = self.navigation_client(file_path, root_dir).await?;
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
        let client = self.navigation_client(file_path, root_dir).await?;
        client.definition(&uri, line, character).await
    }

    /// Get references for a symbol in `file_path` at the given 1-based position.
    ///
    /// An answer holding nothing but the declaration that was asked about is
    /// usually a server that has not finished indexing rather than a symbol
    /// nothing uses, so it is asked again a couple of times.
    pub async fn references(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let uri = path_to_uri(file_path);
        let here = format!("{file_path}:{line}:{character}");
        let client = self.navigation_client(file_path, root_dir).await?;

        let mut answer = client.references(&uri, line, character).await?;
        for _ in 0..REFERENCES_RETRY_COUNT {
            let only_the_declaration = answer.len() == 1 && answer[0] == here;
            if !(answer.is_empty() || only_the_declaration) {
                break;
            }
            tokio::time::sleep(REFERENCES_RETRY_DELAY).await;
            client.wait_for_project_loaded().await;
            answer = client.references(&uri, line, character).await?;
        }
        Ok(answer)
    }

    /// List document symbols for `file_path`.
    pub async fn document_symbols(
        &mut self,
        file_path: &str,
        root_dir: &Path,
    ) -> anyhow::Result<Vec<String>> {
        let uri = path_to_uri(file_path);
        let client = self.navigation_client(file_path, root_dir).await?;
        client.document_symbols(&uri).await
    }

    /// Where the type of the symbol at a position is declared.
    pub async fn type_definition(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let uri = path_to_uri(file_path);
        let client = self.navigation_client(file_path, root_dir).await?;
        client.type_definition(&uri, line, character).await
    }

    /// What implements the interface or trait at a position.
    pub async fn implementation(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Vec<String>> {
        let uri = path_to_uri(file_path);
        let client = self.navigation_client(file_path, root_dir).await?;
        client.implementation(&uri, line, character).await
    }

    /// Symbols matching `query` across the workspace.
    ///
    /// Every non-linter server is asked, because a workspace holds more than
    /// one language and the caller does not name a file here. Repeats are
    /// dropped: two servers indexing one file report the same symbol.
    pub async fn workspace_symbols(
        &mut self,
        query: &str,
        root_dir: &Path,
        limit: usize,
    ) -> anyhow::Result<Vec<WorkspaceSymbol>> {
        let names: Vec<String> = self
            .configs
            .iter()
            .filter(|c| !c.disabled && !c.is_linter)
            .map(|c| c.name.clone())
            .collect();

        let mut found: Vec<WorkspaceSymbol> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for name in names {
            let client = match self.ensure_client(&name, root_dir).await {
                Ok(client) => client,
                Err(e) => {
                    errors.push(e.to_string());
                    continue;
                }
            };
            if !client.supports("workspaceSymbolProvider") {
                continue;
            }
            client.wait_for_project_loaded().await;
            match client.workspace_symbols(query).await {
                Ok(symbols) => found.extend(symbols),
                Err(e) => errors.push(e.to_string()),
            }
        }

        if found.is_empty() && !errors.is_empty() {
            return Err(anyhow::anyhow!("{}", errors.join("; ")));
        }

        found.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then(a.location.file.cmp(&b.location.file))
                .then(a.location.line.cmp(&b.location.line))
        });
        found.dedup_by(|a, b| a.name == b.name && a.location == b.location);
        found.truncate(limit);
        Ok(found)
    }

    /// The edits a rename would make, without applying them.
    pub async fn rename(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let uri = path_to_uri(file_path);
        let client = self.navigation_client(file_path, root_dir).await?;
        if !client.supports("renameProvider") {
            return Err(anyhow::anyhow!(
                "'{}' does not implement rename",
                client.server_name
            ));
        }
        client.rename(&uri, line, character, new_name).await
    }

    /// The code actions offered at a position.
    pub async fn code_actions(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        line: u32,
        character: u32,
        only: Option<&str>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let uri = path_to_uri(file_path);
        let client = self.navigation_client(file_path, root_dir).await?;
        if !client.supports("codeActionProvider") {
            return Err(anyhow::anyhow!(
                "'{}' does not implement code actions",
                client.server_name
            ));
        }
        client.code_actions(&uri, line, character, only).await
    }

    /// Apply one code action: resolve it if needed, apply its edit, run its
    /// command.
    ///
    /// An action may carry an edit, a command, both, or neither, and the
    /// specification says the edit is applied first.
    pub async fn apply_code_action(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        action: &serde_json::Value,
    ) -> anyhow::Result<Vec<String>> {
        let server_name = self
            .primary_server_for_file(file_path)
            .map(|c| c.name.clone())
            .ok_or_else(|| anyhow::anyhow!("no language server handles '{file_path}'"))?;
        let client = self.ensure_client(&server_name, root_dir).await?;

        // A server is allowed to send a title now and the edit only when the
        // action is chosen. A failure here is not fatal: the action may have
        // arrived complete.
        let resolved = if action.get("edit").is_none() && action.get("data").is_some() {
            client
                .resolve_code_action(action)
                .await
                .unwrap_or_else(|_| action.clone())
        } else {
            action.clone()
        };

        let mut report = Vec::new();
        if let Some(edit) = resolved.get("edit") {
            report.extend(apply_workspace_edit(edit)?);
            for operation in workspace_edit_resource_operations(edit) {
                report.push(format!("not performed: {operation}"));
            }
        }
        if let Some(command) = resolved.get("command") {
            // An action's `command` may be the command object itself or a
            // nested one.
            let (name, arguments) = match command.get("command") {
                Some(inner) => (
                    inner.as_str().unwrap_or_default(),
                    command
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ),
                None => (
                    command.as_str().unwrap_or_default(),
                    resolved
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ),
            };
            if !name.is_empty() {
                client.execute_command(name, &arguments).await?;
                report.push(format!("ran {name}"));
            }
        }
        Ok(report)
    }

    /// Format `file_path` with its language server and write the result.
    ///
    /// Returns whether anything changed. The file is read, sent, formatted and
    /// written here rather than by the caller, because the edits address the
    /// text the server holds and any gap between the two would corrupt it.
    pub async fn format_file(&mut self, file_path: &str, root_dir: &Path) -> anyhow::Result<bool> {
        let uri = path_to_uri(file_path);
        let content = tokio::fs::read_to_string(file_path)
            .await
            .map_err(|e| anyhow::anyhow!("cannot read '{file_path}': {e}"))?;
        let options = detect_indent(&content);

        self.sync_file(file_path, root_dir).await?;
        let server_name = self
            .primary_server_for_file(file_path)
            .map(|c| c.name.clone())
            .ok_or_else(|| anyhow::anyhow!("no language server handles '{file_path}'"))?;
        let client = self.ensure_client(&server_name, root_dir).await?;
        if !client.supports("documentFormattingProvider") {
            return Ok(false);
        }

        let edits = client.format_document(&uri, &options).await?;
        if edits.is_empty() {
            return Ok(false);
        }
        let formatted = apply_text_edits(&content, &edits)?;
        if formatted == content {
            return Ok(false);
        }
        tokio::fs::write(file_path, &formatted)
            .await
            .map_err(|e| anyhow::anyhow!("cannot write '{file_path}': {e}"))?;
        // The server's copy is now behind the file it just formatted.
        self.sync_file(file_path, root_dir).await?;
        Ok(true)
    }

    /// Move a file or a directory, and let every server update the references.
    ///
    /// A rename on disk alone leaves every import of the old path broken. The
    /// servers are asked first for the edits the move needs, then the move
    /// happens, then they are told it happened. With `apply` false only the
    /// edits are reported.
    pub async fn rename_file(
        &mut self,
        from: &Path,
        to: &Path,
        root_dir: &Path,
        apply: bool,
    ) -> anyhow::Result<Vec<String>> {
        if from == to {
            return Err(anyhow::anyhow!(
                "the source and the destination are the same"
            ));
        }
        if !from.exists() {
            return Err(anyhow::anyhow!("'{}' does not exist", from.display()));
        }
        if to.exists() {
            return Err(anyhow::anyhow!("'{}' already exists", to.display()));
        }

        let pairs = enumerate_rename_pairs(from, to)?;
        if pairs.is_empty() {
            return Err(anyhow::anyhow!("'{}' holds no files", from.display()));
        }

        let uri_pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(old, new)| {
                (
                    path_to_uri(&old.to_string_lossy()),
                    // The destination does not exist yet, so its URI is built
                    // from the path rather than canonicalised.
                    format!(
                        "file://{}",
                        percent_encode_path(&new.to_string_lossy().replace('\\', "/"))
                    ),
                )
            })
            .collect();

        let affected: Vec<String> = pairs
            .iter()
            .flat_map(|(old, new)| {
                [
                    old.to_string_lossy().into_owned(),
                    new.to_string_lossy().into_owned(),
                ]
            })
            .collect();
        let names: Vec<String> = self
            .configs
            .iter()
            .filter(|c| !c.disabled && !c.is_linter)
            .filter(|c| affected.iter().any(|path| c.handles_file(path)))
            .map(|c| c.name.clone())
            .collect();

        let mut report = Vec::new();
        let mut edits: Vec<serde_json::Value> = Vec::new();
        for name in &names {
            let client = match self.ensure_client(name, root_dir).await {
                Ok(client) => client,
                Err(e) => {
                    report.push(format!("{name}: {e}"));
                    continue;
                }
            };
            if !client.supports("workspace.fileOperations.willRename") {
                continue;
            }
            client.wait_for_project_loaded().await;
            match client.will_rename_files(&uri_pairs).await {
                Ok(edit) if !edit.is_null() => edits.push(edit),
                Ok(_) => {}
                // A server that does not implement it says so, and that is not
                // a reason to abandon the rename.
                Err(e) => report.push(format!("{name}: {e}")),
            }
        }

        if !apply {
            for edit in &edits {
                for (uri, file_edits) in workspace_edit_files(edit) {
                    report.push(format!(
                        "would change {}: {} edit(s)",
                        uri_to_path(&uri),
                        file_edits.len()
                    ));
                }
            }
            report.push(format!("would move {} → {}", from.display(), to.display()));
            return Ok(report);
        }

        for edit in &edits {
            report.extend(apply_workspace_edit(edit)?);
        }

        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::rename(from, to).map_err(|e| {
            anyhow::anyhow!("cannot move {} to {}: {e}", from.display(), to.display())
        })?;
        report.push(format!("moved {} → {}", from.display(), to.display()));

        // The documents are gone from their old paths, so the servers must
        // forget them before they are told about the move.
        for (old_uri, _) in &uri_pairs {
            for name in &names {
                if let Some(client) = self
                    .clients
                    .get_mut(&(name.clone(), root_dir.to_path_buf()))
                {
                    if client.has_open(old_uri) {
                        let _ = client.close_document(old_uri).await;
                    }
                }
            }
        }
        for name in &names {
            if let Some(client) = self.clients.get(&(name.clone(), root_dir.to_path_buf())) {
                let _ = client.did_rename_files(&uri_pairs).await;
            }
        }

        Ok(report)
    }

    /// Send a request this manager has no method for.
    pub async fn raw_request(
        &mut self,
        server_name: &str,
        root_dir: &Path,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let client = self.ensure_client(server_name, root_dir).await?;
        client.raw_request(method, params).await
    }

    /// Diagnostics for `file_path`, waiting for a fresh answer.
    ///
    /// Every server that handles the file is asked, linters included. The file
    /// is sent first, then the answer is waited for by version rather than by
    /// a fixed sleep: a sleep long enough for a cold server would be wasted on
    /// a warm one, and a short one reports "no problems" for a server that has
    /// not replied yet.
    ///
    /// The wait ends early once every server has published again.
    pub async fn fresh_diagnostics(
        &mut self,
        file_path: &str,
        root_dir: &Path,
        wait: std::time::Duration,
    ) -> Vec<LspDiagnostic> {
        let uri = path_to_uri(file_path);
        let names: Vec<String> = self
            .servers_for_file(file_path)
            .iter()
            .map(|c| c.name.clone())
            .collect();

        // The version each server was on before the file was sent.
        let mut before: Vec<(String, u64)> = Vec::new();
        for name in &names {
            match self.ensure_client(name, root_dir).await {
                Ok(client) => before.push((name.clone(), client.diagnostic_version(&uri))),
                Err(e) => tracing::debug!("{e}"),
            }
        }
        if before.is_empty() {
            return Vec::new();
        }

        if let Err(e) = self.sync_file(file_path, root_dir).await {
            tracing::debug!("{e}");
        }

        // A server that answers on request may never publish, so waiting for a
        // notification from one of those would wait for the whole budget and
        // then report nothing.
        let mut pulled: Vec<LspDiagnostic> = Vec::new();
        let pulling: Vec<String> = before
            .iter()
            .filter(|(name, _)| {
                self.clients
                    .get(&(name.clone(), root_dir.to_path_buf()))
                    .map(|client| client.supports_pull_diagnostics())
                    .unwrap_or(false)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in &pulling {
            if let Some(client) = self.clients.get(&(name.clone(), root_dir.to_path_buf())) {
                match client.pull_diagnostics(&uri, file_path).await {
                    Ok(diagnostics) => pulled.extend(diagnostics),
                    Err(e) => tracing::debug!("{name} could not answer a diagnostic request: {e}"),
                }
            }
        }
        before.retain(|(name, _)| !pulling.contains(name));

        let deadline = std::time::Instant::now() + wait;
        while !before.is_empty() && std::time::Instant::now() < deadline {
            let all_fresh = before.iter().all(|(name, version)| {
                self.clients
                    .get(&(name.clone(), root_dir.to_path_buf()))
                    .map(|client| client.diagnostic_version(&uri) > *version)
                    .unwrap_or(true)
            });
            if all_fresh {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        let mut collected: Vec<LspDiagnostic> = before
            .iter()
            .filter_map(|(name, _)| self.clients.get(&(name.clone(), root_dir.to_path_buf())))
            .flat_map(|client| client.get_diagnostics(file_path))
            .collect();
        collected.extend(pulled);
        dedupe_and_sort(&mut collected);
        collected
    }

    /// Get cached diagnostics for `file_path` across all running servers.
    pub fn get_diagnostics_for_file(&self, file_path: &str) -> Vec<LspDiagnostic> {
        let mut collected: Vec<LspDiagnostic> = self
            .clients
            .values()
            .flat_map(|c| c.get_diagnostics(file_path))
            .collect();
        dedupe_and_sort(&mut collected);
        collected
    }

    /// Get all cached diagnostics from all running servers.
    pub fn all_diagnostics(&self) -> Vec<LspDiagnostic> {
        let mut collected: Vec<LspDiagnostic> = self
            .clients
            .values()
            .flat_map(|c| c.all_diagnostics())
            .collect();
        dedupe_and_sort(&mut collected);
        collected
    }

    /// Reload a server: push its settings again, and restart it if that fails.
    ///
    /// A restart is the blunt instrument, so it is the fallback rather than
    /// the first move: the next request pays for indexing the project again.
    pub async fn reload_server(
        &mut self,
        server_name: &str,
        root_dir: &Path,
    ) -> anyhow::Result<String> {
        self.clear_failure(server_name, root_dir);
        let key = (server_name.to_string(), root_dir.to_path_buf());
        let Some(client) = self.clients.get_mut(&key) else {
            // Not running: the next request starts it, which is the reload.
            self.ensure_client(server_name, root_dir).await?;
            return Ok(format!("started {server_name}"));
        };

        client.push_settings().await;
        Ok(format!("reloaded {server_name}"))
    }

    /// Stop a server, so the next request starts it again.
    pub async fn restart_server(
        &mut self,
        server_name: &str,
        root_dir: &Path,
    ) -> anyhow::Result<String> {
        let key = (server_name.to_string(), root_dir.to_path_buf());
        self.init_failures.remove(&key);
        self.last_used.remove(&key);
        match self.clients.remove(&key) {
            Some(mut client) => {
                let _ = client.shutdown().await;
                Ok(format!("restarted {server_name}"))
            }
            None => Ok(format!("{server_name} was not running")),
        }
    }

    /// Every server that is running, with the root it serves.
    pub fn running_clients(&self) -> Vec<(&str, &Path, &LspClient)> {
        self.clients
            .iter()
            .map(|((name, root), client)| (name.as_str(), root.as_path(), client))
            .collect()
    }

    /// Shut down all running servers.
    pub async fn shutdown_all(&mut self) {
        let keys: Vec<ClientKey> = self.clients.keys().cloned().collect();
        for key in keys {
            if let Some(mut client) = self.clients.remove(&key) {
                if let Err(e) = client.shutdown().await {
                    tracing::warn!("Error shutting down LSP server '{}': {}", key.0, e);
                }
            }
        }
        self.last_used.clear();
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
        // Two files: each gets a heading, and the lines under it carry the
        // position only, because the path is already above them.
        let diags = vec![
            make_diagnostic("a.rs", 1, 1, DiagnosticSeverity::Error, "err1"),
            make_diagnostic("b.rs", 2, 3, DiagnosticSeverity::Warning, "warn1"),
        ];
        let result = LspManager::format_diagnostics(&diags);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(
            lines,
            vec![
                "a.rs:",
                "[ERROR] 1:1 - err1",
                "b.rs:",
                "[WARNING] 2:3 - warn1",
            ]
        );
    }

    #[test]
    fn one_file_is_named_once() {
        // Repeating the path on every line is noise: the reader asked about
        // that file.
        let diags = vec![
            make_diagnostic("a.rs", 1, 1, DiagnosticSeverity::Error, "err1"),
            make_diagnostic("a.rs", 9, 2, DiagnosticSeverity::Warning, "warn1"),
        ];
        let result = LspManager::format_diagnostics(&diags);
        assert_eq!(result.matches("a.rs").count(), 1, "result = {result}");
        assert!(result.contains("[ERROR] 1:1 - err1"), "result = {result}");
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

    // ---- Whole-project checks ---------------------------------------------

    #[test]
    fn a_project_is_recognised_by_its_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(detect_project_checks(dir.path()).is_empty());

        touch(&dir.path().join("Cargo.toml"));
        let checks = detect_project_checks(dir.path());
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].command, "cargo");

        touch(&dir.path().join("go.mod"));
        assert_eq!(detect_project_checks(dir.path()).len(), 2);
    }

    #[test]
    fn a_go_workspace_is_checked_once() {
        // `go.work` and `go.mod` side by side describe one project, and
        // building it twice would double the wait for nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        touch(&dir.path().join("go.work"));
        touch(&dir.path().join("go.mod"));
        let checks = detect_project_checks(dir.path());
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert!(checks[0].description.contains("workspace"));
    }

    #[tokio::test]
    async fn a_check_that_cannot_start_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let check = ProjectCheck {
            description: "nothing".to_string(),
            command: "a-build-tool-nobody-installed".to_string(),
            args: vec![],
        };
        let error = error_of(run_project_check(&check, dir.path()).await);
        assert!(error.contains("cannot run"), "{error}");
    }

    #[tokio::test]
    async fn a_silent_successful_check_reports_no_problems() {
        let dir = tempfile::tempdir().expect("tempdir");
        let check = ProjectCheck {
            description: "true".to_string(),
            command: "true".to_string(),
            args: vec![],
        };
        let output = run_project_check(&check, dir.path()).await.expect("ran");
        assert_eq!(output, "no problems");
    }

    // ---- Configuration files ----------------------------------------------

    #[test]
    fn a_config_file_patches_one_field_and_keeps_the_rest() {
        // Overriding one argument must not mean copying the whole entry.
        let mut servers = vec![make_config("rust-analyzer")];
        servers[0].args = vec!["--old".to_string()];
        servers[0].root_markers = vec!["Cargo.toml".to_string()];

        let mut overrides = HashMap::new();
        overrides.insert("rust-analyzer".to_string(), json!({ "args": ["--new"] }));
        apply_config_overrides(&mut servers, &overrides);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].args, vec!["--new".to_string()]);
        assert_eq!(servers[0].root_markers, vec!["Cargo.toml".to_string()]);
        assert_eq!(servers[0].command, "rust-analyzer");
    }

    #[test]
    fn a_config_file_can_switch_a_server_off() {
        let mut servers = vec![make_config("eslint")];
        let mut overrides = HashMap::new();
        overrides.insert("eslint".to_string(), json!({ "disabled": true }));
        apply_config_overrides(&mut servers, &overrides);
        assert!(servers[0].disabled);
    }

    #[test]
    fn a_config_file_can_add_a_server() {
        let mut servers = vec![];
        let mut overrides = HashMap::new();
        overrides.insert(
            "my-ls".to_string(),
            json!({
                "command": "my-ls",
                "args": ["--stdio"],
                "file_patterns": ["*.xyz"],
                "root_markers": [".xyz-project"]
            }),
        );
        apply_config_overrides(&mut servers, &overrides);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "my-ls");
        assert!(servers[0].handles_file("a.xyz"));
    }

    #[test]
    fn an_incomplete_new_server_is_dropped_rather_than_half_registered() {
        let mut servers = vec![];
        let mut overrides = HashMap::new();
        overrides.insert("broken".to_string(), json!({ "args": ["--stdio"] }));
        apply_config_overrides(&mut servers, &overrides);
        assert!(servers.is_empty());
    }

    #[test]
    fn both_file_formats_are_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("lsp.json"),
            r#"{ "servers": { "rust-analyzer": { "args": ["--from-json"] } } }"#,
        )
        .expect("write");
        let json_config = read_lsp_config_file(&dir.path().join("lsp.json")).expect("parsed");
        assert_eq!(
            json_config.overrides["rust-analyzer"]["args"][0],
            "--from-json"
        );

        std::fs::write(
            dir.path().join("lsp.toml"),
            "[servers.rust-analyzer]\nargs = [\"--from-toml\"]\n",
        )
        .expect("write");
        let toml_config = read_lsp_config_file(&dir.path().join("lsp.toml")).expect("parsed");
        assert_eq!(
            toml_config.overrides["rust-analyzer"]["args"][0],
            "--from-toml"
        );
    }

    #[test]
    fn the_flat_form_is_read_as_servers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lsp.json");
        std::fs::write(
            &path,
            r#"{ "idle_timeout_ms": 300000, "gopls": { "args": ["serve", "-rpc.trace"] } }"#,
        )
        .expect("write");

        let config = read_lsp_config_file(&path).expect("parsed");
        assert_eq!(config.idle_timeout_ms, Some(300_000));
        assert_eq!(config.overrides.len(), 1, "{:?}", config.overrides);
        assert!(config.overrides.contains_key("gopls"));
    }

    #[test]
    fn a_broken_config_file_is_ignored_rather_than_fatal() {
        // Refusing to start over a stray comma in an optional file would be
        // worse than running without it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lsp.json");
        std::fs::write(&path, "{ not json").expect("write");
        assert!(read_lsp_config_file(&path).is_none());
    }

    #[test]
    fn the_project_file_wins_over_the_home_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = lsp_config_paths(dir.path());
        let project_index = paths
            .iter()
            .position(|p| p == &dir.path().join("lsp.json"))
            .expect("the project root is searched");
        let dot_mikmik_index = paths
            .iter()
            .position(|p| p == &dir.path().join(".mikmik").join("lsp.json"))
            .expect(".mikmik is searched");
        assert!(
            project_index > dot_mikmik_index,
            "the project root must be read last so it wins"
        );
    }

    // ---- Reporting a write ------------------------------------------------

    #[test]
    fn only_a_new_problem_is_reported() {
        // Repeating the same error after every edit spends the reader's
        // attention on something they already know.
        let mut ledger = DiagnosticsLedger::default();
        let first = vec![
            make_diagnostic("a.rs", 3, 1, DiagnosticSeverity::Error, "missing semicolon"),
            make_diagnostic("a.rs", 9, 1, DiagnosticSeverity::Warning, "unused import"),
        ];
        assert_eq!(ledger.only_new("a.rs", first.clone()).len(), 2);

        // The same two again: nothing new.
        assert!(ledger.only_new("a.rs", first.clone()).is_empty());

        // A third problem is news, the two known ones are not.
        let mut second = first.clone();
        second.push(make_diagnostic(
            "a.rs",
            12,
            1,
            DiagnosticSeverity::Error,
            "type mismatch",
        ));
        let fresh = ledger.only_new("a.rs", second);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].message, "type mismatch");
    }

    #[test]
    fn a_moved_problem_is_not_reported_again() {
        // Inserting a line above an error moves it without changing it.
        let mut ledger = DiagnosticsLedger::default();
        let before = vec![make_diagnostic(
            "a.rs",
            3,
            1,
            DiagnosticSeverity::Error,
            "missing semicolon",
        )];
        assert_eq!(ledger.only_new("a.rs", before).len(), 1);

        let after = vec![make_diagnostic(
            "a.rs",
            4,
            1,
            DiagnosticSeverity::Error,
            "missing semicolon",
        )];
        assert!(
            ledger.only_new("a.rs", after).is_empty(),
            "the same problem was reported again because its line moved"
        );
    }

    #[test]
    fn a_problem_that_returns_is_reported_again() {
        let mut ledger = DiagnosticsLedger::default();
        let problem = vec![make_diagnostic(
            "a.rs",
            3,
            1,
            DiagnosticSeverity::Error,
            "missing semicolon",
        )];
        assert_eq!(ledger.only_new("a.rs", problem.clone()).len(), 1);
        // Fixed.
        assert!(ledger.only_new("a.rs", vec![]).is_empty());
        // Broken again: news the second time too.
        assert_eq!(ledger.only_new("a.rs", problem).len(), 1);
    }

    #[test]
    fn the_indent_of_a_file_is_read_from_the_file() {
        // A wrong answer reformats the whole file on the first save.
        assert_eq!(
            detect_indent("fn main() {\n  let a = 1;\n  if a {\n    let b = 2;\n  }\n}\n"),
            FormatOptions {
                tab_size: 2,
                insert_spaces: true
            }
        );
        assert_eq!(
            detect_indent("fn main() {\n\tlet a = 1;\n\tif a {\n\t\tlet b = 2;\n\t}\n}\n"),
            FormatOptions {
                tab_size: 4,
                insert_spaces: false
            }
        );
        // Nothing to go on: the default rather than a guess.
        assert_eq!(detect_indent("one\ntwo\n"), FormatOptions::default());
    }

    // ---- Symbols and positions -------------------------------------------

    #[test]
    fn a_symbol_names_its_own_column() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {\n    let value = parse(value);\n}\n").expect("write");
        let path = file.to_string_lossy().into_owned();

        assert_eq!(
            resolve_symbol_column(&path, 2, Some("parse")).expect("found"),
            17
        );
        // The second occurrence, not the first.
        assert_eq!(
            resolve_symbol_column(&path, 2, Some("value#2")).expect("found"),
            23
        );
        // No symbol: the first thing that is not whitespace.
        assert_eq!(resolve_symbol_column(&path, 2, None).expect("found"), 5);
    }

    #[test]
    fn a_symbol_column_counts_the_way_the_protocol_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "let café = parse();\n").expect("write");
        let path = file.to_string_lossy().into_owned();

        // `café` is 4 characters and 5 bytes. A byte count would put the
        // request one column late and the server would answer nothing.
        assert_eq!(
            resolve_symbol_column(&path, 1, Some("parse")).expect("found"),
            12
        );
    }

    #[test]
    fn a_missing_symbol_is_reported_rather_than_guessed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write");
        let path = file.to_string_lossy().into_owned();

        let error = resolve_symbol_column(&path, 1, Some("nowhere")).expect_err("should fail");
        assert!(error.to_string().contains("does not appear"), "{error}");

        let error = resolve_symbol_column(&path, 99, None).expect_err("should fail");
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn the_context_of_a_location_is_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "one\ntwo\nthree\nfour\n").expect("write");
        let path = file.to_string_lossy().into_owned();

        let context = read_location_context(&path, 2, 1);
        assert_eq!(context.len(), 3);
        assert!(context[1].contains("two"), "{context:?}");
        // At the first line there is nothing above it to show.
        assert_eq!(read_location_context(&path, 1, 1).len(), 2);
    }

    #[test]
    fn a_workspace_symbol_answer_is_read() {
        let answer = json!([{
            "name": "parse_config",
            "kind": 12,
            "containerName": "config",
            "location": {
                "uri": "file:///tmp/a.rs",
                "range": {
                    "start": { "line": 4, "character": 3 },
                    "end": { "line": 4, "character": 15 }
                }
            }
        }]);
        let symbols = parse_workspace_symbols(&answer);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "parse_config");
        assert_eq!(symbols[0].kind, "function");
        assert_eq!(symbols[0].container.as_deref(), Some("config"));
        assert_eq!(symbols[0].location.to_string(), "/tmp/a.rs:5:4");
    }

    // ---- Moving files -----------------------------------------------------

    #[test]
    fn a_file_move_is_one_pair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("a.rs");
        std::fs::write(&from, "").expect("write");
        let to = dir.path().join("b.rs");

        let pairs = enumerate_rename_pairs(&from, &to).expect("pairs");
        assert_eq!(pairs, vec![(from, to)]);
    }

    #[test]
    fn a_directory_move_names_every_file_in_it() {
        // The servers are told about files, not directories, so a directory
        // move that sent one pair would leave every import broken.
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("src");
        std::fs::create_dir_all(from.join("nested")).expect("mkdir");
        std::fs::write(from.join("a.rs"), "").expect("write");
        std::fs::write(from.join("nested/b.rs"), "").expect("write");
        let to = dir.path().join("lib");

        let mut pairs = enumerate_rename_pairs(&from, &to).expect("pairs");
        pairs.sort();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].1, to.join("a.rs"));
        assert_eq!(pairs[1].1, to.join("nested/b.rs"));
    }

    #[tokio::test]
    async fn a_move_onto_an_existing_path_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("a.rs");
        let to = dir.path().join("b.rs");
        std::fs::write(&from, "").expect("write");
        std::fs::write(&to, "keep").expect("write");

        let mut mgr = LspManager::new();
        let error = error_of(mgr.rename_file(&from, &to, dir.path(), true).await);
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(std::fs::read_to_string(&to).expect("read"), "keep");
    }

    // ---- Client lifecycle -------------------------------------------------

    /// The message of a result that must be an error.
    ///
    /// `expect_err` needs `Debug` on the success type, and a live client
    /// holds a process handle and a set of channels that nothing should try
    /// to format.
    fn error_of<T>(result: anyhow::Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    #[tokio::test]
    async fn a_failed_start_is_not_retried_immediately() {
        // Every request used to pay the same startup timeout again, which
        // turns one missing binary into a session where each call is slow.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = LspManager::new();
        let mut config = make_config("nonexistent-server");
        config.command = "a-server-nobody-installed".to_string();
        mgr.register_server(config);

        let first = error_of(mgr.ensure_client("nonexistent-server", dir.path()).await);
        assert!(first.contains("could not start"), "{first}");

        let second = error_of(mgr.ensure_client("nonexistent-server", dir.path()).await);
        assert!(
            second.contains("not retried"),
            "the second call did not back off: {second}"
        );

        mgr.clear_failure("nonexistent-server", dir.path());
        let third = error_of(mgr.ensure_client("nonexistent-server", dir.path()).await);
        assert!(
            third.contains("could not start"),
            "clearing the failure did not allow a retry: {third}"
        );
    }

    #[tokio::test]
    async fn a_disabled_server_is_not_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = LspManager::new();
        let mut config = make_config("ls");
        config.disabled = true;
        mgr.register_server(config);

        let error = error_of(mgr.ensure_client("ls", dir.path()).await);
        assert!(error.contains("switched off"), "{error}");
    }

    #[test]
    fn diagnostics_from_two_servers_are_deduplicated_and_ordered() {
        // Two servers watching one file report the same compiler error twice.
        let mut diagnostics = vec![
            make_diagnostic("a.rs", 5, 1, DiagnosticSeverity::Hint, "unused"),
            make_diagnostic("a.rs", 1, 1, DiagnosticSeverity::Error, "type mismatch"),
            make_diagnostic("a.rs", 1, 1, DiagnosticSeverity::Error, "type mismatch"),
            make_diagnostic("a.rs", 2, 1, DiagnosticSeverity::Warning, "unused import"),
        ];
        dedupe_and_sort(&mut diagnostics);

        assert_eq!(diagnostics.len(), 3, "the repeat was not dropped");
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diagnostics[1].severity, DiagnosticSeverity::Warning);
        assert_eq!(diagnostics[2].severity, DiagnosticSeverity::Hint);
    }

    #[tokio::test]
    async fn an_idle_server_is_stopped_when_a_timeout_is_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (client, _server) = connected(make_config("ls"));
        let mut mgr = LspManager::new();
        mgr.register_server(make_config("ls"));
        mgr.clients
            .insert(("ls".to_string(), dir.path().to_path_buf()), client);
        mgr.last_used.insert(
            ("ls".to_string(), dir.path().to_path_buf()),
            std::time::Instant::now() - std::time::Duration::from_secs(60),
        );

        // Off by default: a server is kept until the session ends.
        mgr.sweep_idle().await;
        assert_eq!(mgr.running_clients().len(), 1);

        mgr.set_idle_timeout(Some(std::time::Duration::from_secs(30)));
        mgr.sweep_idle().await;
        assert!(mgr.running_clients().is_empty(), "the idle server stayed");
    }

    #[tokio::test]
    async fn a_client_is_keyed_by_its_root_as_well_as_its_name() {
        // Keyed by name alone, a second directory reuses a client that
        // indexed the first one and answers against the wrong project.
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        let mut mgr = LspManager::new();
        mgr.register_server(make_config("ls"));

        let (client_a, _a) = connected(make_config("ls"));
        let (client_b, _b) = connected(make_config("ls"));
        mgr.clients
            .insert(("ls".to_string(), first.path().to_path_buf()), client_a);
        mgr.clients
            .insert(("ls".to_string(), second.path().to_path_buf()), client_b);

        assert_eq!(mgr.running_clients().len(), 2);
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
