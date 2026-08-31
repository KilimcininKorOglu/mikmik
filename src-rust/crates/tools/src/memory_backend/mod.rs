//! Memory backend abstraction: the engine behind the auto-memory directory.
//!
//! Today one engine exists, [`file::FileBackend`], the `.md`-file store this
//! tree has always used. [`backend_for`] selects it; a second engine (sqlite)
//! plugs in behind the same trait without the call sites changing. The read
//! paths that feed the model, the system-prompt block and the Memory tool's
//! search, go through this trait so a different engine can answer them.

pub mod file;
pub mod sqlite;

use crate::ToolResult;
use async_trait::async_trait;
use std::path::Path;

/// One memory a search returned, independent of the engine that stored it.
pub struct MemoryHit {
    /// A heading for the hit; the filename for the file engine.
    pub title: String,
    /// The full body to show the model.
    pub body: String,
    /// Modification time in unix seconds, for the freshness note.
    pub modified_secs: u64,
}

/// The operations every memory engine provides: the two read paths that feed
/// the model, and the two write paths behind the `Learn` and `Retain` tools.
/// Both engines answer them, so which one a session uses is invisible to the
/// call sites.
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    /// The `<memory>` block for the system prompt.
    fn prompt_block(&self) -> String;
    /// The most relevant memories for a query, best first.
    fn search(&self, query: &str, max_files: usize) -> Vec<MemoryHit>;
    /// Record one durable lesson (the `Learn` tool). Returns the tool result.
    async fn append_lesson(
        &self,
        item: &str,
        topic: Option<&str>,
        context: Option<&str>,
    ) -> ToolResult;
    /// Record one durable fact (the `Retain` tool). Returns the tool result.
    async fn retain_fact(
        &self,
        item: &str,
        topic: Option<&str>,
        context: Option<&str>,
    ) -> ToolResult;
}

/// Select the engine for a `memoryBackend` setting. `Some("sqlite")` selects
/// the sqlite engine; every other value, including `None`, is the file engine,
/// so an unset setting behaves exactly as before.
///
/// Selecting the file engine while a `memory.db` is present exports it back to
/// `.md` files first, so a project that switches sqlite → file gets its stored
/// memories back. The export runs once, then sets the database aside.
pub fn backend_for(backend: Option<&str>, memory_dir: &Path) -> Box<dyn MemoryBackend> {
    match backend {
        Some("sqlite") => Box::new(sqlite::SqliteBackend::new(memory_dir.to_path_buf())),
        _ => {
            sqlite::export_to_files(memory_dir);
            Box::new(file::FileBackend::new(memory_dir.to_path_buf()))
        }
    }
}
