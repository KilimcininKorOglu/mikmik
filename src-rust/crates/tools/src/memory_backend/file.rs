//! The file-backed memory engine: the `.md`-file store wrapped behind the
//! [`MemoryBackend`] trait. Every method delegates to the existing `memdir`
//! functions, so selecting it changes nothing about how memory behaves.

use super::{MemoryBackend, MemoryHit};
use std::path::PathBuf;

pub struct FileBackend {
    memory_dir: PathBuf,
}

impl FileBackend {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }
}

impl MemoryBackend for FileBackend {
    fn prompt_block(&self) -> String {
        mikmik_core::memdir::build_memory_prompt_content(&self.memory_dir)
    }

    fn search(&self, query: &str, max_files: usize) -> Vec<MemoryHit> {
        mikmik_core::memdir::find_relevant_memories_simple(&self.memory_dir, query, max_files)
            .into_iter()
            .map(|file| MemoryHit {
                title: file.meta.filename,
                body: file.content,
                modified_secs: file.meta.modified_secs,
            })
            .collect()
    }
}
