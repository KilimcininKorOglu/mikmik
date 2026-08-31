//! The file-backed memory engine: the `.md`-file store wrapped behind the
//! [`MemoryBackend`] trait. Every method delegates to the existing `memdir`
//! functions and the shared [`crate::memory_append`] helper, so selecting it
//! changes nothing about how memory behaves.

use super::{MemoryBackend, MemoryHit};
use crate::memory_append::{append_entry, AppendConfig};
use crate::ToolResult;
use async_trait::async_trait;
use std::path::PathBuf;

/// The append policy for `learned.md`. Public within the crate so the `Learn`
/// tool reads the caps for its input schema from one place.
pub(crate) const LEARN: AppendConfig = AppendConfig {
    filename: "learned.md",
    frontmatter: "---\n\
name: Learned lessons\n\
description: Durable lessons this project taught, newest first\n\
type: project\n\
---\n",
    max_item_chars: 2000,
    max_context_chars: 400,
    cap: 100,
    noun: "lesson",
};

/// The append policy for `facts.md`, the fact twin of [`LEARN`].
pub(crate) const RETAIN: AppendConfig = AppendConfig {
    filename: "facts.md",
    frontmatter: "---\n\
name: Retained facts\n\
description: Durable facts about this project, newest first\n\
type: project\n\
---\n",
    max_item_chars: 2000,
    max_context_chars: 400,
    cap: 100,
    noun: "fact",
};

pub struct FileBackend {
    memory_dir: PathBuf,
}

impl FileBackend {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }
}

#[async_trait]
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

    async fn append_lesson(
        &self,
        item: &str,
        topic: Option<&str>,
        context: Option<&str>,
    ) -> ToolResult {
        append_entry(&self.memory_dir, &LEARN, item, topic, context).await
    }

    async fn retain_fact(
        &self,
        item: &str,
        topic: Option<&str>,
        context: Option<&str>,
    ) -> ToolResult {
        append_entry(&self.memory_dir, &RETAIN, item, topic, context).await
    }
}
