//! Memory backend abstraction: the engine behind the auto-memory directory.
//!
//! Today one engine exists, [`file::FileBackend`], the `.md`-file store this
//! tree has always used. [`backend_for`] selects it; a second engine (sqlite)
//! plugs in behind the same trait without the call sites changing. The read
//! paths that feed the model, the system-prompt block and the Memory tool's
//! search, go through this trait so a different engine can answer them.

pub mod file;

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

/// The read operations every memory engine provides. Extended with the write
/// paths when the sqlite engine lands and needs to intercept them.
pub trait MemoryBackend: Send + Sync {
    /// The `<memory>` block for the system prompt.
    fn prompt_block(&self) -> String;
    /// The most relevant memories for a query, best first.
    fn search(&self, query: &str, max_files: usize) -> Vec<MemoryHit>;
}

/// Select the engine for a `memoryBackend` setting. `Some("sqlite")` will
/// select the sqlite engine once it lands; every other value, including `None`,
/// is the file engine, so an unset setting behaves exactly as before.
pub fn backend_for(backend: Option<&str>, memory_dir: &Path) -> Box<dyn MemoryBackend> {
    // The sqlite engine reads `backend` once it lands; until then every value
    // resolves to the file engine.
    let _ = backend;
    Box::new(file::FileBackend::new(memory_dir.to_path_buf()))
}
