// ToolSearchTool: discover tools by name or keyword.
//
// The model uses this to find the right tool for a task, or to look up a
// tool it half-remembers. The catalog is built from the *actually registered*
// tools and then narrowed by `mikmik_core::tool_gates`, so it lists exactly
// what this session can call rather than a hand-maintained list that can drift
// from the roster in either direction.
//
// Supports two query modes:
//   - "select:ToolName[,Other]" → direct lookup by exact name(s)
//   - "keyword search"          → ranked name + description + keyword match

use crate::{all_tools, PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct ToolSearchTool;

#[derive(Debug, Deserialize)]
struct ToolSearchInput {
    query: String,
    #[serde(default = "default_max")]
    max_results: usize,
}

fn default_max() -> usize {
    5
}

/// A catalog entry describing one searchable tool.
struct CatalogEntry {
    name: String,
    description: String,
    keywords: &'static [&'static str],
}

/// Extra search synonyms for high-value tools, keyed by canonical name.
/// These improve recall for natural-language queries (e.g. "search the web").
fn keywords_for(name: &str) -> &'static [&'static str] {
    match name {
        "Bash" => &["shell", "run", "command", "exec", "terminal"],
        "Read" => &["file", "cat", "content", "open"],
        "Write" => &["file", "create", "save", "new"],
        "Edit" => &["file", "modify", "replace", "patch", "change"],
        "Glob" => &["find", "pattern", "files", "filename"],
        "Grep" => &["search", "regex", "content", "ripgrep"],
        "WebFetch" => &["web", "url", "http", "download", "browse", "internet"],
        "WebSearch" => &["web", "internet", "google", "browse", "news"],
        "NotebookEdit" => &["notebook", "jupyter", "ipynb", "cell"],
        "TodoWrite" => &["todo", "task", "plan", "checklist"],
        "AskUserQuestion" => &["ask", "question", "clarify", "choose"],
        "Agent" => &["agent", "subagent", "delegate", "parallel", "spawn"],
        "Skill" => &["skill", "slash", "command", "template", "prompt"],
        "Config" => &["config", "settings", "model", "permission"],
        "SendMessage" => &["message", "broadcast", "inbox", "communicate"],
        "Memory" => &["memory", "remember", "recall", "notes", "past"],
        "Learn" => &["memory", "remember", "lesson", "record", "note"],
        "Retain" => &["memory", "remember", "fact", "record", "note", "retain"],
        "Reflect" => &[
            "consolidate",
            "memory",
            "dream",
            "summarize",
            "reflect",
            "organize",
        ],
        _ => &[],
    }
}

/// Tools that are registered outside `all_tools()` (e.g. the Agent tool lives
/// in the query crate) but should still be discoverable here.
static SUPPLEMENTAL_TOOLS: &[(&str, &str)] = &[
    (
        "Agent",
        "Launch a sub-agent to handle a complex, multi-step task in parallel.",
    ),
    (
        "Memory",
        "Load the full text of memory files about a topic.",
    ),
    (
        "Learn",
        "Record one durable lesson about this project for a later session.",
    ),
    (
        "Retain",
        "Record a durable fact about this project for a later session.",
    ),
    (
        "Reflect",
        "Consolidate recent sessions into durable memory now.",
    ),
];

/// Collapse a possibly multi-line/verbose description into a single tidy line.
fn one_line(desc: &str) -> String {
    let collapsed = desc.split_whitespace().collect::<Vec<_>>().join(" ");
    // Prefer the first sentence; otherwise cap the length so results stay terse.
    let first_sentence = collapsed
        .find(". ")
        .map(|i| &collapsed[..=i]) // include the period
        .unwrap_or(&collapsed);
    let trimmed = first_sentence.trim_end_matches('.').trim();
    if trimmed.chars().count() > 160 {
        let cut: String = trimmed.chars().take(157).collect();
        format!("{}...", cut.trim_end())
    } else {
        trimmed.to_string()
    }
}

/// Build the searchable catalog from the live tool registry plus supplements.
///
/// Filtered by the same gates the roster uses. A catalog built from
/// `all_tools()` alone advertised tools the session had withheld: the model
/// searched, found `CronList` with `cronEnabled` off, called it, and the
/// dispatcher answered `Unknown tool` — the wasted turn the gating exists to
/// prevent.
fn build_catalog(ctx: &ToolContext) -> Vec<CatalogEntry> {
    let has_mcp = ctx.mcp_manager.is_some();
    let offered = |name: &str| {
        mikmik_core::tool_gates::tool_is_offered(name, has_mcp, &ctx.config, &ctx.working_dir)
    };

    let mut entries: Vec<CatalogEntry> = all_tools()
        .iter()
        .filter(|t| offered(t.name()))
        .map(|t| CatalogEntry {
            name: t.name().to_string(),
            description: one_line(t.description()),
            keywords: keywords_for(t.name()),
        })
        .collect();

    // The supplements are added on a condition rather than withheld on one, so
    // each carries its own gate. `Agent` has none: the roster always adds it.
    let memory_tools = mikmik_core::tool_gates::offers_memory_tools(&ctx.config);
    for (name, desc) in SUPPLEMENTAL_TOOLS {
        let conditional_on = match *name {
            "Memory" | "Learn" | "Retain" | "Reflect" => memory_tools,
            _ => true,
        };
        if conditional_on && offered(name) && !entries.iter().any(|e| e.name == *name) {
            entries.push(CatalogEntry {
                name: (*name).to_string(),
                description: one_line(desc),
                keywords: keywords_for(name),
            });
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Score a single catalog entry against the lowercase query terms.
/// Name matches dominate, then keywords, then description hits.
fn score_entry(entry: &CatalogEntry, terms: &[&str]) -> usize {
    let name_lower = entry.name.to_lowercase();
    let desc_lower = entry.description.to_lowercase();
    let mut score = 0usize;

    for term in terms {
        if name_lower == *term {
            score += 25; // exact name match ranks highest
        } else if name_lower.contains(term) {
            score += 10;
        }

        for &kw in entry.keywords {
            if kw == *term {
                score += 8;
            } else if kw.contains(term) {
                score += 3;
            }
        }

        if desc_lower
            .split_whitespace()
            .any(|w| w.trim_matches(|c: char| !c.is_alphanumeric()) == *term)
        {
            score += 5; // whole-word description hit
        } else if desc_lower.contains(term) {
            score += 2; // substring description hit
        }
    }

    score
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn description(&self) -> &str {
        "Find the right tool for a task. Search all available tools by name or keyword and \
         get back the best-matching tool names with a one-line description each. Use a natural \
         phrase (e.g. 'search the web', 'edit a file') to discover a capability, or \
         'select:ToolName' for a direct lookup. Returns up to 5 results by default."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A task description or keywords to find a tool, or 'select:ToolName' for a direct lookup"
                },
                "max_results": {
                    "type": "number",
                    "description": "Maximum results to return (default: 5, max: 20)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: ToolSearchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let query = params.query.trim();
        let max = params.max_results.clamp(1, 20);
        let catalog = build_catalog(ctx);

        // ---- select: prefix — direct lookup by exact name(s) ----------------
        if let Some(names_str) = query.strip_prefix("select:").map(str::trim) {
            let requested: Vec<&str> = names_str
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let mut found = Vec::new();
            let mut found_names = Vec::new();
            let mut missing = Vec::new();

            for name in requested {
                if let Some(entry) = catalog.iter().find(|e| e.name.eq_ignore_ascii_case(name)) {
                    found.push(format!("{}: {}", entry.name, entry.description));
                    found_names.push(entry.name.to_string());
                } else {
                    missing.push(name.to_string());
                }
            }

            if found.is_empty() {
                return ToolResult::success(format!(
                    "No matching tools found for: {}",
                    missing.join(", ")
                ));
            }

            let mut out = found.join("\n");
            if !missing.is_empty() {
                out.push_str(&format!("\n\nNot found: {}", missing.join(", ")));
            }
            return found_result(out, &found_names);
        }

        // ---- keyword search with scoring ------------------------------------
        let q_lower = query.to_lowercase();
        let terms: Vec<&str> = q_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 1) // drop empties and single-char noise
            .collect();

        if terms.is_empty() {
            return ToolResult::success(format!(
                "Empty query. Provide keywords or a task description, or use 'select:ToolName'. \
                 {} tools available.",
                catalog.len()
            ));
        }

        let mut scored: Vec<(usize, &CatalogEntry)> = catalog
            .iter()
            .filter_map(|entry| {
                let score = score_entry(entry, &terms);
                if score > 0 {
                    Some((score, entry))
                } else {
                    None
                }
            })
            .collect();

        // Highest score first; break ties by name for deterministic output.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored.truncate(max);

        if scored.is_empty() {
            return ToolResult::success(format!(
                "No tools matched '{}'. Try broader keywords or use 'select:ToolName'. \
                 {} tools are available.",
                query,
                catalog.len()
            ));
        }

        let lines: Vec<String> = scored
            .iter()
            .map(|(_, e)| format!("{}: {}", e.name, e.description))
            .collect();
        let found_names: Vec<String> = scored.iter().map(|(_, e)| e.name.to_string()).collect();

        found_result(
            format!(
                "Tools matching '{}' (use one of these for the task):\n\n{}\n\n{} of {} tools shown.",
                query,
                lines.join("\n"),
                scored.len(),
                catalog.len()
            ),
            &found_names,
        )
    }
}

/// The key `metadata` carries the names this search turned up under.
pub const FOUND_TOOLS_KEY: &str = "found_tools";

/// An answer that also names what it found, for the turn loop to declare next.
///
/// The names ride in `metadata` rather than being parsed back out of the text,
/// because the text is written for the model and changing its wording would
/// silently stop the schemas from being sent.
fn found_result(content: String, names: &[String]) -> ToolResult {
    let mut result = ToolResult::success(content);
    result.metadata = Some(json!({ FOUND_TOOLS_KEY: names }));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ctx_with(everything_on())
    }

    fn ctx_with(config: mikmik_core::config::Config) -> ToolContext {
        let mut ctx = crate::test_support::allow_all_context(std::env::temp_dir());
        ctx.config = config;
        ctx
    }

    /// A session that asked for every gated tool, so a test about ranking or
    /// about the supplement is not silently answering a question about gating.
    fn everything_on() -> mikmik_core::config::Config {
        mikmik_core::config::Config {
            auto_memory_enabled: Some(true),
            teams_enabled: true,
            cron_enabled: true,
            repl_enabled: true,
            computer_use_enabled: true,
            ..Default::default()
        }
    }

    async fn run(query: &str) -> String {
        run_with(query, ctx()).await
    }

    async fn run_with(query: &str, ctx: ToolContext) -> String {
        let tool = ToolSearchTool;
        let out = tool.execute(json!({ "query": query }), &ctx).await;
        out.content
    }

    /// The names an answer reports in `metadata`, which is what the turn loop
    /// reads to decide which schemas to declare next.
    async fn found_names(query: &str) -> Vec<String> {
        let out = ToolSearchTool
            .execute(json!({ "query": query }), &ctx())
            .await;
        out.metadata
            .as_ref()
            .and_then(|m| m.get(FOUND_TOOLS_KEY))
            .and_then(|v| v.as_array())
            .map(|names| {
                names
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_selected_tool_is_named_in_the_metadata() {
        // Without this the turn loop never learns what to declare, and a
        // session with schema deferral on could reach nothing but the core
        // tools no matter how often it searched.
        let names = found_names("select:REPL,CronList").await;

        assert!(names.contains(&"REPL".to_string()), "{names:?}");
        assert!(names.contains(&"CronList".to_string()), "{names:?}");
    }

    #[tokio::test]
    async fn a_keyword_search_names_what_it_listed() {
        let names = found_names("search the web").await;

        assert!(names.contains(&"WebSearch".to_string()), "{names:?}");
    }

    #[tokio::test]
    async fn a_name_that_matches_nothing_is_not_reported_as_found() {
        let names = found_names("select:NoSuchTool").await;

        assert!(names.is_empty(), "{names:?}");
    }

    #[tokio::test]
    async fn web_query_surfaces_web_tools() {
        let out = run("search the web").await;
        assert!(
            out.contains("WebSearch"),
            "expected WebSearch in results, got:\n{out}"
        );
        // WebSearch should rank ahead of WebFetch for this query.
        let ws = out.find("WebSearch");
        let wf = out.find("WebFetch");
        if let (Some(ws), Some(wf)) = (ws, wf) {
            assert!(ws < wf, "WebSearch should rank above WebFetch:\n{out}");
        }
    }

    #[tokio::test]
    async fn exact_name_ranks_first() {
        let out = run("grep").await;
        let first_line = out.lines().find(|l| l.contains(": ")).unwrap_or_default();
        assert!(
            first_line.starts_with("Grep:"),
            "exact name match should rank first, got first result line: {first_line:?}\n{out}"
        );
    }

    #[tokio::test]
    async fn select_prefix_direct_lookup() {
        let out = run("select:WebFetch,DoesNotExist").await;
        assert!(out.contains("WebFetch:"), "should find WebFetch:\n{out}");
        assert!(
            out.contains("Not found: DoesNotExist"),
            "should report the missing tool:\n{out}"
        );
    }

    #[tokio::test]
    async fn agent_is_discoverable_via_supplement() {
        let out = run("delegate a subagent task").await;
        assert!(
            out.contains("Agent"),
            "Agent tool should be discoverable even though it lives outside all_tools():\n{out}"
        );
    }

    /// Both memory tools are registered in the query crate's roster rather
    /// than in `all_tools()`, so the catalog only knows them through the
    /// supplement. Without an entry each, a session with memory on carries a
    /// tool that `ToolSearch` reports as not existing.
    #[tokio::test]
    async fn both_memory_tools_are_discoverable_via_supplement() {
        let out = run("select:Memory,Learn").await;
        assert!(out.contains("Memory:"), "{out}");
        assert!(out.contains("Learn:"), "{out}");
        assert!(!out.contains("Not found"), "{out}");
    }

    #[tokio::test]
    async fn a_search_for_remembering_finds_both_memory_tools() {
        let out = run("remember something for a later session").await;
        assert!(out.contains("Memory"), "{out}");
        assert!(out.contains("Learn"), "{out}");
    }

    /// The defect this filter was written for. A real session with
    /// `cronEnabled` off searched, found `CronList` here, called it, and read
    /// `Error: Unknown tool: CronList` — because the dispatcher looks a call up
    /// in the roster and the roster had withheld it.
    #[tokio::test]
    async fn a_withheld_tool_is_not_advertised() {
        let out = run_with("select:CronList", ctx_with(Default::default())).await;

        assert!(
            out.contains("No matching tools found"),
            "search advertised a tool the roster withholds:\n{out}"
        );
    }

    #[tokio::test]
    async fn a_keyword_search_does_not_surface_a_withheld_tool() {
        // `select:` is the direct route, but a keyword search reaches the same
        // catalog and must not leak what the direct route refuses.
        let out = run_with("schedule a recurring job", ctx_with(Default::default())).await;

        assert!(!out.contains("CronCreate"), "{out}");
        assert!(!out.contains("CronList"), "{out}");
    }

    #[tokio::test]
    async fn turning_the_setting_on_makes_the_tool_discoverable_again() {
        let config = mikmik_core::config::Config {
            cron_enabled: true,
            ..Default::default()
        };
        let out = run_with("select:CronList", ctx_with(config)).await;

        assert!(out.contains("CronList:"), "{out}");
    }

    /// The roster filter decides which tools exist, so the catalog has to read
    /// it too. Otherwise `--disallowed-tools Grep` hides the schema and leaves
    /// the search still recommending it.
    #[tokio::test]
    async fn the_roster_filter_reaches_the_catalog() {
        let config = mikmik_core::config::Config {
            disallowed_tools: vec!["Grep".to_string()],
            ..everything_on()
        };
        let out = run_with("select:Grep", ctx_with(config)).await;

        assert!(out.contains("No matching tools found"), "{out}");
    }

    /// Memory rides a condition rather than a gate, so it needs its own check:
    /// the supplement used to add it whatever the setting said.
    #[tokio::test]
    async fn the_memory_supplement_follows_its_setting() {
        let out = run_with("select:Memory,Learn", ctx_with(Default::default())).await;

        assert!(out.contains("No matching tools found"), "{out}");
    }
}
