// System-prompt assembly for the query loop.
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use crate::*;

/// Build the system prompt from config.
///
/// Delegates to `mikmik_core::system_prompt::build_system_prompt` so that all
/// default content (capabilities, safety guidelines, dynamic-boundary marker,
/// etc.) is assembled in one place.  The `QueryConfig` fields map directly to
/// `SystemPromptOptions`:
///
/// - `system_prompt`        → `custom_system_prompt` (added to cacheable block)
/// - `append_system_prompt` → `append_system_prompt` (added after boundary)
///
/// Public so `--dump-system-prompt` can print exactly what a run would send,
/// rather than a second assembly that drifts from this one.
pub fn build_system_prompt(config: &QueryConfig) -> SystemPrompt {
    use mikmik_core::system_prompt::SystemPromptOptions;

    let opts = SystemPromptOptions {
        custom_system_prompt: config.system_prompt.clone(),
        append_system_prompt: config.append_system_prompt.clone(),
        // All other fields use sensible defaults:
        // - prefix:                auto-detect from env
        // - replace_system_prompt: false (additive mode)
        memory_content: memory_content(config),
        output_style: config.output_style,
        custom_output_style_prompt: config.output_style_prompt.clone(),
        working_directory: config.working_directory.clone(),
        workspace_roots: config.workspace_roots.clone(),
        // Forward the session's enabled tool set so per-tool guideline blocks
        // are only emitted for tools that are actually loaded (issue #233).
        enabled_tools: config.enabled_tools.clone(),
        companion_addendum: config.companion_addendum.clone(),
        ..Default::default()
    };

    let text = mikmik_core::system_prompt::build_system_prompt(&opts);
    SystemPrompt::Text(text)
}

/// The project's memory directory, described for the `<memory>` block.
///
/// Empty when the feature is off, when the session has no working directory
/// to scope a project by, or when the directory holds nothing yet.
///
/// The project root is resolved the way transcripts resolve theirs, so a
/// session started in a subdirectory reads the same memory as one started at
/// the repository root.
fn memory_content(config: &QueryConfig) -> String {
    if !config.auto_memory_enabled {
        return String::new();
    }
    let Some(working_dir) = config.working_directory.as_deref() else {
        return String::new();
    };
    let project_root =
        mikmik_core::session_storage::transcript_root_for(std::path::Path::new(working_dir));
    mikmik_core::memdir::build_memory_prompt_content(&mikmik_core::memdir::auto_memory_path(
        &project_root,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that point the memory directory somewhere safe.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point `auto_memory_path` at `dir` and restore the variable on drop.
    struct MemoryDirGuard {
        saved: Option<std::ffi::OsString>,
    }

    impl MemoryDirGuard {
        fn new(dir: &std::path::Path) -> Self {
            let saved = std::env::var_os("MIKMIK_MEMORY_PATH_OVERRIDE");
            std::env::set_var("MIKMIK_MEMORY_PATH_OVERRIDE", dir);
            Self { saved }
        }
    }

    impl Drop for MemoryDirGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var("MIKMIK_MEMORY_PATH_OVERRIDE", value),
                None => std::env::remove_var("MIKMIK_MEMORY_PATH_OVERRIDE"),
            }
        }
    }

    fn config_with_memory(enabled: bool, cwd: &std::path::Path) -> QueryConfig {
        QueryConfig {
            auto_memory_enabled: enabled,
            working_directory: Some(cwd.display().to_string()),
            ..Default::default()
        }
    }

    fn write_memory(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).expect("create memory dir");
        std::fs::write(dir.join("MEMORY.md"), "- the user prefers tabs").expect("write index");
    }

    #[test]
    fn the_memory_block_is_absent_until_the_setting_asks_for_it() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let memory = dir.path().join("memory");
        write_memory(&memory);
        let _guard = MemoryDirGuard::new(&memory);

        let SystemPrompt::Text(text) = build_system_prompt(&config_with_memory(false, dir.path()))
        else {
            panic!("expected a text prompt");
        };

        assert!(
            !text.contains("<memory>"),
            "a disabled feature still reached the prompt"
        );
    }

    #[test]
    fn an_enabled_session_carries_the_directory_into_the_prompt() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let memory = dir.path().join("memory");
        write_memory(&memory);
        let _guard = MemoryDirGuard::new(&memory);

        let SystemPrompt::Text(text) = build_system_prompt(&config_with_memory(true, dir.path()))
        else {
            panic!("expected a text prompt");
        };

        assert!(text.contains("<memory>"), "no memory block:\n{text}");
        assert!(text.contains("the user prefers tabs"), "index missing");
        assert!(
            text.contains(&memory.display().to_string()),
            "the model was not told where to write"
        );
    }

    /// A directory with nothing in it must not add an empty block.
    #[test]
    fn an_untouched_directory_adds_nothing() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = MemoryDirGuard::new(&dir.path().join("memory"));

        let SystemPrompt::Text(text) = build_system_prompt(&config_with_memory(true, dir.path()))
        else {
            panic!("expected a text prompt");
        };

        assert!(!text.contains("<memory>"), "empty block emitted:\n{text}");
    }
}
