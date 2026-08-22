//! What the language server has to say about a file that was just written.
//!
//! Without this the model learns that its edit does not compile only if it
//! runs a build or asks for diagnostics itself, and it usually does neither.
//! The problems reach it on the write's own result instead, while it is still
//! working on that file.

use crate::ToolContext;
use mikmik_core::lsp;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// How long a write waits for the server to answer.
///
/// Much shorter than the wait an explicit `diagnostics` request gets: this one
/// sits between the model and its next step, and a server that is still
/// indexing has nothing useful to say yet anyway.
const WRITE_DIAGNOSTICS_WAIT: std::time::Duration = std::time::Duration::from_millis(700);

/// How many problems one write reports.
const REPORT_LIMIT: usize = 10;

/// The LSP `FileChangeType` for a file whose content changed.
const FILE_CHANGED: u8 = 2;

/// What each session has already reported, so a problem is announced once.
type Ledgers = HashMap<String, lsp::DiagnosticsLedger>;

static LEDGERS: Lazy<Arc<Mutex<Ledgers>>> = Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Forget what a session reported. Called when the session ends.
pub async fn forget_session(session_id: &str) {
    LEDGERS.lock().await.remove(session_id);
}

/// Format the file, then report the problems the write introduced.
///
/// Returns text to append to the tool's own result, or `None` when there is
/// nothing to say. Never returns an error: a language server that is missing
/// or slow must not turn a successful write into a failed one.
pub async fn report_after_write(file_path: &str, ctx: &ToolContext) -> Option<String> {
    let format = ctx.config.effective_lsp_format_on_write();
    let diagnose = ctx.config.effective_lsp_diagnostics_on_write();
    if !format && !diagnose {
        return None;
    }

    let manager_arc = lsp::global_lsp_manager();
    {
        let mut manager = manager_arc.lock().await;
        if ctx.config.effective_lsp_auto_detect() {
            manager.seed_detected(&ctx.working_dir);
        }
        // The precedence order: the catalogue, then `lsp.json`, then the
        // settings file, each overriding the one before it.
        manager.apply_file_config(&ctx.working_dir);
        manager.seed_from_config(&ctx.config.lsp_servers);
        if manager.servers_for_file(file_path).is_empty() {
            // No server for this file type. Nothing to start, nothing to say.
            return None;
        }
    }

    // Every server hears about the change, not only the ones that serve this
    // file type: a server watches files that affect it without serving them,
    // a lock file or a schema for instance.
    {
        let manager = manager_arc.lock().await;
        manager
            .notify_files_changed(&[file_path.to_string()], FILE_CHANGED)
            .await;
    }

    let mut notes: Vec<String> = Vec::new();

    if format {
        let formatted = {
            let mut manager = manager_arc.lock().await;
            manager.format_file(file_path, &ctx.working_dir).await
        };
        match formatted {
            Ok(true) => notes.push("Formatted by the language server.".to_string()),
            Ok(false) => {}
            Err(e) => tracing::debug!("could not format '{file_path}': {e}"),
        }
    }

    if diagnose {
        let diagnostics = {
            let mut manager = manager_arc.lock().await;
            manager
                .fresh_diagnostics(file_path, &ctx.working_dir, WRITE_DIAGNOSTICS_WAIT)
                .await
        };

        let fresh = {
            let mut ledgers = LEDGERS.lock().await;
            ledgers
                .entry(ctx.session_id.clone())
                .or_default()
                .only_new(file_path, diagnostics)
        };

        if !fresh.is_empty() {
            let shown = fresh.len().min(REPORT_LIMIT);
            let mut lines = vec![format!(
                "The language server reports {} new problem(s):",
                fresh.len()
            )];
            lines.push(lsp::LspManager::format_diagnostics(&fresh[..shown]));
            if fresh.len() > shown {
                lines.push(format!("... and {} more", fresh.len() - shown));
            }
            notes.push(lines.join("\n"));
        }
    }

    (!notes.is_empty()).then(|| notes.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;

    #[tokio::test]
    async fn nothing_is_reported_when_both_switches_are_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ctx = allow_all_context(dir.path().to_path_buf());
        ctx.config.lsp_diagnostics_on_write = Some(false);
        ctx.config.lsp_format_on_write = Some(false);

        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write");
        assert!(report_after_write(&file.to_string_lossy(), &ctx)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_file_no_server_handles_is_skipped() {
        // No server means no work: starting one for a file type nothing serves
        // would add a wait to every write.
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = allow_all_context(dir.path().to_path_buf());
        let file = dir.path().join("notes.unknown-extension");
        std::fs::write(&file, "text\n").expect("write");

        assert!(report_after_write(&file.to_string_lossy(), &ctx)
            .await
            .is_none());
    }
}
