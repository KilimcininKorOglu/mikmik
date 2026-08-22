// LspTool — code intelligence via the Language Server Protocol.
//
// Navigation, symbols, diagnostics, refactoring, and an escape hatch for a
// request this tool has no wrapper for.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use mikmik_core::lsp;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct LspTool;

/// How long one file's diagnostics are waited for.
///
/// Long enough for a warm server to answer and for a cold one to finish its
/// first pass on a single file; not long enough to hold a turn when the server
/// has nothing to say.
const SINGLE_DIAGNOSTICS_WAIT: std::time::Duration = std::time::Duration::from_millis(3_000);

/// How many diagnostics one answer carries.
///
/// A file with hundreds of errors says the same thing as a file with fifty,
/// and the rest is context the model pays for.
const DIAGNOSTIC_MESSAGE_LIMIT: usize = 50;

/// How many workspace symbols one answer carries.
const WORKSPACE_SYMBOL_LIMIT: usize = 200;

/// How many results carry the source line around them.
///
/// Context is what makes a result readable without opening the file, and it is
/// also what makes a long list expensive.
const CONTEXT_LIMIT: usize = 20;

/// The actions that only read.
const READ_ONLY_ACTIONS: &[&str] = &[
    "hover",
    "definition",
    "references",
    "symbols",
    "diagnostics",
    "type_definition",
    "implementation",
    "status",
    "capabilities",
];

/// The value that means "the whole workspace" rather than one file.
const WORKSPACE: &str = "*";

/// Explain why no server answered for `file_path`.
///
/// "No server configured" alone left the caller guessing between three very
/// different situations: the language has no catalogue entry, the project does
/// not look like one this server serves, or the binary is not installed. Each
/// needs a different fix, so each is named.
fn no_server_message(file_path: &str, cwd: &Path) -> String {
    let candidates: Vec<&lsp::LspServerConfig> = lsp::builtin_servers()
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
        let has_marker = lsp::has_root_markers(cwd, &server.root_markers);
        let has_binary = lsp::resolve_command(&server.command, cwd).is_some();
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

/// Format a list of `path:line:column` results, with context for the first few.
fn format_locations(locations: &[String], label: &str) -> String {
    if locations.is_empty() {
        return format!("No {label} found");
    }
    let mut lines = vec![format!("Found {} {label}:", locations.len())];
    for (index, location) in locations.iter().enumerate() {
        lines.push(location.clone());
        if index >= CONTEXT_LIMIT {
            continue;
        }
        // `path:line:column`, split from the right so a Windows drive letter
        // stays with the path.
        let mut parts = location.rsplitn(3, ':');
        let (_column, line, path) = (parts.next(), parts.next(), parts.next());
        if let (Some(path), Some(line)) = (path, line.and_then(|l| l.parse::<u32>().ok())) {
            for context in lsp::read_location_context(path, line, 1) {
                lines.push(context);
            }
        }
    }
    if locations.len() > CONTEXT_LIMIT {
        lines.push(format!(
            "({} shown with context, the rest as locations only)",
            CONTEXT_LIMIT
        ));
    }
    lines.join("\n")
}

/// Resolve the column for a position request.
///
/// A caller may give the column, or name the symbol and let the file answer.
/// Naming it is the reliable form: counting columns by hand is the commonest
/// way a request lands on the wrong token and answers nothing.
fn resolve_column(
    file_path: &str,
    line: u32,
    column: Option<u32>,
    symbol: Option<&str>,
) -> Result<u32, String> {
    if let Some(column) = column {
        return Ok(column);
    }
    lsp::resolve_symbol_column(file_path, line, symbol).map_err(|e| e.to_string())
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
        "Query a language server for code intelligence: hover documentation, \
         go-to-definition, type-definition, implementations, find-references, \
         document and workspace symbols, diagnostics, rename, file rename, and \
         code actions. Also reports server status and capabilities, reloads a \
         server, and can send a raw LSP request. Servers for common projects \
         are detected automatically; more can be declared in settings \
         (lsp_servers)."
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
                    "enum": [
                        "hover", "definition", "references", "symbols", "diagnostics",
                        "type_definition", "implementation", "rename", "rename_file",
                        "code_actions", "status", "reload", "capabilities", "request"
                    ],
                    "description": "The LSP action to perform."
                },
                "file": {
                    "type": "string",
                    "description": "Absolute or working-directory-relative path to the source file. Use \"*\" for the workspace with symbols, reload and capabilities. For rename_file this is the path to move."
                },
                "line": {
                    "type": "integer",
                    "description": "1-based line number, for the position-based actions."
                },
                "column": {
                    "type": "integer",
                    "description": "1-based column number. Prefer `symbol`, which finds the column for you."
                },
                "symbol": {
                    "type": "string",
                    "description": "The symbol on `line` to point at, e.g. \"parse_config\". Write \"name#2\" for the second occurrence on that line. More reliable than counting columns."
                },
                "query": {
                    "type": "string",
                    "description": "The search text for workspace symbols, the code-action kind to filter by, or the method name for `request`."
                },
                "new_name": {
                    "type": "string",
                    "description": "The new name for `rename`, or the destination path for `rename_file`."
                },
                "apply": {
                    "type": "boolean",
                    "description": "For rename and rename_file, apply the change (default true; false previews it). For code_actions, apply the action selected by `query` (default false, which lists them)."
                },
                "payload": {
                    "type": "string",
                    "description": "JSON parameters for `request`."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let action = match input.get("action").and_then(|v| v.as_str()) {
            Some(a) => a.to_string(),
            None => return ToolResult::error("'action' is required"),
        };

        let file_raw = input
            .get("file")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let workspace_scope = file_raw.is_empty() || file_raw == WORKSPACE;

        let file_path = if workspace_scope {
            ctx.working_dir.to_string_lossy().into_owned()
        } else if Path::new(&file_raw).is_absolute() {
            file_raw.clone()
        } else {
            ctx.working_dir
                .join(&file_raw)
                .to_string_lossy()
                .into_owned()
        };

        let read_only = READ_ONLY_ACTIONS.contains(&action.as_str());
        if let Err(e) = ctx.check_permission_for_path(
            self.name(),
            &format!("LSP {action} {file_path}"),
            PathBuf::from(&file_path),
            read_only,
        ) {
            return ToolResult::error(e.to_string());
        }

        let line = input.get("line").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        let column = input
            .get("column")
            .and_then(|v| v.as_u64())
            .map(|c| c as u32);
        let symbol = input.get("symbol").and_then(|v| v.as_str());
        let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let new_name = input.get("new_name").and_then(|v| v.as_str()).unwrap_or("");
        let apply = input.get("apply").and_then(|v| v.as_bool());

        let manager_arc = lsp::global_lsp_manager();
        {
            let mut manager = manager_arc.lock().await;
            // The catalogue first, the user's entries second, so an entry
            // naming a catalogue server replaces it rather than competing
            // with it.
            if ctx.config.effective_lsp_auto_detect() {
                manager.seed_detected(&ctx.working_dir);
            }
            // The precedence order: the catalogue, then `lsp.json`, then the
            // settings file, each overriding the one before it.
            manager.apply_file_config(&ctx.working_dir);
            manager.seed_from_config(&ctx.config.lsp_servers);
            // Read on every call rather than once, so a change to the setting
            // applies without restarting the session. The sweep runs here
            // because nothing else wakes up to run it. The settings file wins
            // over `lsp.json`, as it does for everything else.
            let idle = ctx
                .config
                .effective_lsp_idle_timeout()
                .or_else(|| manager.apply_file_config(&ctx.working_dir));
            manager.set_idle_timeout(idle);
            manager.sweep_idle().await;
        }

        // --- Workspace-scoped actions --------------------------------------
        match action.as_str() {
            "status" => {
                let manager = manager_arc.lock().await;
                let configured = manager.servers();
                if configured.is_empty() {
                    return ToolResult::success(
                        "No language server is configured for this project.".to_string(),
                    );
                }
                let running: Vec<String> = manager
                    .running_clients()
                    .iter()
                    .map(|(name, root, _)| format!("{name} @ {}", root.display()))
                    .collect();
                let mut lines = vec!["Language servers:".to_string()];
                for server in configured {
                    let state = if server.disabled {
                        "disabled".to_string()
                    } else if running.iter().any(|r| r.starts_with(&server.name)) {
                        "running".to_string()
                    } else if lsp::resolve_command(&server.command, &ctx.working_dir).is_some() {
                        "configured, not started".to_string()
                    } else {
                        format!("`{}` is not installed", server.command)
                    };
                    let role = if server.is_linter { " (linter)" } else { "" };
                    lines.push(format!("  {}{role}: {state}", server.name));
                }
                return ToolResult::success(lines.join("\n"));
            }

            "capabilities" => {
                let names: Vec<String> = {
                    let manager = manager_arc.lock().await;
                    if workspace_scope {
                        manager
                            .servers()
                            .iter()
                            .filter(|c| !c.disabled)
                            .map(|c| c.name.clone())
                            .collect()
                    } else {
                        manager
                            .servers_for_file(&file_path)
                            .iter()
                            .map(|c| c.name.clone())
                            .collect()
                    }
                };
                if names.is_empty() {
                    return ToolResult::success(no_server_message(&file_path, &ctx.working_dir));
                }
                let mut sections = Vec::new();
                for name in names {
                    let mut manager = manager_arc.lock().await;
                    match manager.ensure_client(&name, &ctx.working_dir).await {
                        Ok(client) => {
                            let caps = serde_json::to_string_pretty(&client.server_capabilities())
                                .unwrap_or_else(|_| "{}".to_string());
                            sections.push(format!("{name}:\n{caps}"));
                        }
                        Err(e) => sections.push(format!("{name}: {e}")),
                    }
                }
                return ToolResult::success(sections.join("\n\n"));
            }

            "reload" => {
                let names: Vec<String> = {
                    let manager = manager_arc.lock().await;
                    if workspace_scope {
                        manager
                            .servers()
                            .iter()
                            .filter(|c| !c.disabled)
                            .map(|c| c.name.clone())
                            .collect()
                    } else {
                        manager
                            .servers_for_file(&file_path)
                            .iter()
                            .map(|c| c.name.clone())
                            .collect()
                    }
                };
                if names.is_empty() {
                    return ToolResult::success(no_server_message(&file_path, &ctx.working_dir));
                }
                let mut report = Vec::new();
                {
                    let mut manager = manager_arc.lock().await;
                    // Detection is re-run: a server may have been installed, or
                    // the project may have gained a marker, since the session
                    // started.
                    manager.forget_detection();
                    for name in names {
                        match manager.reload_server(&name, &ctx.working_dir).await {
                            Ok(line) => report.push(line),
                            Err(e) => report.push(format!("{name}: {e}")),
                        }
                    }
                }
                return ToolResult::success(report.join("\n"));
            }

            "symbols" if workspace_scope => {
                if query.is_empty() {
                    return ToolResult::error(
                        "workspace symbols need a `query`; pass a `file` to list one file's symbols",
                    );
                }
                let result = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .workspace_symbols(query, &ctx.working_dir, WORKSPACE_SYMBOL_LIMIT)
                        .await
                };
                return match result {
                    Ok(symbols) if symbols.is_empty() => {
                        ToolResult::success(format!("No symbol matches \"{query}\""))
                    }
                    Ok(symbols) => {
                        let mut lines = vec![format!(
                            "Found {} symbol(s) matching \"{query}\":",
                            symbols.len()
                        )];
                        for symbol in &symbols {
                            let container = symbol
                                .container
                                .as_deref()
                                .map(|c| format!(" in {c}"))
                                .unwrap_or_default();
                            lines.push(format!(
                                "{} ({}){container} @ {}",
                                symbol.name, symbol.kind, symbol.location
                            ));
                        }
                        ToolResult::success(lines.join("\n"))
                    }
                    Err(e) => ToolResult::error(format!("workspace symbols failed: {e}")),
                };
            }

            "request" => {
                if query.is_empty() {
                    return ToolResult::error("`request` needs the method name in `query`");
                }
                let server_name = {
                    let manager = manager_arc.lock().await;
                    if workspace_scope {
                        manager
                            .servers()
                            .iter()
                            .find(|c| !c.disabled && !c.is_linter)
                            .map(|c| c.name.clone())
                    } else {
                        manager
                            .primary_server_for_file(&file_path)
                            .map(|c| c.name.clone())
                    }
                };
                let Some(server_name) = server_name else {
                    return ToolResult::success(no_server_message(&file_path, &ctx.working_dir));
                };

                let params = match input.get("payload").and_then(|v| v.as_str()) {
                    Some(payload) => match serde_json::from_str::<Value>(payload) {
                        Ok(value) => value,
                        Err(e) => return ToolResult::error(format!("`payload` is not JSON: {e}")),
                    },
                    None if workspace_scope => serde_json::json!({}),
                    None => {
                        let column = match resolve_column(&file_path, line, column, symbol) {
                            Ok(column) => column,
                            Err(e) => return ToolResult::error(e),
                        };
                        serde_json::json!({
                            "textDocument": { "uri": lsp::path_to_uri(&file_path) },
                            "position": {
                                "line": line.saturating_sub(1),
                                "character": column.saturating_sub(1)
                            }
                        })
                    }
                };

                if !workspace_scope {
                    let mut manager = manager_arc.lock().await;
                    if let Err(e) = manager.sync_file(&file_path, &ctx.working_dir).await {
                        return ToolResult::error(e.to_string());
                    }
                }

                let result = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .raw_request(&server_name, &ctx.working_dir, query, params)
                        .await
                };
                return match result {
                    Ok(value) => ToolResult::success(format!(
                        "{server_name} answered {query}:\n{}",
                        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
                    )),
                    Err(e) => ToolResult::error(e.to_string()),
                };
            }

            "rename_file" => {
                if file_raw.is_empty() || workspace_scope {
                    return ToolResult::error("`rename_file` needs the path to move in `file`");
                }
                if new_name.is_empty() {
                    return ToolResult::error(
                        "`rename_file` needs the destination path in `new_name`",
                    );
                }
                let destination = if Path::new(new_name).is_absolute() {
                    PathBuf::from(new_name)
                } else {
                    ctx.working_dir.join(new_name)
                };
                if let Err(e) = ctx.check_permission_for_path(
                    self.name(),
                    &format!("LSP rename_file {}", destination.display()),
                    destination.clone(),
                    false,
                ) {
                    return ToolResult::error(e.to_string());
                }

                let result = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .rename_file(
                            Path::new(&file_path),
                            &destination,
                            &ctx.working_dir,
                            apply.unwrap_or(true),
                        )
                        .await
                };
                return match result {
                    Ok(report) => ToolResult::success(report.join("\n")),
                    Err(e) => ToolResult::error(e.to_string()),
                };
            }

            _ => {}
        }

        // --- File-scoped actions -------------------------------------------
        if file_raw.is_empty() {
            return ToolResult::error(format!("`{action}` needs a `file`"));
        }

        {
            let manager = manager_arc.lock().await;
            if manager.servers_for_file(&file_path).is_empty() {
                return ToolResult::success(no_server_message(&file_path, &ctx.working_dir));
            }
        }

        // Diagnostics send the file themselves, as part of waiting for a fresh
        // answer.
        if action != "diagnostics" {
            let mut manager = manager_arc.lock().await;
            if let Err(e) = manager.sync_file(&file_path, &ctx.working_dir).await {
                return ToolResult::error(format!("could not send the file to the server: {e}"));
            }
        }

        match action.as_str() {
            "hover" => {
                let column = match resolve_column(&file_path, line, column, symbol) {
                    Ok(column) => column,
                    Err(e) => return ToolResult::error(e),
                };
                let result = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .hover(&file_path, &ctx.working_dir, line, column)
                        .await
                };
                match result {
                    Ok(Some(text)) => ToolResult::success(text),
                    Ok(None) => ToolResult::success(format!(
                        "No hover information at {file_path}:{line}:{column}"
                    )),
                    Err(e) => ToolResult::error(format!("hover failed: {e}")),
                }
            }

            "definition" | "type_definition" | "implementation" => {
                let column = match resolve_column(&file_path, line, column, symbol) {
                    Ok(column) => column,
                    Err(e) => return ToolResult::error(e),
                };
                let result = {
                    let mut manager = manager_arc.lock().await;
                    match action.as_str() {
                        "type_definition" => {
                            manager
                                .type_definition(&file_path, &ctx.working_dir, line, column)
                                .await
                        }
                        "implementation" => {
                            manager
                                .implementation(&file_path, &ctx.working_dir, line, column)
                                .await
                        }
                        _ => {
                            manager
                                .definition(&file_path, &ctx.working_dir, line, column)
                                .await
                        }
                    }
                };
                let label = match action.as_str() {
                    "type_definition" => "type definition(s)",
                    "implementation" => "implementation(s)",
                    _ => "definition(s)",
                };
                match result {
                    Ok(locations) => ToolResult::success(format_locations(&locations, label)),
                    Err(e) => ToolResult::error(format!("{action} failed: {e}")),
                }
            }

            "references" => {
                let column = match resolve_column(&file_path, line, column, symbol) {
                    Ok(column) => column,
                    Err(e) => return ToolResult::error(e),
                };
                let result = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .references(&file_path, &ctx.working_dir, line, column)
                        .await
                };
                match result {
                    Ok(locations) => {
                        ToolResult::success(format_locations(&locations, "reference(s)"))
                    }
                    Err(e) => ToolResult::error(format!("references failed: {e}")),
                }
            }

            "symbols" => {
                let result = {
                    let mut manager = manager_arc.lock().await;
                    manager.document_symbols(&file_path, &ctx.working_dir).await
                };
                match result {
                    Ok(symbols) if symbols.is_empty() => {
                        ToolResult::success(format!("No symbols found in '{file_path}'."))
                    }
                    Ok(symbols) => ToolResult::success(format!(
                        "Symbols in {file_path}:\n{}",
                        symbols.join("\n")
                    )),
                    Err(e) => ToolResult::error(format!("symbols failed: {e}")),
                }
            }

            "rename" => {
                if new_name.is_empty() {
                    return ToolResult::error("`rename` needs the new name in `new_name`");
                }
                let column = match resolve_column(&file_path, line, column, symbol) {
                    Ok(column) => column,
                    Err(e) => return ToolResult::error(e),
                };
                let edit = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .rename(&file_path, &ctx.working_dir, line, column, new_name)
                        .await
                };
                let edit = match edit {
                    Ok(edit) => edit,
                    Err(e) => return ToolResult::error(format!("rename failed: {e}")),
                };

                let files = lsp::workspace_edit_files(&edit);
                if files.is_empty() {
                    return ToolResult::success("The rename produced no edits".to_string());
                }
                if apply == Some(false) {
                    let mut lines = vec!["Rename preview:".to_string()];
                    for (uri, edits) in &files {
                        lines.push(format!(
                            "{}: {} edit(s)",
                            lsp::uri_to_path(uri),
                            edits.len()
                        ));
                    }
                    return ToolResult::success(lines.join("\n"));
                }
                match lsp::apply_workspace_edit(&edit) {
                    Ok(applied) => {
                        let mut lines = vec![format!("Renamed to {new_name}:")];
                        lines.extend(applied);
                        for operation in lsp::workspace_edit_resource_operations(&edit) {
                            lines.push(format!("not performed: {operation}"));
                        }
                        ToolResult::success(lines.join("\n"))
                    }
                    Err(e) => ToolResult::error(format!("could not apply the rename: {e}")),
                }
            }

            "code_actions" => {
                let column = match resolve_column(&file_path, line, column, symbol) {
                    Ok(column) => column,
                    Err(e) => return ToolResult::error(e),
                };
                let listing_filter = if apply == Some(true) {
                    None
                } else {
                    Some(query)
                };
                let actions = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .code_actions(&file_path, &ctx.working_dir, line, column, listing_filter)
                        .await
                };
                let actions = match actions {
                    Ok(actions) => actions,
                    Err(e) => return ToolResult::error(format!("code actions failed: {e}")),
                };
                if actions.is_empty() {
                    return ToolResult::success(format!(
                        "No code action at {file_path}:{line}:{column}"
                    ));
                }

                let titles: Vec<String> = actions
                    .iter()
                    .map(|a| {
                        a.get("title")
                            .and_then(|t| t.as_str())
                            .unwrap_or("<untitled>")
                            .to_string()
                    })
                    .collect();

                if apply != Some(true) {
                    let mut lines = vec![format!("{} code action(s):", actions.len())];
                    for (index, title) in titles.iter().enumerate() {
                        let kind = actions[index]
                            .get("kind")
                            .and_then(|k| k.as_str())
                            .unwrap_or("");
                        lines.push(format!("{index}: [{kind}] {title}"));
                    }
                    lines.push(
                        "Pass apply=true with query set to the index or part of the title to apply one."
                            .to_string(),
                    );
                    return ToolResult::success(lines.join("\n"));
                }

                if query.is_empty() {
                    return ToolResult::error(
                        "applying a code action needs `query`: the index or part of the title",
                    );
                }
                let chosen = query
                    .parse::<usize>()
                    .ok()
                    .filter(|index| *index < actions.len())
                    .or_else(|| {
                        let wanted = query.to_lowercase();
                        titles
                            .iter()
                            .position(|title| title.to_lowercase().contains(&wanted))
                    });
                let Some(chosen) = chosen else {
                    let mut lines = vec![format!("No code action matches \"{query}\". Available:")];
                    for (index, title) in titles.iter().enumerate() {
                        lines.push(format!("{index}: {title}"));
                    }
                    return ToolResult::success(lines.join("\n"));
                };

                let result = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .apply_code_action(&file_path, &ctx.working_dir, &actions[chosen])
                        .await
                };
                match result {
                    Ok(report) if report.is_empty() => ToolResult::success(format!(
                        "\"{}\" has no edit and no command to apply",
                        titles[chosen]
                    )),
                    Ok(report) => ToolResult::success(format!(
                        "Applied \"{}\":\n{}",
                        titles[chosen],
                        report.join("\n")
                    )),
                    Err(e) => ToolResult::error(format!("could not apply the action: {e}")),
                }
            }

            "diagnostics" => {
                // Wait for a fresh answer rather than sleeping a fixed 200 ms
                // and reading whatever the cache happens to hold: a cold
                // server had not replied by then, so a broken file reported
                // "no diagnostics".
                let diagnostics = {
                    let mut manager = manager_arc.lock().await;
                    manager
                        .fresh_diagnostics(&file_path, &ctx.working_dir, SINGLE_DIAGNOSTICS_WAIT)
                        .await
                };

                if diagnostics.is_empty() {
                    return ToolResult::success(format!("No diagnostics for '{file_path}'."));
                }

                let shown = diagnostics.len().min(DIAGNOSTIC_MESSAGE_LIMIT);
                let output = lsp::LspManager::format_diagnostics(&diagnostics[..shown]);
                if diagnostics.len() > shown {
                    return ToolResult::success(format!(
                        "{output}\n... and {} more, hidden to keep the output readable",
                        diagnostics.len() - shown
                    ));
                }
                ToolResult::success(output)
            }

            other => ToolResult::error(format!(
                "Unknown action '{other}'. Valid actions: hover, definition, type_definition, \
                 implementation, references, symbols, diagnostics, rename, rename_file, \
                 code_actions, status, reload, capabilities, request"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_action_is_not_treated_as_a_write() {
        // The permission prompt says which it is, so a mistake here asks for
        // write access to answer a hover.
        assert!(READ_ONLY_ACTIONS.contains(&"hover"));
        assert!(READ_ONLY_ACTIONS.contains(&"diagnostics"));
        assert!(!READ_ONLY_ACTIONS.contains(&"rename"));
        assert!(!READ_ONLY_ACTIONS.contains(&"rename_file"));
        assert!(!READ_ONLY_ACTIONS.contains(&"code_actions"));
        assert!(!READ_ONLY_ACTIONS.contains(&"request"));
    }

    #[test]
    fn every_action_in_the_schema_is_handled() {
        // A name in the schema that the match does not answer would be
        // rejected as unknown after the model had been told to use it.
        let schema = LspTool.input_schema();
        let listed: Vec<String> = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("the schema lists the actions")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(listed.len(), 14, "listed = {listed:?}");
        for action in &listed {
            assert!(
                READ_ONLY_ACTIONS.contains(&action.as_str())
                    || ["rename", "rename_file", "code_actions", "reload", "request"]
                        .contains(&action.as_str()),
                "'{action}' is in the schema but is neither read-only nor a known write"
            );
        }
    }

    #[test]
    fn an_empty_location_list_says_so() {
        assert_eq!(
            format_locations(&[], "definition(s)"),
            "No definition(s) found"
        );
    }

    #[test]
    fn a_location_is_shown_with_the_line_around_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "one\ntwo\nthree\n").expect("write");

        let location = format!("{}:2:1", file.display());
        let text = format_locations(&[location], "definition(s)");
        assert!(text.contains("Found 1 definition(s)"), "{text}");
        assert!(text.contains("two"), "the context line is missing: {text}");
    }

    #[test]
    fn a_named_symbol_beats_a_counted_column() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() { parse(); }\n").expect("write");
        let path = file.to_string_lossy().into_owned();

        // The explicit column wins when it is given.
        assert_eq!(resolve_column(&path, 1, Some(7), Some("parse")), Ok(7));
        // Otherwise the symbol answers, 1-based.
        assert_eq!(resolve_column(&path, 1, None, Some("parse")), Ok(13));
        // With neither, the first non-whitespace column.
        assert_eq!(resolve_column(&path, 1, None, None), Ok(1));
        // A symbol that is not there is reported rather than guessed.
        assert!(resolve_column(&path, 1, None, Some("missing")).is_err());
    }
}
