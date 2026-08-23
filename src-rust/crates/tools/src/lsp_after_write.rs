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

/// What one session has reported, and when it wrote each file.
#[derive(Default)]
struct SessionState {
    /// So a problem is announced once.
    ledger: lsp::DiagnosticsLedger,
    /// When each file was last written, so an answer that arrives after the
    /// write's own budget can still be recognised as being about that write.
    written: HashMap<String, std::time::Instant>,
}

type Ledgers = HashMap<String, SessionState>;

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
    report_after_batch(&[file_path.to_string()], ctx).await
}

/// The same report for a batch of files, on one shared wait.
///
/// A batch that reported per file would pay the wait once per file, and the
/// wait is the whole cost. The files are sent together and the budget is spent
/// once, on all of them.
pub async fn report_after_batch(file_paths: &[String], ctx: &ToolContext) -> Option<String> {
    let format = ctx.config.effective_lsp_format_on_write();
    let diagnose = ctx.config.effective_lsp_diagnostics_on_write();
    if !format && !diagnose || file_paths.is_empty() {
        return None;
    }

    let manager_arc = lsp::global_lsp_manager();
    let served: Vec<String> = {
        let mut manager = manager_arc.lock().await;
        if ctx.config.effective_lsp_auto_detect() {
            manager.seed_detected(&ctx.working_dir);
        }
        // The precedence order: the catalogue, then `lsp.json`, then the
        // settings file, each overriding the one before it.
        manager.apply_file_config(&ctx.working_dir);
        manager.seed_from_config(&ctx.config.lsp_servers);
        file_paths
            .iter()
            .filter(|path| !manager.servers_for_file(path).is_empty())
            .cloned()
            .collect()
    };
    if served.is_empty() {
        // No server for any of these file types. Nothing to start, nothing to
        // say.
        return None;
    }

    // Every server hears about the change, not only the ones that serve these
    // file types: a server watches files that affect it without serving them,
    // a lock file or a schema for instance.
    {
        let manager = manager_arc.lock().await;
        manager.notify_files_changed(file_paths, FILE_CHANGED).await;
    }

    let mut notes: Vec<String> = Vec::new();

    if format {
        let mut formatted_count = 0usize;
        for file_path in &served {
            let formatted = {
                let mut manager = manager_arc.lock().await;
                manager.format_file(file_path, &ctx.working_dir).await
            };
            match formatted {
                Ok(true) => formatted_count += 1,
                Ok(false) => {}
                Err(e) => tracing::debug!("could not format '{file_path}': {e}"),
            }
        }
        if formatted_count == 1 && served.len() == 1 {
            notes.push("Formatted by the language server.".to_string());
        } else if formatted_count > 0 {
            notes.push(format!(
                "Formatted {formatted_count} file(s) by the language server."
            ));
        }
    }

    if diagnose {
        // What a server said about an earlier write, after that write had
        // stopped waiting. A slow server has an answer nobody read yet, and
        // reading it costs nothing: it is already in memory.
        let late = late_report(ctx, &served).await;
        if !late.is_empty() {
            notes.push(format_problems(
                &late,
                "The language server has since reported {} problem(s) in a file written earlier:",
            ));
        }

        let diagnostics = {
            let mut manager = manager_arc.lock().await;
            manager
                .fresh_diagnostics_for_files(&served, &ctx.working_dir, WRITE_DIAGNOSTICS_WAIT)
                .await
        };

        let fresh = {
            let mut ledgers = LEDGERS.lock().await;
            let state = ledgers.entry(ctx.session_id.clone()).or_default();
            let mut fresh = Vec::new();
            // The ledger remembers per file, so the batch is split back apart.
            // A server may answer with a different spelling of the path, so
            // both sides are resolved through the filesystem before they are
            // compared.
            for file_path in &served {
                let uri = lsp::path_to_uri(file_path);
                let mine: Vec<lsp::LspDiagnostic> = diagnostics
                    .iter()
                    .filter(|d| lsp::path_to_uri(&d.file) == uri)
                    .cloned()
                    .collect();
                fresh.extend(state.ledger.only_new(file_path, mine));
                state
                    .written
                    .insert(file_path.clone(), std::time::Instant::now());
            }
            fresh
        };

        if !fresh.is_empty() {
            notes.push(format_problems(
                &fresh,
                "The language server reports {} new problem(s):",
            ));
        }
    }

    (!notes.is_empty()).then(|| notes.join("\n"))
}

/// One block of problems under a heading, capped.
fn format_problems(problems: &[lsp::LspDiagnostic], heading: &str) -> String {
    let shown = problems.len().min(REPORT_LIMIT);
    let mut lines = vec![heading.replace("{}", &problems.len().to_string())];
    lines.push(lsp::LspManager::format_diagnostics(&problems[..shown]));
    if problems.len() > shown {
        lines.push(format!("... and {} more", problems.len() - shown));
    }
    lines.join("\n")
}

/// Problems a server published for an earlier write, after that write had
/// stopped waiting.
///
/// The files in this batch are excluded: they are about to be asked properly.
/// A publish from before the file was written is left out, because it describes
/// content that is gone.
async fn late_report(ctx: &ToolContext, current: &[String]) -> Vec<lsp::LspDiagnostic> {
    // The two locks are taken one after the other rather than one inside the
    // other, so no order between them can ever be established.
    let earlier: Vec<(String, std::time::Instant)> = {
        let ledgers = LEDGERS.lock().await;
        let Some(state) = ledgers.get(&ctx.session_id) else {
            return Vec::new();
        };
        state
            .written
            .iter()
            .filter(|(file, _)| !current.contains(file))
            .map(|(file, at)| (file.clone(), *at))
            .collect()
    };
    if earlier.is_empty() {
        return Vec::new();
    }

    let published: Vec<(String, Vec<lsp::LspDiagnostic>)> = {
        let manager = lsp::global_lsp_manager();
        let manager = manager.lock().await;
        earlier
            .into_iter()
            .map(|(file, written_at)| {
                let found = manager.late_diagnostics(&file, written_at);
                (file, found)
            })
            .filter(|(_, found)| !found.is_empty())
            .collect()
    };

    let mut ledgers = LEDGERS.lock().await;
    let Some(state) = ledgers.get_mut(&ctx.session_id) else {
        return Vec::new();
    };
    let mut late = Vec::new();
    for (file, found) in published {
        late.extend(state.ledger.only_new(&file, found));
    }
    late
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
    async fn an_empty_batch_reports_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = allow_all_context(dir.path().to_path_buf());
        assert!(report_after_batch(&[], &ctx).await.is_none());
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
