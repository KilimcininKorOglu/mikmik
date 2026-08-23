//! Refuse a write that would store a credential in the memory directory.
//!
//! The memory directory is not an ordinary destination. Anything in it is read
//! back into the system prompt of every later session in the same project, so
//! a credential written here is re-sent on every request until somebody opens
//! the file. The consolidation sub-agent reaches this directory through the
//! ordinary `Write` and `Edit` tools, reading session transcripts on the way,
//! which is exactly the path a token would travel.
//!
//! The check refuses rather than rewrites. A tool that silently changed the
//! bytes it was asked to write would report success for a file the caller
//! never asked for, and the model would have no way to tell.
//!
//! Everywhere else on disk this check is silent: the user's own repository is
//! their business, and a `.env` file is not made safer by a tool that will not
//! write it.

use std::path::Path;

use crate::ToolContext;

/// Whether `path` is inside this project's auto-memory directory.
fn inside_memory_dir(ctx: &ToolContext, path: &Path) -> bool {
    let project_root = mikmik_core::session_storage::transcript_root_for(&ctx.working_dir);
    let memory_dir = mikmik_core::memdir::auto_memory_path(&project_root);
    // Compare the paths as given. Both sides come from the same resolver, and
    // canonicalising would fail for a file that does not exist yet, which is
    // the common case for a first write.
    path.starts_with(&memory_dir)
}

/// The refusal message for a write into the memory directory that carries a
/// credential, or `None` when the write may proceed.
///
/// The message names the class and never the value: an error string is copied
/// into the transcript, and quoting the secret there would defeat the check
/// that just caught it.
pub(crate) fn refuse_secret_write(ctx: &ToolContext, path: &Path, content: &str) -> Option<String> {
    if !inside_memory_dir(ctx, path) {
        return None;
    }

    let classes = mikmik_core::redact::find_secrets(content);
    if classes.is_empty() {
        return None;
    }

    Some(format!(
        "This write was refused. {} is a memory file, and the content carries \
         what looks like a credential ({}). A memory file is read back into the \
         system prompt of every later session in this project, so a key stored \
         here is re-sent on every request. Write the fact without the value: \
         name the variable, not what it holds.",
        path.display(),
        classes.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use serde_json::json;

    /// Serialises the tests that redirect the memory directory, because the
    /// override is a process-wide environment variable.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A context whose memory directory is a known temporary path.
    struct Fixture {
        _dir: tempfile::TempDir,
        _guard: MemoryDirGuard,
        ctx: ToolContext,
        memory_dir: std::path::PathBuf,
        elsewhere: std::path::PathBuf,
    }

    struct MemoryDirGuard {
        saved: Option<std::ffi::OsString>,
    }

    impl MemoryDirGuard {
        fn new(dir: &Path) -> Self {
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

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).expect("mkdir");
        let guard = MemoryDirGuard::new(&memory_dir);
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let elsewhere = dir.path().join("src");
        Fixture {
            _dir: dir,
            _guard: guard,
            ctx,
            memory_dir,
            elsewhere,
        }
    }

    const SECRET: &str = "the key is ghp_AAAABBBBCCCCDDDDEEEEFFFFGGGG";

    #[tokio::test]
    async fn a_credential_bound_for_the_memory_directory_is_refused() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let message = refuse_secret_write(&f.ctx, &f.memory_dir.join("notes.md"), SECRET)
            .expect("the write should have been refused");
        assert!(message.contains("github"), "{message}");
        assert!(
            !message.contains("ghp_AAAABBBBCCCCDDDDEEEEFFFFGGGG"),
            "the refusal quoted the secret back: {message}"
        );
    }

    #[tokio::test]
    async fn a_nested_memory_file_is_covered_too() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let nested = f.memory_dir.join("topics").join("deploy.md");
        assert!(refuse_secret_write(&f.ctx, &nested, SECRET).is_some());
    }

    #[tokio::test]
    async fn the_same_content_is_allowed_anywhere_else() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        assert!(refuse_secret_write(&f.ctx, &f.elsewhere.join("main.rs"), SECRET).is_none());
    }

    #[tokio::test]
    async fn ordinary_memory_content_passes() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let clean = "The deploy step reads the token from the environment.";
        assert!(refuse_secret_write(&f.ctx, &f.memory_dir.join("notes.md"), clean).is_none());
    }

    // ---- the three writers that reach the directory -----------------------
    //
    // The consolidation sub-agent uses these tools, not the predicate above,
    // so each one needs its own proof that the check is actually wired in.

    #[tokio::test]
    async fn the_write_tool_refuses_and_leaves_no_file() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let target = f.memory_dir.join("notes.md");

        let result = crate::FileWriteTool
            .execute(
                json!({ "file_path": target.to_string_lossy(), "content": SECRET }),
                &f.ctx,
            )
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("github"), "{}", result.content);
        assert!(!target.exists(), "the file was written despite the refusal");
    }

    #[tokio::test]
    async fn the_edit_tool_refuses_without_touching_the_file() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let target = f.memory_dir.join("notes.md");
        std::fs::write(&target, "the deploy token lives in the environment\n").expect("seed");

        let result = crate::FileEditTool
            .execute(
                json!({
                    "file_path": target.to_string_lossy(),
                    "old_string": "the environment",
                    "new_string": SECRET,
                }),
                &f.ctx,
            )
            .await;

        assert!(result.is_error, "{}", result.content);
        assert_eq!(
            std::fs::read_to_string(&target).expect("read back"),
            "the deploy token lives in the environment\n"
        );
    }

    /// BatchEdit validates every edit before it writes any of them, so one
    /// credential has to abort the whole batch, including the clean edit.
    #[tokio::test]
    async fn the_batch_tool_aborts_the_whole_batch() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let target = f.memory_dir.join("notes.md");
        std::fs::write(&target, "alpha\nbeta\n").expect("seed");

        let result = crate::BatchEditTool
            .execute(
                json!({ "edits": [
                    { "file_path": target.to_string_lossy(), "old_string": "alpha", "new_string": "ALPHA" },
                    { "file_path": target.to_string_lossy(), "old_string": "beta", "new_string": SECRET },
                ] }),
                &f.ctx,
            )
            .await;

        assert!(result.is_error, "{}", result.content);
        assert_eq!(
            std::fs::read_to_string(&target).expect("read back"),
            "alpha\nbeta\n",
            "the clean edit was applied even though the batch was refused"
        );
    }

    /// The same three writers must stay unchanged outside the directory. The
    /// user's own repository is their business.
    #[tokio::test]
    async fn a_write_outside_the_directory_is_untouched() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let target = f.elsewhere.join("config.rs");

        let result = crate::FileWriteTool
            .execute(
                json!({ "file_path": target.to_string_lossy(), "content": SECRET }),
                &f.ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(std::fs::read_to_string(&target).expect("read back"), SECRET);
    }
}
