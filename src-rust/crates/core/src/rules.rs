//! Conditional rules: instructions that wait for the model to break them.
//!
//! A rule lives in the same directories as memory files. Without a `condition`
//! it is ordinary memory and reaches the prompt on every turn. With one it
//! leaves the prompt and waits: when the model writes something the condition
//! matches, the rule is put in front of it, at that moment and only then.
//!
//! What a rule matches is the text a tool call would introduce, not the whole
//! argument object. An `Edit` that **removes** a `.unwrap()` must not trip the
//! rule that forbids writing one, and it would if `old_string` were matched.

use crate::claudemd::{is_conditional_rule, MemoryFileInfo, MemoryFilenames};
use regex::Regex;
use std::path::{Path, PathBuf};

/// What happens when a rule matches a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    /// Run the call, then put the rule on top of its result.
    Remind,
    /// Refuse the call and answer with the rule instead.
    Block,
}

/// How often a rule may speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatPolicy {
    /// Once per session. The default: a rule that repeats every turn becomes
    /// noise, and noise is ignored.
    Once,
    /// Every time it matches.
    Always,
    /// Again after this many turns.
    AfterTurns(u32),
}

/// Which tools a rule watches.
#[derive(Debug, Clone)]
pub enum ToolSelector {
    /// Every tool whose payload is known.
    Any,
    /// Named tools, each with an optional file-path pattern.
    Named(Vec<(String, Option<glob::Pattern>)>),
}

/// The streams a rule watches.
#[derive(Debug, Clone)]
pub struct RuleScope {
    pub text: bool,
    pub thinking: bool,
    pub tools: Option<ToolSelector>,
}

impl Default for RuleScope {
    /// Tools, and not prose.
    ///
    /// A rule about code is a rule about what gets written, and matching the
    /// model's prose as well would fire on it merely discussing the thing.
    fn default() -> Self {
        Self {
            text: false,
            thinking: false,
            tools: Some(ToolSelector::Any),
        }
    }
}

/// One conditional rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// The file stem. Two rules of the same name are the same rule, and the
    /// later scope wins.
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    /// The body the model is shown.
    pub content: String,
    pub conditions: Vec<Regex>,
    pub scope: RuleScope,
    pub globs: Vec<glob::Pattern>,
    pub action: RuleAction,
    pub repeat: RepeatPolicy,
}

/// The text one tool call would introduce, and the file it goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub path: Option<String>,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Glob helpers
// ---------------------------------------------------------------------------

/// Expand `{a,b}` alternation into separate patterns.
///
/// The `glob` crate has no brace alternation, and a rule that names
/// `*.{ts,tsx}` would otherwise match a file called exactly that.
pub fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else {
        return vec![pattern.to_string()];
    };

    let head = &pattern[..open];
    let tail = &pattern[close + 1..];
    let mut out = Vec::new();
    for choice in pattern[open + 1..close].split(',') {
        // The tail may hold another group, so each result is expanded again.
        for expanded in expand_braces(&format!("{head}{}{tail}", choice.trim())) {
            out.push(expanded);
        }
    }
    out
}

/// Compile one path pattern, brace groups included.
fn compile_globs(pattern: &str) -> Vec<glob::Pattern> {
    expand_braces(pattern)
        .into_iter()
        .filter_map(|p| match glob::Pattern::new(&p) {
            Ok(compiled) => Some(compiled),
            Err(e) => {
                tracing::warn!("rule glob '{p}' is not a pattern: {e}");
                None
            }
        })
        .collect()
}

/// Does `path` match any of `patterns`?
///
/// The whole path and the file name are both tried, so `*.rs` matches
/// `crates/core/src/lib.rs` without the author having to write `**/*.rs`.
fn path_matches(patterns: &[glob::Pattern], path: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    patterns
        .iter()
        .any(|p| p.matches(path) || (!name.is_empty() && p.matches(&name)))
}

// ---------------------------------------------------------------------------
// Scope parsing
// ---------------------------------------------------------------------------

/// Read a scope line such as `tool:Edit(*.rs), tool:Write(*.rs)`.
///
/// Tokens: `text`, `thinking`, `tool`, `tool:Name`, `tool:Name(glob)`. A bare
/// name with no `tool:` prefix is a tool name too, which is the spelling most
/// rule files use.
pub fn parse_scope(scope: &str) -> RuleScope {
    let mut result = RuleScope {
        text: false,
        thinking: false,
        tools: None,
    };
    let mut named: Vec<(String, Option<glob::Pattern>)> = Vec::new();
    let mut any_tool = false;

    for token in scope.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if token.eq_ignore_ascii_case("text") {
            result.text = true;
            continue;
        }
        if token.eq_ignore_ascii_case("thinking") {
            result.thinking = true;
            continue;
        }

        // `tool:Name(glob)`, `Name(glob)`, `tool`, `Name`.
        let (head, path_pattern) = match token.split_once('(') {
            Some((head, rest)) => (head.trim(), rest.trim_end_matches(')').trim()),
            None => (token, ""),
        };
        let name = head
            .strip_prefix("tool:")
            .or_else(|| head.strip_prefix("toolcall:"))
            .unwrap_or(head)
            .trim();

        if name.is_empty() || name.eq_ignore_ascii_case("tool") {
            any_tool = true;
            continue;
        }
        if path_pattern.is_empty() {
            named.push((name.to_string(), None));
            continue;
        }
        for compiled in compile_globs(path_pattern) {
            named.push((name.to_string(), Some(compiled)));
        }
    }

    result.tools = if any_tool {
        Some(ToolSelector::Any)
    } else if named.is_empty() {
        None
    } else {
        Some(ToolSelector::Named(named))
    };

    // A scope naming nothing at all is the default rather than a rule that can
    // never match, because a typo would otherwise silence the rule in silence.
    if !result.text && !result.thinking && result.tools.is_none() {
        return RuleScope::default();
    }
    result
}

// ---------------------------------------------------------------------------
// Tool payloads
// ---------------------------------------------------------------------------

/// Collect every string in `value`, deepest first.
fn all_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| all_strings(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| all_strings(v, out)),
        _ => {}
    }
}

/// The lines a unified diff adds.
fn patch_additions(patch: &str) -> String {
    patch
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .map(|line| &line[1..])
        .collect::<Vec<_>>()
        .join("\n")
}

/// The file paths a unified diff writes to.
fn patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| line.strip_prefix("+++ "))
        .map(|path| path.trim().trim_start_matches("b/").to_string())
        .filter(|path| path != "/dev/null")
        .collect()
}

fn string_field(input: &serde_json::Value, key: &str) -> Option<String> {
    input.get(key)?.as_str().map(str::to_string)
}

/// What `tool` would write, split per file.
///
/// Only the fields that carry new content. Matching the whole argument object
/// would make an `Edit` that removes a forbidden call trip the rule against it.
///
/// `named_scope` is true when a rule asked for this tool by name. A tool this
/// function has no mapping for then falls back to all of its string arguments,
/// which is coarse but was asked for explicitly.
pub fn tool_payloads(tool: &str, input: &serde_json::Value, named_scope: bool) -> Vec<Payload> {
    use crate::constants::*;

    let one = |path: Option<String>, text: Option<String>| -> Vec<Payload> {
        match text {
            Some(text) if !text.is_empty() => vec![Payload { path, text }],
            _ => Vec::new(),
        }
    };

    match tool {
        TOOL_NAME_FILE_WRITE => one(
            string_field(input, "file_path"),
            string_field(input, "content"),
        ),
        TOOL_NAME_FILE_EDIT => one(
            string_field(input, "file_path"),
            string_field(input, "new_string"),
        ),
        TOOL_NAME_NOTEBOOK_EDIT => one(
            string_field(input, "notebook_path"),
            string_field(input, "new_source"),
        ),
        TOOL_NAME_BASH => one(None, string_field(input, "command")),
        TOOL_NAME_BATCH_EDIT => input
            .get("edits")
            .and_then(|e| e.as_array())
            .map(|edits| {
                edits
                    .iter()
                    .filter_map(|edit| {
                        Some(Payload {
                            path: string_field(edit, "file_path"),
                            text: string_field(edit, "new_string")?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        TOOL_NAME_APPLY_PATCH => {
            let Some(patch) = string_field(input, "patch") else {
                return Vec::new();
            };
            let added = patch_additions(&patch);
            if added.is_empty() {
                return Vec::new();
            }
            let paths = patch_paths(&patch);
            if paths.is_empty() {
                return vec![Payload {
                    path: None,
                    text: added,
                }];
            }
            // The same added text under each path the patch touches. Splitting
            // a diff per file exactly would need a hunk parser, and the gate a
            // path serves here is which rules apply.
            paths
                .into_iter()
                .map(|path| Payload {
                    path: Some(path),
                    text: added.clone(),
                })
                .collect()
        }
        _ if named_scope => {
            let mut strings = Vec::new();
            all_strings(input, &mut strings);
            one(None, Some(strings.join("\n")))
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The rule set
// ---------------------------------------------------------------------------

/// Every rule a session may fire.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

/// Turn one loaded file into a rule.
///
/// Returns `None` when nothing in it compiles: a rule with no usable condition
/// can never match, and keeping it would only make `rules list` lie.
fn rule_from_file(file: &MemoryFileInfo) -> Option<Rule> {
    let name = file.path.file_stem()?.to_string_lossy().into_owned();
    let mut conditions = Vec::new();
    for pattern in &file.frontmatter.condition {
        match Regex::new(pattern) {
            Ok(compiled) => conditions.push(compiled),
            // A bad pattern is the rule author's mistake, not the session's.
            // It is reported and skipped, and the session starts.
            Err(e) => tracing::warn!("rule '{name}' has an unusable condition '{pattern}': {e}"),
        }
    }
    if conditions.is_empty() {
        return None;
    }

    let action = match file.frontmatter.on_match.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("block") => RuleAction::Block,
        Some(value) if !value.eq_ignore_ascii_case("remind") => {
            tracing::warn!("rule '{name}' has an unknown on_match '{value}'; reminding instead");
            RuleAction::Remind
        }
        _ => RuleAction::Remind,
    };

    let repeat = match file.frontmatter.repeat.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("always") => RepeatPolicy::Always,
        Some(value) if value.eq_ignore_ascii_case("once") => RepeatPolicy::Once,
        Some(value) => match value.parse::<u32>() {
            Ok(turns) => RepeatPolicy::AfterTurns(turns),
            Err(_) => {
                tracing::warn!("rule '{name}' has an unknown repeat '{value}'; using once");
                RepeatPolicy::Once
            }
        },
        None => RepeatPolicy::Once,
    };

    Some(Rule {
        name,
        path: file.path.clone(),
        description: file.frontmatter.description.clone(),
        content: file.content.trim().to_string(),
        conditions,
        scope: file
            .frontmatter
            .scope
            .as_deref()
            .map(parse_scope)
            .unwrap_or_default(),
        globs: file
            .frontmatter
            .globs
            .iter()
            .flat_map(|g| compile_globs(g))
            .collect(),
        action,
        repeat,
    })
}

impl RuleSet {
    /// The rules a project's directories describe.
    ///
    /// A later file of the same name replaces an earlier one, so a project may
    /// restate a rule the user set globally, and the user may restate a
    /// built-in one.
    pub fn load(
        project_root: &Path,
        filenames: MemoryFilenames,
        builtin: bool,
        disabled: &[String],
    ) -> Self {
        let mut set = Self::default();
        if builtin {
            for rule in builtin_rules() {
                set.insert(rule.clone());
            }
        }
        let files = crate::claudemd::load_all_memory_files(project_root, filenames);
        for file in files.iter().filter(|f| is_conditional_rule(f)) {
            if let Some(rule) = rule_from_file(file) {
                set.insert(rule);
            }
        }
        set.rules
            .retain(|rule| !disabled.iter().any(|name| name == &rule.name));
        set
    }

    /// Add a rule, replacing one of the same name.
    pub fn insert(&mut self, rule: Rule) {
        match self.rules.iter_mut().find(|r| r.name == rule.name) {
            Some(existing) => *existing = rule,
            None => self.rules.push(rule),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter()
    }

    /// Does this rule watch `tool`, and does it allow `path`?
    fn watches(rule: &Rule, tool: &str, path: Option<&str>) -> bool {
        let Some(selector) = &rule.scope.tools else {
            return false;
        };
        let scope_ok = match selector {
            ToolSelector::Any => true,
            ToolSelector::Named(named) => named.iter().any(|(name, pattern)| {
                if !name.eq_ignore_ascii_case(tool) {
                    return false;
                }
                match (pattern, path) {
                    (None, _) => true,
                    (Some(pattern), Some(path)) => {
                        path_matches(std::slice::from_ref(pattern), path)
                    }
                    // A pattern with no path to test cannot be satisfied.
                    (Some(_), None) => false,
                }
            }),
        };
        if !scope_ok {
            return false;
        }
        match path {
            Some(path) => path_matches(&rule.globs, path),
            // A rule with globs needs a path to judge.
            None => rule.globs.is_empty(),
        }
    }

    /// Whether any rule asks for this tool by name.
    ///
    /// A tool with no payload mapping is only read when a rule named it, so
    /// this decides whether to fall back to its raw arguments.
    fn named_by_any(&self, tool: &str) -> bool {
        self.rules.iter().any(|rule| match &rule.scope.tools {
            Some(ToolSelector::Named(named)) => named
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(tool)),
            _ => false,
        })
    }

    /// The rules one tool call breaks.
    pub fn match_tool(&self, tool: &str, input: &serde_json::Value) -> Vec<&Rule> {
        if self.rules.is_empty() {
            return Vec::new();
        }
        let payloads = tool_payloads(tool, input, self.named_by_any(tool));
        if payloads.is_empty() {
            return Vec::new();
        }
        self.rules
            .iter()
            .filter(|rule| {
                payloads.iter().any(|payload| {
                    Self::watches(rule, tool, payload.path.as_deref())
                        && rule.conditions.iter().any(|c| c.is_match(&payload.text))
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The catalogue that ships with the binary
// ---------------------------------------------------------------------------

/// The rules embedded in the binary, by name.
///
/// Named one by one rather than scanned from a directory, because the files
/// have to be in the binary and a directory scan at runtime would look for
/// them on a machine that never had them. `NOTICE.md` is deliberately absent:
/// it is the third-party notice, not a rule.
const BUILTIN_RULES: &[(&str, &str)] = &[
    (
        "git-add-all",
        include_str!("../assets/rules/git-add-all.md"),
    ),
    (
        "git-destructive",
        include_str!("../assets/rules/git-destructive.md"),
    ),
    (
        "go-add-cleanup",
        include_str!("../assets/rules/go-add-cleanup.md"),
    ),
    (
        "go-exp-promoted",
        include_str!("../assets/rules/go-exp-promoted.md"),
    ),
    ("go-ioutil", include_str!("../assets/rules/go-ioutil.md")),
    (
        "go-join-hostport",
        include_str!("../assets/rules/go-join-hostport.md"),
    ),
    ("go-rand-v2", include_str!("../assets/rules/go-rand-v2.md")),
    ("no-secrets", include_str!("../assets/rules/no-secrets.md")),
    (
        "rs-box-leak",
        include_str!("../assets/rules/rs-box-leak.md"),
    ),
    (
        "rs-future-prelude",
        include_str!("../assets/rules/rs-future-prelude.md"),
    ),
    (
        "rs-lazylock",
        include_str!("../assets/rules/rs-lazylock.md"),
    ),
    (
        "rs-match-ergonomics",
        include_str!("../assets/rules/rs-match-ergonomics.md"),
    ),
    (
        "rs-no-unwrap",
        include_str!("../assets/rules/rs-no-unwrap.md"),
    ),
    (
        "rs-parking-lot",
        include_str!("../assets/rules/rs-parking-lot.md"),
    ),
    (
        "rs-result-type",
        include_str!("../assets/rules/rs-result-type.md"),
    ),
    (
        "rs-unsafe-safety",
        include_str!("../assets/rules/rs-unsafe-safety.md"),
    ),
    (
        "sql-parameterize",
        include_str!("../assets/rules/sql-parameterize.md"),
    ),
    (
        "ts-bare-catch",
        include_str!("../assets/rules/ts-bare-catch.md"),
    ),
    (
        "ts-import-type",
        include_str!("../assets/rules/ts-import-type.md"),
    ),
    ("ts-no-any", include_str!("../assets/rules/ts-no-any.md")),
    (
        "ts-no-deprecated-leftovers",
        include_str!("../assets/rules/ts-no-deprecated-leftovers.md"),
    ),
    (
        "ts-no-dynamic-import",
        include_str!("../assets/rules/ts-no-dynamic-import.md"),
    ),
    (
        "ts-no-local-is-record",
        include_str!("../assets/rules/ts-no-local-is-record.md"),
    ),
    (
        "ts-no-return-type",
        include_str!("../assets/rules/ts-no-return-type.md"),
    ),
    (
        "ts-no-test-timers",
        include_str!("../assets/rules/ts-no-test-timers.md"),
    ),
    (
        "ts-no-tiny-functions",
        include_str!("../assets/rules/ts-no-tiny-functions.md"),
    ),
    (
        "ts-promise-with-resolvers",
        include_str!("../assets/rules/ts-promise-with-resolvers.md"),
    ),
    ("ts-set-map", include_str!("../assets/rules/ts-set-map.md")),
    (
        "web-no-localstorage",
        include_str!("../assets/rules/web-no-localstorage.md"),
    ),
];

/// Every rule that ships with the binary.
///
/// A file that fails to parse here is a build-time mistake rather than a
/// user's, so it is logged and skipped and the session still starts.
pub fn builtin_rules() -> &'static [Rule] {
    static RULES: once_cell::sync::Lazy<Vec<Rule>> = once_cell::sync::Lazy::new(|| {
        BUILTIN_RULES
            .iter()
            .filter_map(|(name, text)| {
                let (frontmatter, body) = crate::claudemd::parse_frontmatter(text);
                let file = MemoryFileInfo {
                    path: PathBuf::from(format!("<built-in>/{name}.md")),
                    scope: crate::claudemd::MemoryScope::Managed,
                    content: body.to_string(),
                    frontmatter,
                    mtime: None,
                };
                let rule = rule_from_file(&file);
                if rule.is_none() {
                    tracing::error!("built-in rule '{name}' has no usable condition");
                }
                rule
            })
            .collect()
    });
    &RULES
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// The rule set each project root produced, so the directories are read once.
static LOADED: once_cell::sync::Lazy<
    parking_lot::RwLock<std::collections::HashMap<PathBuf, std::sync::Arc<RuleSet>>>,
> = once_cell::sync::Lazy::new(Default::default);

/// Which turn each rule last spoke on, per session.
static FIRED: once_cell::sync::Lazy<
    parking_lot::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, u64>>>,
> = once_cell::sync::Lazy::new(Default::default);

/// The rules for `project_root`, reading the directories on the first call.
pub fn rules_for(
    project_root: &Path,
    filenames: MemoryFilenames,
    builtin: bool,
    disabled: &[String],
) -> std::sync::Arc<RuleSet> {
    if let Some(found) = LOADED.read().get(project_root) {
        return found.clone();
    }
    // Loaded outside the lock: this reads a directory tree, and holding a
    // global write lock across that would stall every other tool call.
    let loaded = std::sync::Arc::new(RuleSet::load(project_root, filenames, builtin, disabled));
    LOADED
        .write()
        .entry(project_root.to_path_buf())
        .or_insert_with(|| loaded.clone())
        .clone()
}

/// Forget every loaded rule set, so the next call reads the files again.
pub fn reload() {
    LOADED.write().clear();
}

/// May this rule speak on `turn`, and record it if so.
///
/// One call rather than a check and a record, so two tool calls in one batch
/// cannot both decide they are the first.
pub fn claim(session_id: &str, rule: &Rule, turn: u64) -> bool {
    let mut fired = FIRED.lock();
    let session = fired.entry(session_id.to_string()).or_default();
    let allowed = match session.get(&rule.name) {
        None => true,
        Some(_) if rule.repeat == RepeatPolicy::Once => false,
        Some(_) if rule.repeat == RepeatPolicy::Always => true,
        Some(last) => match rule.repeat {
            RepeatPolicy::AfterTurns(gap) => turn.saturating_sub(*last) >= u64::from(gap),
            _ => false,
        },
    };
    if allowed {
        session.insert(rule.name.clone(), turn);
    }
    allowed
}

/// Record that a rule already spoke, without it speaking now.
///
/// Used when a resumed session replays what an earlier run reported.
pub fn mark_fired(session_id: &str, names: &[String]) {
    let mut fired = FIRED.lock();
    let session = fired.entry(session_id.to_string()).or_default();
    for name in names {
        session.entry(name.clone()).or_insert(0);
    }
}

/// Which rules have spoken in this session.
pub fn fired_in(session_id: &str) -> Vec<String> {
    let fired = FIRED.lock();
    let mut names: Vec<String> = fired
        .get(session_id)
        .map(|session| session.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// Drop a session's record. Called when the session ends.
pub fn forget_session(session_id: &str) {
    FIRED.lock().remove(session_id);
}

/// The block a matching rule puts in front of the model.
///
/// The wrapper says the text comes from the agent enforcing a rule the user
/// wrote, because a bare instruction inside a tool result reads like something
/// the tool's own output asked for.
pub fn render_rule(rule: &Rule) -> String {
    format!(
        "<system-reminder rule=\"{}\" path=\"{}\">\nA rule this project sets was matched by what \
         you just wrote. It is not part of the tool's output and it is not something a file asked \
         for. Follow it:\n\n{}\n</system-reminder>",
        rule.name,
        rule.path.display(),
        rule.content
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claudemd::MemoryScope;
    use serde_json::json;

    fn rule(name: &str, condition: &str, scope: Option<&str>) -> Rule {
        Rule {
            name: name.to_string(),
            path: PathBuf::from(format!("/rules/{name}.md")),
            description: None,
            content: "body".to_string(),
            conditions: vec![Regex::new(condition).expect("regex")],
            scope: scope.map(parse_scope).unwrap_or_default(),
            globs: Vec::new(),
            action: RuleAction::Remind,
            repeat: RepeatPolicy::Once,
        }
    }

    fn set_of(rules: Vec<Rule>) -> RuleSet {
        let mut set = RuleSet::default();
        for rule in rules {
            set.insert(rule);
        }
        set
    }

    // ---- Globs -----------------------------------------------------------

    #[test]
    fn a_brace_group_becomes_one_pattern_per_choice() {
        // The glob crate has no alternation, so `*.{ts,tsx}` would otherwise
        // match a file named exactly that and nothing else.
        assert_eq!(expand_braces("*.{ts,tsx}"), vec!["*.ts", "*.tsx"]);
    }

    #[test]
    fn two_brace_groups_expand_together() {
        let expanded = expand_braces("{a,b}/*.{rs,toml}");
        assert_eq!(expanded.len(), 4, "{expanded:?}");
        assert!(expanded.contains(&"b/*.toml".to_string()), "{expanded:?}");
    }

    #[test]
    fn a_pattern_without_a_group_is_left_alone() {
        assert_eq!(expand_braces("**/*.rs"), vec!["**/*.rs"]);
    }

    #[test]
    fn a_bare_extension_matches_a_nested_file() {
        // Nobody writes `**/*.rs` in a rule, and `*.rs` failing on
        // `src/lib.rs` would make every scoped rule silently dead.
        let patterns = compile_globs("*.rs");
        assert!(path_matches(&patterns, "crates/core/src/lib.rs"));
        assert!(!path_matches(&patterns, "crates/core/src/lib.ts"));
    }

    // ---- Scope -----------------------------------------------------------

    #[test]
    fn the_default_scope_watches_tools_and_not_prose() {
        let scope = RuleScope::default();
        assert!(!scope.text);
        assert!(!scope.thinking);
        assert!(matches!(scope.tools, Some(ToolSelector::Any)));
    }

    #[test]
    fn a_scope_names_one_tool_and_one_file_type() {
        let rules = set_of(vec![rule(
            "rs-only",
            "forbidden",
            Some("tool:Edit(*.rs), tool:Write(*.rs)"),
        )]);
        let hit = json!({ "file_path": "src/a.rs", "new_string": "forbidden" });
        let miss = json!({ "file_path": "src/a.ts", "new_string": "forbidden" });
        assert_eq!(rules.match_tool("Edit", &hit).len(), 1);
        assert!(rules.match_tool("Edit", &miss).is_empty());
    }

    #[test]
    fn a_lower_case_tool_name_still_matches() {
        // Rule files written for another agent spell the tool in lower case.
        let rules = set_of(vec![rule("x", "forbidden", Some("tool:edit(*.rs)"))]);
        let input = json!({ "file_path": "a.rs", "new_string": "forbidden" });
        assert_eq!(rules.match_tool("Edit", &input).len(), 1);
    }

    #[test]
    fn an_unreadable_scope_falls_back_to_the_default() {
        // Silently watching nothing would leave the author with a rule that
        // never fires and no way to tell.
        let scope = parse_scope("   ");
        assert!(matches!(scope.tools, Some(ToolSelector::Any)));
    }

    // ---- Payloads --------------------------------------------------------

    #[test]
    fn removing_a_forbidden_call_does_not_trip_the_rule_against_writing_one() {
        // The whole point of reading `new_string` alone.
        let rules = set_of(vec![rule("no-unwrap", r"\.unwrap\(\)", None)]);
        let removal = json!({
            "file_path": "a.rs",
            "old_string": "let x = y.unwrap();",
            "new_string": "let x = y?;"
        });
        assert!(rules.match_tool("Edit", &removal).is_empty());
    }

    #[test]
    fn a_batch_edit_is_read_edit_by_edit() {
        let rules = set_of(vec![rule("no-unwrap", r"\.unwrap\(\)", Some("Edit(*.rs)"))]);
        let mut set = RuleSet::default();
        for r in rules.iter().cloned() {
            set.insert(Rule {
                scope: parse_scope("BatchEdit(*.rs)"),
                ..r
            });
        }
        let input = json!({
            "edits": [
                { "file_path": "a.rs", "old_string": "a", "new_string": "b" },
                { "file_path": "b.rs", "old_string": "c", "new_string": "d.unwrap()" }
            ]
        });
        assert_eq!(set.match_tool("BatchEdit", &input).len(), 1);
    }

    #[test]
    fn a_patch_is_read_by_the_lines_it_adds() {
        let payloads = tool_payloads(
            "ApplyPatch",
            &json!({
                "patch": "--- a/x.rs\n+++ b/x.rs\n@@\n-let a = 1;\n+let b = c.unwrap();\n"
            }),
            false,
        );
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        assert_eq!(payloads[0].path.as_deref(), Some("x.rs"));
        assert!(payloads[0].text.contains(".unwrap()"));
        assert!(
            !payloads[0].text.contains("let a = 1"),
            "a removed line is not something the call writes"
        );
    }

    #[test]
    fn a_command_is_read_from_bash() {
        let rules = set_of(vec![rule("no-add-all", r"git add -A", Some("tool:Bash"))]);
        let input = json!({ "command": "git add -A" });
        assert_eq!(rules.match_tool("Bash", &input).len(), 1);
    }

    #[test]
    fn a_tool_with_no_payload_mapping_is_read_only_when_a_rule_names_it() {
        let named = set_of(vec![rule("x", "secret", Some("tool:WebFetch"))]);
        let unnamed = set_of(vec![rule("x", "secret", None)]);
        let input = json!({ "url": "https://example.com/secret" });
        assert_eq!(named.match_tool("WebFetch", &input).len(), 1);
        assert!(
            unnamed.match_tool("WebFetch", &input).is_empty(),
            "a rule that names no tool must not read every tool's arguments"
        );
    }

    // ---- The set ---------------------------------------------------------

    #[test]
    fn a_later_rule_of_the_same_name_replaces_the_earlier_one() {
        let mut set = RuleSet::default();
        set.insert(rule("same", "first", None));
        set.insert(rule("same", "second", None));
        assert_eq!(set.len(), 1);
        let input = json!({ "file_path": "a.rs", "new_string": "second" });
        assert_eq!(set.match_tool("Edit", &input).len(), 1);
    }

    #[test]
    fn a_rule_with_no_usable_condition_is_dropped() {
        let file = MemoryFileInfo {
            path: PathBuf::from("/rules/broken.md"),
            scope: MemoryScope::Managed,
            content: "body".to_string(),
            frontmatter: crate::claudemd::MemoryFrontmatter {
                condition: vec!["(unclosed".to_string()],
                ..Default::default()
            },
            mtime: None,
        };
        assert!(rule_from_file(&file).is_none());
    }

    // ---- The shipped catalogue -------------------------------------------

    #[test]
    fn every_shipped_rule_parses() {
        // A rule that does not compile is dropped, so without this the
        // catalogue could shrink silently between releases.
        assert_eq!(
            builtin_rules().len(),
            BUILTIN_RULES.len(),
            "a shipped rule was dropped: {:?}",
            BUILTIN_RULES
                .iter()
                .map(|(name, _)| *name)
                .filter(|name| !builtin_rules().iter().any(|r| &r.name == name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_shipped_rule_says_what_it_is_about() {
        for rule in builtin_rules() {
            assert!(
                rule.description.is_some(),
                "'{}' has no description, so a listing cannot explain it",
                rule.name
            );
            assert!(!rule.content.is_empty(), "'{}' has no body", rule.name);
        }
    }

    #[test]
    fn the_catalogue_names_each_rule_once() {
        let mut names: Vec<&str> = BUILTIN_RULES.iter().map(|(name, _)| *name).collect();
        names.sort();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "a name is listed twice");
    }

    #[test]
    fn a_shipped_rule_is_replaced_by_one_of_the_same_name() {
        // How a user overrides a rule they disagree with, rather than only
        // switching it off.
        let mut set = RuleSet::default();
        for rule in builtin_rules() {
            set.insert(rule.clone());
        }
        let before = set.len();
        set.insert(rule("rs-no-unwrap", "never-matches-anything", None));
        assert_eq!(set.len(), before);

        let input = json!({ "file_path": "a.rs", "new_string": "x.unwrap()" });
        assert!(
            !set.match_tool("Edit", &input)
                .iter()
                .any(|r| r.name == "rs-no-unwrap"),
            "the replacement's condition should be the one that runs"
        );
    }

    #[test]
    fn the_git_rules_catch_what_they_are_for() {
        let mut set = RuleSet::default();
        for rule in builtin_rules() {
            set.insert(rule.clone());
        }
        let fires = |command: &str| -> Vec<String> {
            set.match_tool("Bash", &json!({ "command": command }))
                .iter()
                .map(|r| r.name.clone())
                .collect()
        };

        assert!(fires("git add -A").contains(&"git-add-all".to_string()));
        assert!(fires("git add .").contains(&"git-add-all".to_string()));
        assert!(fires("git commit -a -m x").contains(&"git-add-all".to_string()));
        assert!(fires("git reset --hard").contains(&"git-destructive".to_string()));
        assert!(fires("git clean -fd").contains(&"git-destructive".to_string()));
        assert!(fires("git push --force origin main").contains(&"git-destructive".to_string()));

        // The ordinary forms have to stay silent, or the rule is unusable.
        assert!(
            fires("git add src/a.rs").is_empty(),
            "naming a file is fine"
        );
        assert!(fires("git status --short").is_empty());
        assert!(
            fires("git checkout -- src/a.rs").is_empty(),
            "restoring one named file is the advice, not the mistake"
        );
        assert!(fires("git commit -m 'add a thing'").is_empty());
    }

    #[test]
    fn the_unwrap_rule_reads_what_is_written_not_what_is_removed() {
        let mut set = RuleSet::default();
        for rule in builtin_rules() {
            set.insert(rule.clone());
        }
        let writing = json!({
            "file_path": "src/a.rs",
            "old_string": "let x = 1;",
            "new_string": "let x = y.unwrap();"
        });
        let removing = json!({
            "file_path": "src/a.rs",
            "old_string": "let x = y.unwrap();",
            "new_string": "let x = y?;"
        });
        assert!(set
            .match_tool("Edit", &writing)
            .iter()
            .any(|r| r.name == "rs-no-unwrap"));
        assert!(set.match_tool("Edit", &removing).is_empty());
    }

    #[test]
    fn a_shipped_rule_stays_inside_its_language() {
        let mut set = RuleSet::default();
        for rule in builtin_rules() {
            set.insert(rule.clone());
        }
        // `: any` is a TypeScript rule and this is Rust, where the same two
        // characters appear in every type annotation.
        let rust = json!({ "file_path": "src/a.rs", "new_string": "fn f(x: anyhow::Error) {}" });
        assert!(
            !set.match_tool("Edit", &rust)
                .iter()
                .any(|r| r.name == "ts-no-any"),
            "a TypeScript rule fired on a Rust file"
        );
    }

    // ---- Repeat policy ---------------------------------------------------

    fn repeating(name: &str, repeat: RepeatPolicy) -> Rule {
        Rule {
            repeat,
            ..rule(name, "x", None)
        }
    }

    #[test]
    fn a_rule_speaks_once_by_default() {
        // A rule that repeats every turn becomes noise, and noise is ignored.
        let session = "once-session";
        forget_session(session);
        let rule = repeating("r", RepeatPolicy::Once);
        assert!(claim(session, &rule, 1));
        assert!(!claim(session, &rule, 2));
        forget_session(session);
    }

    #[test]
    fn an_always_rule_speaks_every_time() {
        let session = "always-session";
        forget_session(session);
        let rule = repeating("r", RepeatPolicy::Always);
        assert!(claim(session, &rule, 1));
        assert!(claim(session, &rule, 2));
        forget_session(session);
    }

    #[test]
    fn a_gap_rule_waits_the_stated_turns() {
        let session = "gap-session";
        forget_session(session);
        let rule = repeating("r", RepeatPolicy::AfterTurns(3));
        assert!(claim(session, &rule, 10));
        assert!(!claim(session, &rule, 12), "two turns is not three");
        assert!(claim(session, &rule, 13));
        forget_session(session);
    }

    #[test]
    fn a_rule_marked_from_an_earlier_run_stays_quiet() {
        // What a resumed session needs: the rule already said its piece.
        let session = "resumed-session";
        forget_session(session);
        mark_fired(session, &["r".to_string()]);
        assert!(!claim(session, &repeating("r", RepeatPolicy::Once), 1));
        assert_eq!(fired_in(session), vec!["r".to_string()]);
        forget_session(session);
        assert!(fired_in(session).is_empty());
    }

    #[test]
    fn the_rendered_block_names_the_rule_and_its_file() {
        let rendered = render_rule(&rule("no-unwrap", "x", None));
        assert!(rendered.contains("rule=\"no-unwrap\""), "{rendered}");
        assert!(rendered.contains("/rules/no-unwrap.md"), "{rendered}");
        assert!(rendered.contains("</system-reminder>"), "{rendered}");
    }
}
