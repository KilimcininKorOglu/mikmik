//! `/memories` — the directory MikMik keeps for itself.
//!
//! Separate from `/memory`, which is about `AGENTS.md`. The two stores look
//! alike and behave differently: `AGENTS.md` is a file the user writes and
//! commits, while this directory sits outside the checkout and the model
//! writes to it unprompted. One command with subcommands for both would put a
//! `clear` that empties `AGENTS.md` one word away from a `clear` that empties
//! the model's memory.
//!
//! Everything here reports or removes. Nothing calls a model: the consolidation
//! that writes this directory runs from the turn loop, and `rebuild` opens its
//! gate rather than starting it, because a command has no session to spawn a
//! sub-agent in.

use super::*;
use async_trait::async_trait;

pub struct MemoriesCommand;

/// The word `clear` needs before it deletes anything.
const CONFIRM_WORD: &str = "confirm";

/// Where this project's memory lives, and whether the feature is on at all.
struct Located {
    dir: std::path::PathBuf,
    conversations: std::path::PathBuf,
}

fn locate(ctx: &CommandContext) -> Result<Located, String> {
    if !mikmik_core::memdir::is_auto_memory_enabled(ctx.config.auto_memory_enabled) {
        return Err(
            "Auto memory is off, so there is no memory directory to report on.\n\
             Turn it on with /settings → Auto memory."
                .to_string(),
        );
    }
    let project_root = mikmik_core::session_storage::transcript_root_for(&ctx.working_dir);
    Ok(Located {
        dir: mikmik_core::memdir::auto_memory_path(&project_root),
        conversations: mikmik_core::session_storage::transcript_dir(&project_root),
    })
}

/// Every `.md` file in the directory, plus `MEMORY.md`, which the scan skips.
fn all_memory_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = mikmik_core::memdir::scan_memory_dir(dir)
        .into_iter()
        .map(|meta| meta.path)
        .collect();
    let index = dir.join(mikmik_core::memdir::MEMORY_ENTRYPOINT);
    if index.exists() {
        paths.push(index);
    }
    paths
}

// ---- /memories ------------------------------------------------------------

fn view(here: &Located) -> String {
    let files = mikmik_core::memdir::scan_memory_dir(&here.dir);
    let index = mikmik_core::memdir::load_memory_index(&here.dir);

    let mut out = format!(
        "Memory directory\n════════════════\nPath: {}\n",
        here.dir.display()
    );

    match index {
        Some(index) => out.push_str(&format!(
            "\n{} ({} lines)\n─────────────────────────────────\n{}\n",
            mikmik_core::memdir::MEMORY_ENTRYPOINT,
            index.content.lines().count(),
            index.content.trim_end()
        )),
        None => out.push_str(&format!(
            "\nNo {} yet. The model writes one when it has something to index.\n",
            mikmik_core::memdir::MEMORY_ENTRYPOINT
        )),
    }

    if files.is_empty() {
        out.push_str("\nNo memory files yet.\n");
    } else {
        out.push_str(&format!(
            "\nMemory files ({})\n─────────────────────────────────\n{}\n",
            files.len(),
            mikmik_core::memdir::format_memory_manifest(&files)
        ));
    }

    out.push_str(
        "\nSubcommands:\n\
         /memories stats     — size against the caps\n\
         /memories diagnose  — why consolidation has or has not run\n\
         /memories clear     — empty the directory\n\
         /memories rebuild   — let consolidation run at the next turn",
    );
    out
}

// ---- /memories stats ------------------------------------------------------

fn stats(here: &Located) -> String {
    let files = mikmik_core::memdir::scan_memory_dir(&here.dir);
    let total_bytes: u64 = files
        .iter()
        .filter_map(|meta| std::fs::metadata(&meta.path).ok())
        .map(|meta| meta.len())
        .sum();

    let mut out = format!(
        "Memory statistics\n═════════════════\nPath: {}\nFiles: {}\nTotal: {} bytes\n",
        here.dir.display(),
        files.len(),
        total_bytes
    );

    // The scan sorts newest first, so the ends of the list are the extremes.
    if let (Some(newest), Some(oldest)) = (files.first(), files.last()) {
        out.push_str(&format!(
            "Newest: {} ({})\nOldest: {} ({})\n",
            newest.filename,
            mikmik_core::memdir::memory_age(newest.modified_secs),
            oldest.filename,
            mikmik_core::memdir::memory_age(oldest.modified_secs)
        ));
    }

    // The index is the one file with a size limit, because it is injected
    // whole. Everything else is only listed until the model asks for it.
    match mikmik_core::memdir::load_memory_index(&here.dir) {
        Some(index) => {
            let lines = index.content.lines().count();
            let bytes = index.content.len();
            out.push_str(&format!(
                "\n{}: {} of {} lines, {} of {} bytes{}\n",
                mikmik_core::memdir::MEMORY_ENTRYPOINT,
                lines,
                mikmik_core::memdir::MAX_ENTRYPOINT_LINES,
                bytes,
                mikmik_core::memdir::MAX_ENTRYPOINT_BYTES,
                if lines > mikmik_core::memdir::MAX_ENTRYPOINT_LINES
                    || bytes > mikmik_core::memdir::MAX_ENTRYPOINT_BYTES
                {
                    " — truncated before it reaches the prompt"
                } else {
                    ""
                }
            ));
        }
        None => out.push_str(&format!(
            "\nNo {} yet.\n",
            mikmik_core::memdir::MEMORY_ENTRYPOINT
        )),
    }

    out
}

// ---- /memories diagnose ---------------------------------------------------

async fn diagnose(here: &Located) -> String {
    let dreamer =
        mikmik_query::auto_dream::AutoDream::new(here.dir.clone(), here.conversations.clone());
    let state = dreamer.load_state().await;
    let report = match dreamer.diagnose(&state).await {
        Ok(report) => report,
        Err(error) => return format!("Could not read the consolidation state: {error}"),
    };

    let mark = |ok: bool| if ok { "yes" } else { "no " };

    let last = match report.last_consolidated_at {
        Some(_) => match report.hours_elapsed {
            Some(hours) => format!("{hours:.1} hours ago"),
            None => "unknown".to_string(),
        },
        None => "never".to_string(),
    };

    format!(
        "Consolidation gates\n═══════════════════\n\
         Memory directory: {}\n\
         Transcripts:      {}{}\n\
         Last run:         {}\n\n\
         {} time      — {} of {} hours\n\
         {} sessions  — {} of {} new transcripts\n\
         {} lock      — {}\n\n\
         All three have to pass at the end of a turn. \
         `/memories rebuild` clears the state file, which opens the time gate.",
        here.dir.display(),
        here.conversations.display(),
        if here.conversations.exists() {
            ""
        } else {
            " (missing — the session gate cannot pass)"
        },
        last,
        mark(report.time_ok),
        report
            .hours_elapsed
            .map(|hours| format!("{hours:.1}"))
            .unwrap_or_else(|| "never run".to_string()),
        report.min_hours,
        mark(report.sessions_seen >= report.sessions_needed),
        report.sessions_seen,
        report.sessions_needed,
        mark(report.lock_free),
        if report.lock_free {
            "free".to_string()
        } else {
            format!("held by {}", report.lock_file.display())
        }
    )
}

// ---- /memories clear ------------------------------------------------------

fn describe_clear(here: &Located) -> String {
    let paths = all_memory_files(&here.dir);
    if paths.is_empty() {
        return format!(
            "The memory directory is already empty.\nPath: {}",
            here.dir.display()
        );
    }

    let listing = paths
        .iter()
        .map(|path| {
            format!(
                "  {}",
                path.strip_prefix(&here.dir).unwrap_or(path).display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "This would delete {} file(s) from {}:\n\n{}\n\n\
         Nothing has been deleted. Run `/memories clear {}` to go ahead.\n\
         What the model learned about this project is not recoverable afterwards.",
        paths.len(),
        here.dir.display(),
        listing,
        CONFIRM_WORD
    )
}

async fn clear(here: &Located) -> String {
    let paths = all_memory_files(&here.dir);
    let mut removed = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for path in &paths {
        match tokio::fs::remove_file(path).await {
            Ok(()) => removed += 1,
            Err(error) => failures.push(format!("  {}: {error}", path.display())),
        }
    }

    if failures.is_empty() {
        format!(
            "Deleted {removed} file(s) from {}.\n\
             The directory itself is kept, so the next session writes into it again.",
            here.dir.display()
        )
    } else {
        format!(
            "Deleted {removed} file(s); {} could not be removed:\n{}",
            failures.len(),
            failures.join("\n")
        )
    }
}

// ---- /memories rebuild ----------------------------------------------------

async fn rebuild(here: &Located) -> String {
    let state_file = here.dir.join(".consolidation_state.json");
    if !state_file.exists() {
        return format!(
            "There is no consolidation state to clear, so the time gate is \
             already open. Consolidation runs at the end of a turn once {} \
             transcripts are newer than the last run.",
            mikmik_query::auto_dream::AutoDreamConfig::default().min_sessions
        );
    }

    match tokio::fs::remove_file(&state_file).await {
        Ok(()) => format!(
            "Cleared {}.\n\
             The time gate is open now. Consolidation still waits for the session \
             gate, so run `/memories diagnose` to see whether it will fire.",
            state_file.display()
        ),
        Err(error) => format!("Could not clear {}: {error}", state_file.display()),
    }
}

// ---- the command ----------------------------------------------------------

#[async_trait]
impl SlashCommand for MemoriesCommand {
    fn name(&self) -> &str {
        "memories"
    }

    fn description(&self) -> &str {
        "Inspect or clear the memory directory MikMik keeps for this project"
    }

    fn help(&self) -> &str {
        "Usage: /memories [stats|diagnose|clear [confirm]|rebuild]\n\n\
         The memory directory is the second memory store. It sits outside the \
         checkout and MikMik writes to it on its own; /memory is the other one, \
         the AGENTS.md files you write.\n\n\
         Subcommands:\n\
           /memories                 — path, index and file list\n\
           /memories stats           — file count, size, and the index against its caps\n\
           /memories diagnose        — why consolidation has or has not run\n\
           /memories clear           — list what would be deleted\n\
           /memories clear confirm   — delete it\n\
           /memories rebuild         — clear the consolidation state so the time gate opens\n\n\
         Turn the directory on and off with /settings → Auto memory."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let here = match locate(ctx) {
            Ok(here) => here,
            Err(message) => return CommandResult::Message(message),
        };

        let mut words = args.split_whitespace();
        let output = match words.next() {
            None => view(&here),
            Some("stats") => stats(&here),
            Some("diagnose") => diagnose(&here).await,
            Some("rebuild") => rebuild(&here).await,
            Some("clear") | Some("reset") => match words.next() {
                Some(word) if word == CONFIRM_WORD => clear(&here).await,
                _ => describe_clear(&here),
            },
            Some(other) => {
                return CommandResult::Error(format!(
                    "Unknown subcommand `{other}`. Try /memories, \
                     /memories stats, /memories diagnose, /memories clear or \
                     /memories rebuild."
                ))
            }
        };

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

    async fn run(ctx: &mut CommandContext, args: &str) -> String {
        match MemoriesCommand.execute(args, ctx).await {
            CommandResult::Message(text) => text,
            CommandResult::Error(text) => text,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// A project whose memory directory holds an index and one topic file.
    fn seeded() -> (tempfile::TempDir, MemoryDirGuard, std::path::PathBuf) {
        let project = tempfile::tempdir().expect("tempdir");
        let memory = project.path().join("memory");
        std::fs::create_dir_all(&memory).expect("mkdir");
        std::fs::write(memory.join("MEMORY.md"), "- [Deploy](deploy.md) — releases")
            .expect("index");
        std::fs::write(
            memory.join("deploy.md"),
            "---\nname: Deploy\ndescription: releases\ntype: project\n---\nTag, then wait.",
        )
        .expect("topic");
        let guard = MemoryDirGuard::new(&memory);
        (project, guard, memory)
    }

    #[tokio::test]
    async fn the_view_names_the_path_the_index_and_the_files() {
        let _lock = ENV_LOCK.lock().await;
        let (project, _guard, memory) = seeded();
        let mut ctx = ctx_in(project.path(), Some(true));

        let out = run(&mut ctx, "").await;

        assert!(out.contains(&memory.display().to_string()), "{out}");
        assert!(out.contains("releases"), "{out}");
        assert!(out.contains("deploy.md"), "{out}");
    }

    #[tokio::test]
    async fn stats_measure_the_index_against_its_caps() {
        let _lock = ENV_LOCK.lock().await;
        let (project, _guard, _memory) = seeded();
        let mut ctx = ctx_in(project.path(), Some(true));

        let out = run(&mut ctx, "stats").await;

        assert!(out.contains("Files: 1"), "{out}");
        assert!(
            out.contains(&format!(
                "of {} lines",
                mikmik_core::memdir::MAX_ENTRYPOINT_LINES
            )),
            "{out}"
        );
    }

    #[tokio::test]
    async fn diagnose_reports_all_three_gates() {
        let _lock = ENV_LOCK.lock().await;
        let (project, _guard, _memory) = seeded();
        let mut ctx = ctx_in(project.path(), Some(true));

        let out = run(&mut ctx, "diagnose").await;

        assert!(out.contains("time"), "{out}");
        assert!(out.contains("sessions"), "{out}");
        assert!(out.contains("lock"), "{out}");
        assert!(
            out.contains("never"),
            "a fresh project has never consolidated:\n{out}"
        );
    }

    /// The dangerous one. Without the word, nothing may be deleted.
    #[tokio::test]
    async fn clear_without_the_word_deletes_nothing() {
        let _lock = ENV_LOCK.lock().await;
        let (project, _guard, memory) = seeded();
        let mut ctx = ctx_in(project.path(), Some(true));

        let out = run(&mut ctx, "clear").await;

        assert!(out.contains("Nothing has been deleted"), "{out}");
        assert!(memory.join("deploy.md").exists());
        assert!(memory.join("MEMORY.md").exists());
    }

    #[tokio::test]
    async fn clear_with_the_word_empties_the_directory() {
        let _lock = ENV_LOCK.lock().await;
        let (project, _guard, memory) = seeded();
        let mut ctx = ctx_in(project.path(), Some(true));

        let out = run(&mut ctx, "clear confirm").await;

        assert!(out.contains("Deleted 2 file(s)"), "{out}");
        assert!(!memory.join("deploy.md").exists());
        assert!(
            !memory.join("MEMORY.md").exists(),
            "the scan skips the index, so clearing has to add it back"
        );
        assert!(memory.exists(), "the directory itself is kept");
    }

    #[tokio::test]
    async fn rebuild_clears_the_consolidation_state() {
        let _lock = ENV_LOCK.lock().await;
        let (project, _guard, memory) = seeded();
        let state = memory.join(".consolidation_state.json");
        std::fs::write(&state, r#"{"last_consolidated_at":1700000000}"#).expect("state");
        let mut ctx = ctx_in(project.path(), Some(true));

        let out = run(&mut ctx, "rebuild").await;

        assert!(out.contains("time gate is open"), "{out}");
        assert!(!state.exists());
    }

    #[tokio::test]
    async fn every_subcommand_says_so_when_the_feature_is_off() {
        let _lock = ENV_LOCK.lock().await;
        let (project, _guard, _memory) = seeded();
        let mut ctx = ctx_in(project.path(), Some(false));

        for args in ["", "stats", "diagnose", "clear confirm", "rebuild"] {
            let out = run(&mut ctx, args).await;
            assert!(
                out.contains("Auto memory is off"),
                "`/memories {args}` did not check the gate:\n{out}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_directory_answers_every_subcommand() {
        let _lock = ENV_LOCK.lock().await;
        let project = tempfile::tempdir().expect("tempdir");
        let memory = project.path().join("memory");
        let _guard = MemoryDirGuard::new(&memory);
        let mut ctx = ctx_in(project.path(), Some(true));

        for args in ["", "stats", "diagnose", "clear", "clear confirm", "rebuild"] {
            let out = run(&mut ctx, args).await;
            assert!(!out.is_empty(), "`/memories {args}` said nothing");
        }
    }

    #[tokio::test]
    async fn an_unknown_subcommand_lists_the_real_ones() {
        let _lock = ENV_LOCK.lock().await;
        let (project, _guard, _memory) = seeded();
        let mut ctx = ctx_in(project.path(), Some(true));

        let out = run(&mut ctx, "wipe").await;
        assert!(out.contains("Unknown subcommand"), "{out}");
        assert!(out.contains("/memories stats"), "{out}");
    }
}
