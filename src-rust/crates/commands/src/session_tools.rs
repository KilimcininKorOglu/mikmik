// Session & output tools: `/skills`, `/rewind`, `/stats`, `/files`, `/rename`, `/effort`, `/summary`, `/commit`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct SkillsCommand;
pub struct RewindCommand;
pub struct StatsCommand;
pub struct FilesCommand;
pub struct RenameCommand;
pub struct EffortCommand;
pub struct SummaryCommand;
pub struct CommitCommand;

// ---- /skills -------------------------------------------------------------

#[async_trait]
impl SlashCommand for SkillsCommand {
    fn name(&self) -> &str {
        "skills"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["skill"]
    }
    fn description(&self) -> &str {
        "List available skills in .mikmik/commands/"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        // `commands/` holds prompt templates the Skill tool loads by name.
        // They are not slash commands: nothing in the slash router reads this
        // directory, so the list names them without a leading slash.
        let mut found: Vec<String> = Vec::new();
        let dirs = [
            ctx.working_dir.join(".mikmik").join("commands"),
            mikmik_core::config::Settings::config_dir().join("commands"),
        ];

        for dir in &dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().is_some_and(|e| e == "md") {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            let name = stem.to_string();
                            if !found.contains(&name) {
                                found.push(name);
                            }
                        }
                    }
                }
            }
        }

        // Skills a plugin contributed arrive through discovery below: the
        // session adds each plugin's `skills/` directory to `config.skills`,
        // so they are listed by the same route that can run them.
        //
        // Include discovered skills from .mikmik/skills/ and configured paths/URLs.
        let discovered = mikmik_core::discover_skills(&ctx.working_dir, &ctx.config.skills);

        let mut output = if found.is_empty() && discovered.is_empty() {
            return CommandResult::Message(
                "No skills found.\nCreate .md files in .mikmik/commands/ to define skills.\n\
                 Example: .mikmik/commands/review.md"
                    .to_string(),
            );
        } else if found.is_empty() {
            String::new()
        } else {
            found.sort();
            format!(
                "Skills the model can load ({}):\n{}",
                found.len(),
                found
                    .iter()
                    .map(|s| format!("  {}", s))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };

        if !discovered.is_empty() {
            let mut disc_list: Vec<&mikmik_core::ResolvedSkill> = discovered.iter().collect();
            disc_list.sort_by(|a, b| a.command_name.cmp(&b.command_name));

            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("\nSkills you can type ({}):\n", disc_list.len()));
            for resolved in disc_list {
                output.push_str(&format!(
                    "  /{}{} — {} ({})\n",
                    resolved.command_name,
                    shadow_note(&resolved.command_name, ctx),
                    resolved.tagged_description(),
                    resolved.skill.source_path.display()
                ));
            }
        }

        CommandResult::Message(output.trim_end().to_string())
    }
}

/// Say what answers a skill's name instead of the skill.
///
/// `execute_command` resolves a built-in command first and a command defined
/// in settings second, so a skill that shares either name can never run.
/// Returning the winner here is what keeps the list from promising a command
/// that does nothing.
fn shadow_note(name: &str, ctx: &CommandContext) -> String {
    if crate::find_command(name).is_some() {
        return " (shadowed by the built-in command)".to_string();
    }
    if ctx.config.commands.contains_key(name) {
        return " (shadowed by a command in settings)".to_string();
    }
    String::new()
}

// ---- /rewind -------------------------------------------------------------

#[async_trait]
impl SlashCommand for RewindCommand {
    fn name(&self) -> &str {
        "rewind"
    }
    fn description(&self) -> &str {
        "Interactively select a message to rewind to"
    }
    fn help(&self) -> &str {
        "Usage: /rewind [n]\n\
         With no argument on a terminal, opens an overlay to select the message\n\
         to rewind to: ↑↓ to navigate, Enter to select, y/n to confirm.\n\
         Elsewhere it lists the messages, and /rewind <n> keeps the first n."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        if ctx.messages.is_empty() {
            return CommandResult::Message(
                "Nothing to rewind — conversation is empty.".to_string(),
            );
        }

        let args = args.trim();
        if !args.is_empty() {
            return match args.parse::<usize>() {
                Ok(keep) if keep <= ctx.messages.len() => {
                    CommandResult::SetMessages(ctx.messages[..keep].to_vec())
                }
                Ok(keep) => CommandResult::Error(format!(
                    "The conversation has {} messages, so {keep} cannot be kept.",
                    ctx.messages.len()
                )),
                Err(_) => CommandResult::Error(format!(
                    "\"{args}\" is not a message count. Run /rewind to see them numbered."
                )),
            };
        }

        if ctx.interactive {
            return CommandResult::OpenRewindOverlay;
        }
        CommandResult::Message(rewind_listing(&ctx.messages))
    }
}

/// The messages numbered, so a caller with no overlay can name one.
///
/// The number is how many messages would be kept, which is what `/rewind <n>`
/// takes: rewinding to a message means keeping everything before it.
fn rewind_listing(messages: &[mikmik_core::types::Message]) -> String {
    let mut out = String::from("Run /rewind <n> to keep the first n messages.\n");
    for (index, message) in messages.iter().enumerate() {
        let who = match message.role {
            mikmik_core::types::Role::User => "user",
            mikmik_core::types::Role::Assistant => "agent",
        };
        out.push_str(&format!(
            "\n{:>3}  {who:<6} {}",
            index,
            message_preview(message)
        ));
    }
    out
}

/// How long a message preview runs before it is cut short.
const PREVIEW_CHARS: usize = 70;

/// The first line of a message, short enough to sit in a list.
fn message_preview(message: &mikmik_core::types::Message) -> String {
    let text = message.get_all_text();
    let Some(line) = text.lines().find(|l| !l.trim().is_empty()) else {
        // A turn made only of tool calls has no text to show.
        let calls = message.get_tool_use_blocks().len();
        return match calls {
            0 => "(no text)".to_string(),
            1 => "(1 tool call)".to_string(),
            n => format!("({n} tool calls)"),
        };
    };
    let line = line.trim();
    let mut preview: String = line.chars().take(PREVIEW_CHARS).collect();
    if line.chars().count() > PREVIEW_CHARS {
        preview.push('…');
    }
    preview
}

// ---- /stats --------------------------------------------------------------

#[async_trait]
impl SlashCommand for StatsCommand {
    fn name(&self) -> &str {
        "stats"
    }
    fn description(&self) -> &str {
        "Show token usage and cost statistics"
    }
    fn help(&self) -> &str {
        "Usage: /stats\n\n\
         Shows detailed token usage and cost breakdown for the current session,\n\
         including cache creation/read token counts, turn counts, and session duration.\n\
         Use /usage for quota and account info. Use /cost for a quick cost summary."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let input = ctx.cost_tracker.input_tokens();
        let output = ctx.cost_tracker.output_tokens();
        let cache_creation = ctx.cost_tracker.cache_creation_tokens();
        let cache_read = ctx.cost_tracker.cache_read_tokens();
        let total = ctx.cost_tracker.total_tokens();
        let cost = ctx.cost_tracker.total_cost_usd();
        let model = ctx.config.effective_model();

        // Count user/assistant turns separately.
        let user_turns = ctx
            .messages
            .iter()
            .filter(|m| m.role == mikmik_core::types::Role::User)
            .count();
        let assistant_turns = ctx
            .messages
            .iter()
            .filter(|m| m.role == mikmik_core::types::Role::Assistant)
            .count();

        // Count tool-use invocations.
        let tool_calls: usize = ctx
            .messages
            .iter()
            .map(|m| m.get_tool_use_blocks().len())
            .sum();

        // Cost breakdown note: cache-read tokens are cheaper than input, and
        // cache-creation tokens are slightly more expensive. Provide a note if
        // caching is active.
        let cache_note = if cache_creation > 0 || cache_read > 0 {
            format!(
                "\n  (Cache write: {:>10}    Cache read: {:>10})",
                cache_creation, cache_read
            )
        } else {
            String::new()
        };

        let by_model = crate::stats::by_model_block(&ctx.cost_tracker);

        CommandResult::Message(format!(
            "Session Statistics\n\
             ══════════════════\n\
             Model:          {model}\n\
             \n\
             Conversation:\n\
               User turns:     {user_turns:>10}\n\
               Assistant turns:{assistant_turns:>10}\n\
               Tool calls:     {tool_calls:>10}\n\
             \n\
             Token usage:\n\
               Input:          {input:>10}\n\
               Output:         {output:>10}\n\
               Total:          {total:>10}{cache_note}\n\
             \n\
             Estimated cost:   ${cost:.4}\n\
             {by_model}\n\
             Use /usage for quota info · /cost for quick cost · /extra-usage for per-call breakdown",
            model = model,
            by_model = by_model,
            user_turns = user_turns,
            assistant_turns = assistant_turns,
            tool_calls = tool_calls,
            input = input,
            output = output,
            total = total,
            cache_note = cache_note,
            cost = cost,
        ))
    }
}

// ---- /files --------------------------------------------------------------

#[async_trait]
impl SlashCommand for FilesCommand {
    fn name(&self) -> &str {
        "files"
    }
    fn description(&self) -> &str {
        "List files referenced in the current conversation"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        use std::collections::HashSet;
        // Scan message content for file paths (simple heuristic)
        let mut files: HashSet<String> = HashSet::new();
        let path_re =
            regex::Regex::new(r#"(?m)([A-Za-z]:[\\/][^\s,;:"'<>]+|/[^\s,;:"'<>]{3,})"#).ok();

        for msg in &ctx.messages {
            let text = msg.get_all_text();
            if let Some(ref re) = path_re {
                for cap in re.captures_iter(&text) {
                    let path = cap[1].trim().to_string();
                    if std::path::Path::new(&path).exists() {
                        files.insert(path);
                    }
                }
            }
        }

        if files.is_empty() {
            return CommandResult::Message(
                "No referenced files detected in the conversation.".to_string(),
            );
        }

        let mut sorted: Vec<String> = files.into_iter().collect();
        sorted.sort();

        CommandResult::Message(format!(
            "Referenced files ({}):\n{}",
            sorted.len(),
            sorted
                .iter()
                .map(|f| format!("  {}", f))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

// ---- /rename -------------------------------------------------------------

#[async_trait]
impl SlashCommand for RenameCommand {
    fn name(&self) -> &str {
        "rename"
    }
    fn description(&self) -> &str {
        "Rename the current session"
    }
    fn help(&self) -> &str {
        "Usage: /rename [new name]\n\n\
         With a name: sets the session title immediately.\n\
         With no argument: auto-generates a kebab-case name from the conversation.\n\n\
         Examples:\n\
           /rename fix-login-bug\n\
           /rename              — auto-generate from conversation history"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let name = args.trim();

        if !name.is_empty() {
            // Explicit name provided: rename immediately.
            return CommandResult::RenameSession(name.to_string());
        }

        // No name given — auto-generate from conversation context.
        if ctx.messages.is_empty() {
            return CommandResult::Error(
                "No conversation context yet. Usage: /rename <name>".to_string(),
            );
        }

        // Build a short conversation excerpt (up to ~2000 chars) for the model.
        let excerpt: String = ctx
            .messages
            .iter()
            .take(20)
            .filter_map(|m| {
                let text = m.get_all_text();
                if text.is_empty() {
                    return None;
                }
                let role = match m.role {
                    mikmik_core::types::Role::User => "User",
                    mikmik_core::types::Role::Assistant => "Assistant",
                };
                Some(format!(
                    "{}: {}",
                    role,
                    text.chars().take(300).collect::<String>()
                ))
            })
            .collect::<Vec<_>>()
            .join("\n");

        if excerpt.is_empty() {
            return CommandResult::Error(
                "No text content in conversation. Usage: /rename <name>".to_string(),
            );
        }

        // The account comes from the route, not from `selected_provider_id`:
        // the two disagree whenever the chosen model carries a prefix, and
        // `provider_for_config` took the second while the model id came from
        // the first, so the request went to one account addressed at another's
        // model.
        let rename_route = resolve_fast_model_route(&ctx.config);
        let provider =
            match mikmik_api::provider_for_account(&ctx.config, &rename_route.account).await {
                Ok(provider) => provider,
                Err(e) => {
                    return CommandResult::Error(format!(
                        "Could not create a provider client for auto-naming: {e}.\n\
                     Use /rename <name> to set the name manually."
                    ));
                }
            };

        let system_prompt = "Generate a short kebab-case name (2-4 words) that captures the \
            main topic of this conversation. Use lowercase words separated by hyphens. \
            Examples: fix-login-bug, add-auth-feature, refactor-api-client. \
            Respond with ONLY the name, nothing else.";

        let request = mikmik_api::ProviderRequest {
            model: rename_route.model.clone(),
            messages: vec![Message::user(format!(
                "Conversation to name:\n\n{}",
                &excerpt[..excerpt.len().min(2000)]
            ))],
            system_prompt: Some(mikmik_api::SystemPrompt::Text(system_prompt.to_string())),
            tools: vec![],
            max_tokens: 64,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            provider_options: serde_json::Value::Object(Default::default()),
        };

        match provider.create_message(request).await {
            Ok(response) => {
                let raw_text = text_from_content_blocks(&response.content)
                    .trim()
                    .to_string();

                let generated = raw_text
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '-')
                    .collect::<String>();

                // Trim leading/trailing hyphens and ensure non-empty.
                let cleaned = generated.trim_matches('-').to_string();
                if cleaned.is_empty() {
                    return CommandResult::Error(
                        "Could not generate a valid name from conversation. \
                         Use /rename <name> to set manually."
                            .to_string(),
                    );
                }

                CommandResult::RenameSession(cleaned)
            }
            Err(e) => CommandResult::Error(format!(
                "Auto-name generation failed: {e}\n\
                 Use /rename <name> to set the name manually."
            )),
        }
    }
}

// ---- /effort -------------------------------------------------------------

#[async_trait]
impl SlashCommand for EffortCommand {
    fn name(&self) -> &str {
        "effort"
    }
    fn description(&self) -> &str {
        "Set the model's reasoning effort"
    }
    fn help(&self) -> &str {
        "Usage: /effort [none|minimal|low|medium|high|xhigh|max|ultracode]\n\
         Sets how much reasoning the model spends before answering.\n\n\
         `low`, `normal` and `high` also set the output limit, to 4096, the\n\
         default, and 32768 tokens. The other levels leave it alone."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();
        if args.is_empty() {
            // Read from the context rather than naming a level here. This line
            // used to say "normal" whatever the session was actually doing,
            // and a remote client has no picker to check it against.
            let current = match ctx.effort_level {
                Some(level) => level.as_str(),
                None => "unset (the model's own default)",
            };
            return CommandResult::Message(format!(
                "Current effort: {current}\nUse /effort [none|minimal|low|medium|high|xhigh|max|ultracode] to change."
            ));
        }

        let Some(level) = mikmik_core::effort::EffortLevel::from_str(args) else {
            return CommandResult::Error(format!(
                "Unknown effort level '{args}'. Use: none | minimal | low | medium | high | xhigh | max | ultracode"
            ));
        };

        // The output limit rides along for the three original words only.
        // Widening it to the whole ladder would silently change the limit for
        // levels that never touched it.
        match args.to_ascii_lowercase().as_str() {
            "low" => ctx.config.max_tokens = Some(4096),
            "normal" => ctx.config.max_tokens = None,
            "high" => ctx.config.max_tokens = Some(32768),
            _ => {}
        }

        ctx.effort_level = Some(level);
        CommandResult::ConfigChange(ctx.config.clone())
    }
}

// ---- /summary ------------------------------------------------------------

#[async_trait]
impl SlashCommand for SummaryCommand {
    fn name(&self) -> &str {
        "summary"
    }
    fn description(&self) -> &str {
        "Generate a brief summary of the conversation so far"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let count = ctx.messages.len();
        if count == 0 {
            return CommandResult::Message("No messages in conversation yet.".to_string());
        }

        // Ask the model to summarize by injecting a hidden user message
        CommandResult::UserMessage(
            "Please provide a brief (3-5 sentence) summary of our conversation so far, \
             focusing on what has been accomplished and the current state."
                .to_string(),
        )
    }
}

// ---- /commit -------------------------------------------------------------

#[async_trait]
impl SlashCommand for CommitCommand {
    fn name(&self) -> &str {
        "commit"
    }
    fn description(&self) -> &str {
        "Ask MikMik to commit the work in this repository"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let extra = if args.trim().is_empty() {
            String::new()
        } else {
            format!(" with message: {}", args.trim())
        };

        // `git diff HEAD` rather than `git diff --cached`, because this command
        // used to see only what the user had already staged and answered an
        // empty diff when nothing was. Splitting is asked for here rather than
        // computed, because the model reads the diff and the person who made
        // the change is the one who knows which parts belong together.
        CommandResult::UserMessage(format!(
            "Please commit the work in this repository{}. \
             Run `git status` and `git diff HEAD` to see everything that changed, \
             staged or not. Split unrelated changes into separate commits and \
             stage each group by explicit path. Read `git log` for the \
             repository's existing commit message conventions.",
            extra
        ))
    }
}

#[cfg(test)]
mod commit_command_tests {
    use super::*;

    fn ctx() -> CommandContext {
        CommandContext {
            context_window: 200_000,
            context_used_tokens: 0,
            config: mikmik_core::Config::default(),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("."),
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

    async fn prompt(args: &str) -> String {
        match CommitCommand.execute(args, &mut ctx()).await {
            CommandResult::UserMessage(text) => text,
            other => panic!("expected a UserMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_whole_working_tree_is_in_scope() {
        // The command used to name `git diff --cached`, so a user who had
        // staged nothing sent the model an empty diff.
        let text = prompt("").await;
        assert!(text.contains("git diff HEAD"), "{text}");
        assert!(text.contains("git status"), "{text}");
        assert!(
            !text.contains("--cached"),
            "the prompt still scopes the diff to the index: {text}"
        );
    }

    #[tokio::test]
    async fn unrelated_changes_are_asked_to_be_split() {
        let text = prompt("").await;
        assert!(text.contains("Split unrelated changes"), "{text}");
        assert!(text.contains("stage each group by explicit path"), "{text}");
    }

    #[tokio::test]
    async fn an_argument_reaches_the_model_as_the_requested_message() {
        let text = prompt("  fix the parser  ").await;
        assert!(text.contains("with message: fix the parser"), "{text}");
    }
}
