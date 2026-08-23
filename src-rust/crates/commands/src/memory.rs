// `/memory` command (AGENTS.md memory files).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct MemoryCommand;

/// The project's auto memory directory, described for `/memory` output.
///
/// Reports the state rather than the contents: the directory holds whole
/// documents and `/memory` already prints every AGENTS.md in full.
fn auto_memory_section(working_dir: &std::path::Path, config: &mikmik_core::Config) -> String {
    if !mikmik_core::memdir::is_auto_memory_enabled(config.auto_memory_enabled) {
        return "\n\nAuto memory is off. Turn it on with /settings → Auto memory.".to_string();
    }

    let project_root = mikmik_core::session_storage::transcript_root_for(working_dir);
    let dir = mikmik_core::memdir::auto_memory_path(&project_root);
    let files = mikmik_core::memdir::scan_memory_dir(&dir);
    let has_index = mikmik_core::memdir::load_memory_index(&dir).is_some();

    let index_line = if has_index {
        format!("{} is present", mikmik_core::memdir::MEMORY_ENTRYPOINT)
    } else {
        format!("no {} yet", mikmik_core::memdir::MEMORY_ENTRYPOINT)
    };

    let files_line = match files.len() {
        0 => "no memory files yet".to_string(),
        1 => "1 memory file".to_string(),
        n => format!("{n} memory files"),
    };

    format!(
        "\n\nAuto memory\n\
         ───────────\n\
         Path: {}\n\
         {}, {}.\n\
         Use /memories to read, measure or clear it.",
        dir.display(),
        index_line,
        files_line
    )
}

// ---- /memory -------------------------------------------------------------

#[async_trait]
impl SlashCommand for MemoryCommand {
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        "View, edit, or clear AGENTS.md memory files"
    }
    fn help(&self) -> &str {
        "Usage: /memory [edit|clear] [global]\n\n\
         Shows the content of AGENTS.md files that provide project context to MikMik.\n\
         MikMik reads these files automatically at session start.\n\n\
         Subcommands:\n\
           /memory              — show all AGENTS.md files\n\
           /memory edit         — open project AGENTS.md in your editor\n\
           /memory edit global  — open global ~/.config/mikmik/AGENTS.md in your editor\n\
           /memory clear        — clear the project AGENTS.md\n\
           /memory clear global — clear the global ~/.config/mikmik/AGENTS.md\n\n\
         Locations checked (in priority order):\n\
           1. <project>/.mikmik/AGENTS.md\n\
           2. <project>/AGENTS.md\n\
           3. ~/.config/mikmik/AGENTS.md  (global)\n\n\
         Use /init to create a new AGENTS.md from a template."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let project_claude_dir = ctx.working_dir.join(".mikmik").join("AGENTS.md");
        let project_root = ctx.working_dir.join("AGENTS.md");
        let global_path = mikmik_core::config::Settings::config_dir().join("AGENTS.md");

        let locations = [
            ("project (.mikmik/AGENTS.md)", project_claude_dir.clone()),
            ("project (AGENTS.md)", project_root.clone()),
            ("global (~/.config/mikmik/AGENTS.md)", global_path.clone()),
        ];

        let cmd = args.trim();

        // ---- /memory edit [global|project] ------------------------------------
        if cmd == "edit" || cmd.starts_with("edit ") {
            let target_hint = cmd
                .strip_prefix("edit")
                .map(|s| s.trim())
                .unwrap_or("project");
            let target = match target_hint {
                "global" => {
                    // Ensure global dir exists
                    if let Some(parent) = global_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    global_path.clone()
                }
                _ => {
                    // Best project AGENTS.md
                    if project_root.exists() {
                        project_root.clone()
                    } else if project_claude_dir.exists() {
                        project_claude_dir.clone()
                    } else {
                        project_root.clone() // will be created by editor
                    }
                }
            };
            // Create file if it doesn't exist yet
            if !target.exists() {
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&target, "");
            }
            // Handed to the session loop rather than launched here: this crate
            // has no terminal to give the editor, and starting one under the
            // TUI draws it over the frame.
            let (_, editor_hint) = mikmik_core::paths::preferred_editor();
            return CommandResult::OpenInEditor {
                message: format!("Edited {}.\n{}", target.display(), editor_hint),
                path: target,
            };
        }

        // ---- /memory clear [global|project] -----------------------------------
        if cmd == "clear" || cmd.starts_with("clear ") {
            let target_hint = cmd
                .strip_prefix("clear")
                .map(|s| s.trim())
                .unwrap_or("project");
            let (label, target) = match target_hint {
                "global" => ("global (~/.config/mikmik/AGENTS.md)", global_path.clone()),
                _ => {
                    if project_claude_dir.exists() {
                        ("project (.mikmik/AGENTS.md)", project_claude_dir.clone())
                    } else {
                        ("project (AGENTS.md)", project_root.clone())
                    }
                }
            };
            if !target.exists() {
                return CommandResult::Message(format!(
                    "No {} memory file found (nothing to clear).",
                    label
                ));
            }
            return match tokio::fs::write(&target, "").await {
                Ok(_) => CommandResult::Message(format!(
                    "Cleared {} memory file at {}.\n\
                     MikMik will no longer see this content at session start.",
                    label,
                    target.display()
                )),
                Err(e) => {
                    CommandResult::Error(format!("Failed to clear {}: {}", target.display(), e))
                }
            };
        }

        // ---- /memory (show all) -----------------------------------------------
        let mut output = String::from("AGENTS.md Memory Files\n══════════════════════\n");
        let mut found_any = false;

        for (label, path) in &locations {
            if path.exists() {
                found_any = true;
                match tokio::fs::read_to_string(path).await {
                    Ok(content) => {
                        let lines: usize = content.lines().count();
                        let chars = content.len();
                        output.push_str(&format!(
                            "\n[{label}]\nPath: {path}\nSize: {lines} lines, {chars} chars\n\
                             ─────────────────────────────────\n\
                             {content}\n",
                            label = label,
                            path = path.display(),
                            lines = lines,
                            chars = chars,
                            content = if content.len() > 2000 {
                                format!(
                                    "{}…\n(truncated — file is {} chars)",
                                    &content[..2000],
                                    chars
                                )
                            } else {
                                content.clone()
                            }
                        ));
                    }
                    Err(e) => output.push_str(&format!(
                        "\n[{label}] — Error reading {}: {}\n",
                        path.display(),
                        e,
                        label = label
                    )),
                }
            }
        }

        if !found_any {
            output.push_str(
                "\nNo AGENTS.md files found.\n\
                 Use /init to create one in the current project.\n\
                 Use /memory edit to create and open a memory file.",
            );
        } else {
            output.push_str(
                "\nSubcommands:\n\
                 /memory edit          — edit project AGENTS.md\n\
                 /memory edit global   — edit global ~/.config/mikmik/AGENTS.md\n\
                 /memory clear         — clear project AGENTS.md\n\
                 /memory clear global  — clear global AGENTS.md",
            );
        }

        // The second store. Without this the auto memory directory is
        // invisible: it is outside the checkout, the model writes to it on its
        // own, and nothing else in the interface names it.
        output.push_str(&auto_memory_section(&ctx.working_dir, &ctx.config));

        CommandResult::Message(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::CostTracker;

    /// Serialises the tests that redirect the memory directory.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    fn ctx_in(dir: &std::path::Path, auto_memory: Option<bool>) -> CommandContext {
        CommandContext {
            context_window: 200_000,
            context_used_tokens: 0,
            config: mikmik_core::config::Config {
                auto_memory_enabled: auto_memory,
                ..Default::default()
            },
            cost_tracker: CostTracker::new(),
            messages: vec![],
            working_dir: dir.to_path_buf(),
            session_id: "test-session".to_string(),
            session_title: None,
            effort_level: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            interactive: true,
            active_agent: None,
        }
    }

    async fn run(ctx: &mut CommandContext) -> String {
        match MemoryCommand.execute("", ctx).await {
            CommandResult::Message(text) => text,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// The auto memory directory sits outside the checkout and the model
    /// writes to it unprompted, so `/memory` has to name it.
    #[tokio::test]
    async fn the_report_names_the_auto_memory_directory() {
        let _lock = ENV_LOCK.lock().await;
        let project = tempfile::tempdir().expect("tempdir");
        let memory = project.path().join("memory");
        std::fs::create_dir_all(&memory).expect("mkdir");
        std::fs::write(memory.join("MEMORY.md"), "- an index line").expect("index");
        std::fs::write(memory.join("one.md"), "---\nname: One\n---\nbody").expect("topic");
        let _guard = MemoryDirGuard::new(&memory);

        let mut ctx = ctx_in(project.path(), Some(true));
        let output = run(&mut ctx).await;

        assert!(output.contains(&memory.display().to_string()), "{output}");
        assert!(output.contains("MEMORY.md is present"), "{output}");
        assert!(output.contains("1 memory file"), "{output}");
    }

    #[tokio::test]
    async fn a_disabled_directory_is_reported_as_off() {
        let _lock = ENV_LOCK.lock().await;
        let project = tempfile::tempdir().expect("tempdir");
        let _guard = MemoryDirGuard::new(&project.path().join("memory"));

        let mut ctx = ctx_in(project.path(), Some(false));
        let output = run(&mut ctx).await;

        assert!(output.contains("Auto memory is off"), "{output}");
    }
}
