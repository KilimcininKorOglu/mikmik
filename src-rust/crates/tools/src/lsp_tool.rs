// LspTool — code intelligence via Language Server Protocol.
//
// Supports hover, definition, references, document symbols, and diagnostics.
// Ported from the TypeScript LSPTool; extended with full action routing.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde_json::Value;

pub struct LspTool;

/// Explain why no server answered for `file_path`.
///
/// "No server configured" alone left the caller guessing between three very
/// different situations: the language has no catalogue entry, the project does
/// not look like one this server serves, or the binary is not installed. Each
/// needs a different fix, so each is named.
fn no_server_message(file_path: &str, cwd: &std::path::Path) -> String {
    let candidates: Vec<&mikmik_core::lsp::LspServerConfig> = mikmik_core::lsp::builtin_servers()
        .iter()
        .filter(|server| server.handles_file(file_path))
        .collect();

    if candidates.is_empty() {
        return format!(
            "No language server handles '{file_path}'. \
             The bundled catalogue has no entry for this file type. \
             Add one to `lsp_servers` in your settings to serve it."
        );
    }

    let mut reasons: Vec<String> = Vec::new();
    for server in &candidates {
        let has_marker = mikmik_core::lsp::has_root_markers(cwd, &server.root_markers);
        let has_binary = mikmik_core::lsp::resolve_command(&server.command, cwd).is_some();
        let reason = match (has_marker, has_binary) {
            (false, false) => format!(
                "{}: no {} in this directory, and `{}` is not installed",
                server.name,
                server.root_markers.join(" or "),
                server.command
            ),
            (false, true) => format!(
                "{}: no {} in this directory",
                server.name,
                server.root_markers.join(" or ")
            ),
            (true, false) => format!("{}: `{}` is not installed", server.name, server.command),
            // Detected, so it would have answered. Reached only when the user
            // switched detection off or disabled this entry.
            (true, true) => format!("{}: switched off", server.name),
        };
        reasons.push(reason);
    }

    format!(
        "No language server is running for '{file_path}'. The servers that could serve it:\n{}",
        reasons
            .iter()
            .map(|r| format!("  - {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

#[async_trait]
impl Tool for LspTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "LSP"
    }

    fn description(&self) -> &str {
        "Query a language server for code intelligence. Supports hover documentation, \
         go-to-definition, find-references, document symbols, and diagnostics. \
         Language servers must be configured in settings (lsp_servers)."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["hover", "definition", "references", "symbols", "diagnostics"],
                    "description": "The LSP action to perform."
                },
                "file": {
                    "type": "string",
                    "description": "Absolute or working-directory-relative path to the source file."
                },
                "line": {
                    "type": "integer",
                    "description": "1-based line number (required for hover, definition, references)."
                },
                "column": {
                    "type": "integer",
                    "description": "1-based column number (required for hover, definition, references)."
                }
            },
            "required": ["action", "file"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        // --- Parse inputs ---------------------------------------------------
        let action = match input.get("action").and_then(|v| v.as_str()) {
            Some(a) => a.to_string(),
            None => return ToolResult::error("'action' is required"),
        };

        let file_raw = match input.get("file").and_then(|v| v.as_str()) {
            Some(f) => f.to_string(),
            None => return ToolResult::error("'file' is required"),
        };

        // Resolve to absolute path
        let file_path = if std::path::Path::new(&file_raw).is_absolute() {
            file_raw.clone()
        } else {
            ctx.working_dir
                .join(&file_raw)
                .to_string_lossy()
                .into_owned()
        };

        if let Err(e) = ctx.check_permission_for_path(
            self.name(),
            &format!("LSP {} {}", action, file_path),
            std::path::PathBuf::from(&file_path),
            true,
        ) {
            return ToolResult::error(e.to_string());
        }

        // line/column only required for position-based actions
        let line = input.get("line").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        let column = input.get("column").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

        // --- Seed the global LSP manager with configs from current session ---
        let lsp_manager_arc = mikmik_core::lsp::global_lsp_manager();
        {
            let mut manager = lsp_manager_arc.lock().await;
            // The catalogue first, the user's entries second, so an entry
            // naming a catalogue server replaces it rather than competing
            // with it.
            if ctx.config.effective_lsp_auto_detect() {
                manager.seed_detected(&ctx.working_dir);
            }
            manager.seed_from_config(&ctx.config.lsp_servers);
        }

        // Check that at least one server is registered for this file before
        // doing expensive I/O.
        {
            let manager = lsp_manager_arc.lock().await;
            if manager.server_name_for_file_pub(&file_path).is_none() {
                return ToolResult::success(no_server_message(&file_path, &ctx.working_dir));
            }
        }

        // --- Ensure the file is opened on its LSP server --------------------
        {
            let mut manager = lsp_manager_arc.lock().await;
            if let Err(e) = manager.open_file(&file_path, &ctx.working_dir).await {
                return ToolResult::error(format!("Failed to open file in LSP: {}", e));
            }
        }

        // --- Dispatch action ------------------------------------------------
        match action.as_str() {
            "hover" => {
                let result = {
                    let mut manager = lsp_manager_arc.lock().await;
                    manager
                        .hover(&file_path, &ctx.working_dir, line, column)
                        .await
                };
                match result {
                    Ok(Some(text)) => ToolResult::success(text),
                    Ok(None) => ToolResult::success(format!(
                        "No hover information at {}:{}:{}",
                        file_path, line, column
                    )),
                    Err(e) => ToolResult::error(format!("hover failed: {}", e)),
                }
            }

            "definition" => {
                let result = {
                    let mut manager = lsp_manager_arc.lock().await;
                    manager
                        .definition(&file_path, &ctx.working_dir, line, column)
                        .await
                };
                match result {
                    Ok(locs) if locs.is_empty() => ToolResult::success(format!(
                        "No definition found at {}:{}:{}",
                        file_path, line, column
                    )),
                    Ok(locs) => ToolResult::success(locs.join("\n")),
                    Err(e) => ToolResult::error(format!("definition failed: {}", e)),
                }
            }

            "references" => {
                let result = {
                    let mut manager = lsp_manager_arc.lock().await;
                    manager
                        .references(&file_path, &ctx.working_dir, line, column)
                        .await
                };
                match result {
                    Ok(locs) if locs.is_empty() => ToolResult::success(format!(
                        "No references found at {}:{}:{}",
                        file_path, line, column
                    )),
                    Ok(locs) => ToolResult::success(format!(
                        "{} reference(s):\n{}",
                        locs.len(),
                        locs.join("\n")
                    )),
                    Err(e) => ToolResult::error(format!("references failed: {}", e)),
                }
            }

            "symbols" => {
                let result = {
                    let mut manager = lsp_manager_arc.lock().await;
                    manager
                        .document_symbols(&file_path, &ctx.working_dir)
                        .await
                };
                match result {
                    Ok(syms) if syms.is_empty() => {
                        ToolResult::success(format!("No symbols found in '{}'.", file_path))
                    }
                    Ok(syms) => ToolResult::success(syms.join("\n")),
                    Err(e) => ToolResult::error(format!("symbols failed: {}", e)),
                }
            }

            "diagnostics" => {
                // Give the server a short window to deliver diagnostics via the
                // textDocument/publishDiagnostics notification (at most 200 ms).
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                let diagnostics = {
                    let manager = lsp_manager_arc.lock().await;
                    manager.get_diagnostics_for_file(&file_path)
                };

                if diagnostics.is_empty() {
                    return ToolResult::success(format!(
                        "No diagnostics for '{}'.",
                        file_path
                    ));
                }

                let output = mikmik_core::lsp::LspManager::format_diagnostics(&diagnostics);
                ToolResult::success(output)
            }

            other => ToolResult::error(format!(
                "Unknown action '{}'. Valid actions: hover, definition, references, symbols, diagnostics",
                other
            )),
        }
    }
}
