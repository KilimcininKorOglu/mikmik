//! Applies TOML-defined filter rules to command output.
//!
//! Ported from RTK (Rust Token Killer), stripped of its CLI/hook/trust layers.
//! Provides a declarative pipeline of 8 stages configured via TOML. In this port
//! only the built-in filters (embedded below) are loaded; user-defined project
//! and global filter files are a later phase.
//!
//! Pipeline stages (applied in order):
//!   1. strip_ansi           — remove ANSI escape codes
//!   2. replace              — regex substitutions, line-by-line, chainable
//!   3. match_output         — short-circuit: if blob matches a pattern, return message
//!   4. strip/keep_lines     — filter lines by regex
//!   5. truncate_lines_at    — truncate each line to N chars
//!   6. head/tail_lines      — keep first/last N lines
//!   7. max_lines            — absolute line cap
//!   8. on_empty             — message if result is empty

use regex::{Regex, RegexSet};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::LazyLock;

// Built-in filters, embedded at compile time.
const BUILTIN_TOML: &str = include_str!("builtin_filters.toml");

// ---------------------------------------------------------------------------
// Deserialization types (TOML schema)
// ---------------------------------------------------------------------------

/// A match-output rule: if `pattern` matches anywhere in the full output blob,
/// the filter short-circuits and returns `message` immediately. First matching
/// rule wins. Optional `unless`: if this regex also matches the blob, the rule
/// is skipped (prevents short-circuiting when errors or warnings are present).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchOutputRule {
    pattern: String,
    message: String,
    #[serde(default)]
    unless: Option<String>,
}

/// A regex substitution applied line-by-line. Rules are chained sequentially:
/// rule N+1 operates on the output of rule N. Backreferences (`$1`, ...) work.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRule {
    pattern: String,
    replacement: String,
}

#[derive(Deserialize)]
struct TomlFilterFile {
    schema_version: u32,
    #[serde(default)]
    filters: BTreeMap<String, TomlFilterDef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlFilterDef {
    description: Option<String>,
    match_command: String,
    #[serde(default)]
    strip_ansi: bool,
    /// Regex substitutions, applied line-by-line before match_output (stage 2).
    #[serde(default)]
    replace: Vec<ReplaceRule>,
    /// Short-circuit rules: if the full output blob matches, return the message (stage 3).
    #[serde(default)]
    match_output: Vec<MatchOutputRule>,
    #[serde(default)]
    strip_lines_matching: Vec<String>,
    #[serde(default)]
    keep_lines_matching: Vec<String>,
    truncate_lines_at: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    max_lines: Option<usize>,
    on_empty: Option<String>,
    /// When true, stderr is captured and merged with stdout before filtering.
    #[serde(default)]
    filter_stderr: bool,
}

// ---------------------------------------------------------------------------
// Compiled types (post-validation, ready to use)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CompiledMatchOutputRule {
    pattern: Regex,
    message: String,
    unless: Option<Regex>,
}

#[derive(Debug)]
struct CompiledReplaceRule {
    pattern: Regex,
    replacement: String,
}

#[derive(Debug)]
enum LineFilter {
    None,
    Strip(RegexSet),
    Keep(RegexSet),
}

/// A filter that has been parsed and compiled — all regexes are ready.
#[derive(Debug)]
pub struct CompiledFilter {
    pub name: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    match_regex: Regex,
    strip_ansi: bool,
    replace: Vec<CompiledReplaceRule>,
    match_output: Vec<CompiledMatchOutputRule>,
    line_filter: LineFilter,
    truncate_lines_at: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    pub max_lines: Option<usize>,
    on_empty: Option<String>,
    /// When true, the runner should capture stderr and merge it with stdout.
    #[allow(dead_code)]
    pub filter_stderr: bool,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

struct TomlFilterRegistry {
    filters: Vec<CompiledFilter>,
}

impl TomlFilterRegistry {
    /// Load registry from the built-in TOML. Emits a warning to stderr on parse
    /// errors but never panics — a bad built-in yields an empty registry.
    fn load() -> Self {
        let filters = match Self::parse_and_compile(BUILTIN_TOML, "builtin") {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[mikmik] warning: builtin filters: {}", e);
                Vec::new()
            }
        };
        TomlFilterRegistry { filters }
    }

    fn parse_and_compile(content: &str, source: &str) -> Result<Vec<CompiledFilter>, String> {
        let file: TomlFilterFile = toml::from_str(content)
            .map_err(|e| format!("TOML parse error in {}: {}", source, e))?;

        if file.schema_version != 1 {
            return Err(format!(
                "unsupported schema_version {} in {} (expected 1)",
                file.schema_version, source
            ));
        }

        let mut compiled = Vec::new();
        for (name, def) in file.filters {
            match compile_filter(name.clone(), def) {
                Ok(f) => compiled.push(f),
                Err(e) => eprintln!("[mikmik] warning: filter '{}' in {}: {}", name, source, e),
            }
        }
        Ok(compiled)
    }
}

fn compile_filter(name: String, def: TomlFilterDef) -> Result<CompiledFilter, String> {
    // Mutual exclusion: strip and keep cannot both be set.
    if !def.strip_lines_matching.is_empty() && !def.keep_lines_matching.is_empty() {
        return Err("strip_lines_matching and keep_lines_matching are mutually exclusive".into());
    }

    let match_regex = Regex::new(&def.match_command)
        .map_err(|e| format!("invalid match_command regex: {}", e))?;

    let replace = def
        .replace
        .into_iter()
        .map(|r| {
            let pat = r.pattern.clone();
            Regex::new(&r.pattern)
                .map(|pattern| CompiledReplaceRule {
                    pattern,
                    replacement: r.replacement,
                })
                .map_err(|e| format!("invalid replace pattern '{}': {}", pat, e))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let match_output = def
        .match_output
        .into_iter()
        .map(|r| -> Result<CompiledMatchOutputRule, String> {
            let pat = r.pattern.clone();
            let pattern = Regex::new(&r.pattern)
                .map_err(|e| format!("invalid match_output pattern '{}': {}", pat, e))?;
            let unless = r
                .unless
                .as_deref()
                .map(|u| {
                    Regex::new(u)
                        .map_err(|e| format!("invalid match_output unless pattern '{}': {}", u, e))
                })
                .transpose()?;
            Ok(CompiledMatchOutputRule {
                pattern,
                message: r.message,
                unless,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let line_filter = if !def.strip_lines_matching.is_empty() {
        let set = RegexSet::new(&def.strip_lines_matching)
            .map_err(|e| format!("invalid strip_lines_matching regex: {}", e))?;
        LineFilter::Strip(set)
    } else if !def.keep_lines_matching.is_empty() {
        let set = RegexSet::new(&def.keep_lines_matching)
            .map_err(|e| format!("invalid keep_lines_matching regex: {}", e))?;
        LineFilter::Keep(set)
    } else {
        LineFilter::None
    };

    Ok(CompiledFilter {
        name,
        description: def.description,
        match_regex,
        strip_ansi: def.strip_ansi,
        replace,
        match_output,
        line_filter,
        truncate_lines_at: def.truncate_lines_at,
        head_lines: def.head_lines,
        tail_lines: def.tail_lines,
        max_lines: def.max_lines,
        on_empty: def.on_empty,
        filter_stderr: def.filter_stderr,
    })
}

// ---------------------------------------------------------------------------
// Singleton (lazy-loaded, one-time cost — reused for the process lifetime)
// ---------------------------------------------------------------------------

static REGISTRY: LazyLock<TomlFilterRegistry> = LazyLock::new(TomlFilterRegistry::load);

static MATCH_SET: LazyLock<RegexSet> = LazyLock::new(build_match_set);

/// Whether any built-in filter's `match_command` matches this command. Cheap
/// pre-check before doing the full registry lookup.
pub fn command_matches_filter(command: &str) -> bool {
    MATCH_SET.is_match(command)
}

fn build_match_set() -> RegexSet {
    let patterns = match_patterns_in(BUILTIN_TOML);
    RegexSet::new(&patterns).unwrap_or_else(|_| {
        let valid: Vec<String> = patterns
            .into_iter()
            .filter(|p| Regex::new(p).is_ok())
            .collect();
        RegexSet::new(&valid).unwrap_or_else(|_| RegexSet::empty())
    })
}

fn match_patterns_in(content: &str) -> Vec<String> {
    match toml::from_str::<TomlFilterFile>(content) {
        Ok(file) if file.schema_version == 1 => file
            .filters
            .into_values()
            .map(|def| def.match_command)
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Public API — pure functions (testable without global state)
// ---------------------------------------------------------------------------

/// Find the first matching filter in a slice. O(N) on the number of filters.
pub fn find_filter_in<'a>(
    command: &str,
    filters: &'a [CompiledFilter],
) -> Option<&'a CompiledFilter> {
    filters.iter().find(|f| f.match_regex.is_match(command))
}

/// Find a matching filter from the global registry, initialising it lazily.
pub fn find_matching_filter(command: &str) -> Option<&'static CompiledFilter> {
    find_filter_in(command, &REGISTRY.filters)
}

/// Apply a compiled filter pipeline to raw stdout. Pure String -> String.
pub fn apply_filter(filter: &CompiledFilter, stdout: &str) -> String {
    apply_filter_with_info(filter, stdout).0
}

/// Describes how much of the raw output a filter dropped, so a recovery hint
/// (tee) can tell the reader how to get the rest back.
#[derive(Debug, PartialEq)]
pub enum Lossiness {
    None,
    /// `tail -n +{tail_offset}` over `tee_payload` reproduces the dropped lines.
    Tail {
        tee_payload: String,
        tail_offset: usize,
    },
    Whole,
}

pub fn apply_filter_with_info(filter: &CompiledFilter, stdout: &str) -> (String, Lossiness) {
    let mut lines: Vec<String> = stdout.lines().map(String::from).collect();

    // 1. strip_ansi
    if filter.strip_ansi {
        lines = lines.into_iter().map(|l| super::strip_ansi(&l)).collect();
    }

    // 2. replace — line-by-line, rules chained sequentially
    if !filter.replace.is_empty() {
        lines = lines
            .into_iter()
            .map(|mut line| {
                for rule in &filter.replace {
                    line = rule
                        .pattern
                        .replace_all(&line, rule.replacement.as_str())
                        .into_owned();
                }
                line
            })
            .collect();
    }

    // 3. match_output — short-circuit on full blob match (first rule wins).
    //    If `unless` is set and also matches the blob, the rule is skipped.
    if !filter.match_output.is_empty() {
        let blob = lines.join("\n");
        for rule in &filter.match_output {
            if rule.pattern.is_match(&blob) {
                if let Some(ref unless_re) = rule.unless {
                    if unless_re.is_match(&blob) {
                        continue; // errors/warnings present — skip this rule
                    }
                }
                return (rule.message.clone(), Lossiness::Whole);
            }
        }
    }

    // 4. strip OR keep (mutually exclusive)
    match &filter.line_filter {
        LineFilter::Strip(set) => lines.retain(|l| !set.is_match(l)),
        LineFilter::Keep(set) => lines.retain(|l| set.is_match(l)),
        LineFilter::None => {}
    }

    // 5. truncate_lines_at — unicode-safe
    let mut intra_line_loss = false;
    if let Some(max_chars) = filter.truncate_lines_at {
        lines = lines
            .into_iter()
            .map(|line| {
                let truncated = super::truncate(&line, max_chars);
                if truncated != line {
                    intra_line_loss = true;
                }
                truncated
            })
            .collect();
    }

    let snapshot_for_tail = !intra_line_loss
        && filter.tail_lines.is_none()
        && (filter.head_lines.is_some() || filter.max_lines.is_some());
    let pre_cut = snapshot_for_tail.then(|| lines.clone());

    // 6. head + tail
    let total = lines.len();
    let mut noncontiguous_drop = false;
    let mut head_cut: Option<usize> = None;
    if let (Some(head), Some(tail)) = (filter.head_lines, filter.tail_lines) {
        if total > head + tail {
            let mut result = lines[..head].to_vec();
            result.push(format!("... ({} lines omitted)", total - head - tail));
            result.extend_from_slice(&lines[total - tail..]);
            lines = result;
            noncontiguous_drop = true;
        }
    } else if let Some(head) = filter.head_lines {
        if total > head {
            lines.truncate(head);
            lines.push(format!("... ({} lines omitted)", total - head));
            head_cut = Some(head);
        }
    } else if let Some(tail) = filter.tail_lines {
        if total > tail {
            let omitted = total - tail;
            lines = lines[omitted..].to_vec();
            lines.insert(0, format!("... ({} lines omitted)", omitted));
            noncontiguous_drop = true;
        }
    }

    // 7. max_lines — absolute cap applied after head/tail (includes omit messages)
    let mut max_cut: Option<usize> = None;
    if let Some(max) = filter.max_lines {
        if lines.len() > max {
            let dropped = lines.len() - max;
            lines.truncate(max);
            lines.push(format!("... ({} lines truncated)", dropped));
            max_cut = Some(max);
        }
    }

    // 8. on_empty
    let result = lines.join("\n");
    if result.trim().is_empty() {
        if let Some(ref msg) = filter.on_empty {
            return (msg.clone(), Lossiness::None);
        }
    }

    let loss = if let Some(snapshot) = pre_cut {
        match (head_cut, max_cut) {
            (Some(_), Some(_)) => Lossiness::Whole,
            (Some(head), None) => Lossiness::Tail {
                tee_payload: snapshot.join("\n"),
                tail_offset: head + 1,
            },
            (None, Some(max)) => Lossiness::Tail {
                tee_payload: snapshot.join("\n"),
                tail_offset: max + 1,
            },
            (None, None) => Lossiness::None,
        }
    } else if noncontiguous_drop || intra_line_loss || head_cut.is_some() || max_cut.is_some() {
        Lossiness::Whole
    } else {
        Lossiness::None
    };

    (result, loss)
}

// ---------------------------------------------------------------------------
// Tests (ported from RTK, trimmed to the engine + built-in filters)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_filters(toml: &str) -> Vec<CompiledFilter> {
        TomlFilterRegistry::parse_and_compile(toml, "test").expect("test TOML should be valid")
    }

    fn first_filter(toml: &str) -> CompiledFilter {
        make_filters(toml)
            .into_iter()
            .next()
            .expect("expected at least one filter")
    }

    fn loss_of(toml: &str, input: &str) -> Lossiness {
        apply_filter_with_info(&first_filter(toml), input).1
    }

    #[test]
    fn command_matches_filter_agrees_with_find_matching_filter() {
        for cmd in ["make all", "terraform plan", "frobnicate xyz", "cd /tmp"] {
            assert_eq!(
                command_matches_filter(cmd),
                find_matching_filter(cmd).is_some(),
                "match-set disagreed with registry for {cmd:?}"
            );
        }
    }

    // --- Lossiness ---

    #[test]
    fn test_loss_head_lines_is_tail() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nhead_lines = 2\n";
        let (out, loss) = apply_filter_with_info(&first_filter(toml), "a\nb\nc\nd\ne");
        assert!(out.starts_with("a\nb\n"));
        match loss {
            Lossiness::Tail {
                tee_payload,
                tail_offset,
            } => {
                assert_eq!(tail_offset, 3);
                let recovered: Vec<&str> = tee_payload.lines().skip(tail_offset - 1).collect();
                assert_eq!(recovered, vec!["c", "d", "e"]);
            }
            other => panic!("expected Tail, got {:?}", other),
        }
    }

    #[test]
    fn test_loss_max_lines_is_tail() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nmax_lines = 2\n";
        match loss_of(toml, "a\nb\nc\nd\ne") {
            Lossiness::Tail {
                tee_payload,
                tail_offset,
            } => {
                assert_eq!(tail_offset, 3);
                let recovered: Vec<&str> = tee_payload.lines().skip(tail_offset - 1).collect();
                assert_eq!(recovered, vec!["c", "d", "e"]);
            }
            other => panic!("expected Tail, got {:?}", other),
        }
    }

    #[test]
    fn test_loss_tail_lines_is_whole() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\ntail_lines = 2\n";
        assert_eq!(loss_of(toml, "a\nb\nc\nd\ne"), Lossiness::Whole);
    }

    #[test]
    fn test_loss_head_then_max_is_whole() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nhead_lines = 2\nmax_lines = 2\n";
        assert_eq!(loss_of(toml, "a\nb\nc\nd\ne"), Lossiness::Whole);
    }

    #[test]
    fn test_loss_truncate_lines_at_is_whole() {
        let toml =
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\ntruncate_lines_at = 3\n";
        assert_eq!(loss_of(toml, "abcdefgh\nshort"), Lossiness::Whole);
    }

    #[test]
    fn test_loss_match_output_is_whole() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\n[[filters.f.match_output]]\npattern = \"ok\"\nmessage = \"all good\"\n";
        let (out, loss) = apply_filter_with_info(&first_filter(toml), "everything ok here\nmore");
        assert_eq!(out, "all good");
        assert_eq!(loss, Lossiness::Whole);
    }

    #[test]
    fn test_loss_strip_only_is_none() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nstrip_lines_matching = [\"^noise\"]\n";
        let (out, loss) = apply_filter_with_info(&first_filter(toml), "keep\nnoise line\nkeep2");
        assert_eq!(out, "keep\nkeep2");
        assert_eq!(loss, Lossiness::None);
    }

    #[test]
    fn test_loss_no_truncation_is_none() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nhead_lines = 10\n";
        assert_eq!(loss_of(toml, "a\nb\nc"), Lossiness::None);
    }

    #[test]
    fn test_apply_filter_wrapper_matches_with_info() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nhead_lines = 2\n";
        let f = first_filter(toml);
        let input = "a\nb\nc\nd";
        assert_eq!(apply_filter(&f, input), apply_filter_with_info(&f, input).0);
    }

    // --- Pipeline primitives ---

    #[test]
    fn test_strip_ansi_removes_codes() {
        let f = first_filter(
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nstrip_ansi = true\n",
        );
        assert_eq!(
            apply_filter(&f, "\x1b[31mError\x1b[0m\nnormal"),
            "Error\nnormal"
        );
    }

    #[test]
    fn test_strip_lines_matching_basic() {
        let f = first_filter("schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nstrip_lines_matching = [\"^noise\", \"^verbose\"]\n");
        let out = apply_filter(&f, "noise line\nkeep this\nverbose stuff\nalso keep");
        assert_eq!(out, "keep this\nalso keep");
    }

    #[test]
    fn test_keep_lines_matching_basic() {
        let f = first_filter("schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nkeep_lines_matching = [\"^PASS\", \"^FAIL\"]\n");
        let out = apply_filter(&f, "PASS test_a\nsome noise\nFAIL test_b\nmore noise");
        assert_eq!(out, "PASS test_a\nFAIL test_b");
    }

    #[test]
    fn test_truncate_lines_at_unicode_safe() {
        let f = first_filter(
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\ntruncate_lines_at = 5\n",
        );
        assert_eq!(apply_filter(&f, "hello\n日本語xyz"), "hello\n日本...");
    }

    #[test]
    fn test_head_lines() {
        let f = first_filter(
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nhead_lines = 2\n",
        );
        let out = apply_filter(&f, "a\nb\nc\nd\ne");
        assert!(out.starts_with("a\nb\n"));
        assert!(out.contains("3 lines omitted"));
    }

    #[test]
    fn test_tail_lines() {
        let f = first_filter(
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\ntail_lines = 2\n",
        );
        let out = apply_filter(&f, "a\nb\nc\nd\ne");
        assert!(out.contains("3 lines omitted"));
        assert!(out.ends_with("d\ne"));
    }

    #[test]
    fn test_head_and_tail_combined() {
        let f = first_filter("schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nhead_lines = 2\ntail_lines = 2\n");
        let out = apply_filter(&f, "a\nb\nc\nd\ne\nf");
        assert!(out.starts_with("a\nb\n"));
        assert!(out.contains("2 lines omitted"));
        assert!(out.ends_with("e\nf"));
    }

    #[test]
    fn test_max_lines_counts_omit_message() {
        let f = first_filter(
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nmax_lines = 3\n",
        );
        let out = apply_filter(&f, "a\nb\nc\nd\ne");
        assert_eq!(out.lines().count(), 4);
        assert!(out.contains("lines truncated"));
    }

    #[test]
    fn test_on_empty_when_all_filtered() {
        let f = first_filter("schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nstrip_lines_matching = [\".*\"]\non_empty = \"nothing left\"\n");
        assert_eq!(apply_filter(&f, "line1\nline2"), "nothing left");
    }

    #[test]
    fn test_replace_backreferences() {
        let f = first_filter("schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nreplace = [\n  { pattern = \"(\\\\w+):(\\\\w+)\", replacement = \"$2:$1\" },\n]\n");
        assert_eq!(apply_filter(&f, "hello:world"), "world:hello");
    }

    #[test]
    fn test_match_output_unless_blocks_when_errors_present() {
        let f = first_filter("schema_version = 1\n[filters.f]\nmatch_command = \"^rsync\"\nmatch_output = [\n  { pattern = \"total size is\", message = \"ok (synced)\", unless = \"error|failed\" },\n]\n");
        let out = apply_filter(
            &f,
            "rsync: [sender] error\ntotal size is 1000  speedup is 3.33\n",
        );
        assert_ne!(out.trim(), "ok (synced)");
        assert!(out.contains("error"));
    }

    // --- Validation ---

    #[test]
    fn test_mutual_exclusion_strip_keep_errors() {
        let result = make_filters("schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nstrip_lines_matching = [\"a\"]\nkeep_lines_matching = [\"b\"]\n");
        assert!(result.is_empty());
    }

    #[test]
    fn test_invalid_regex_returns_err() {
        assert!(
            make_filters("schema_version = 1\n[filters.f]\nmatch_command = \"[\"\n").is_empty()
        );
    }

    #[test]
    fn test_schema_version_mismatch_errors() {
        assert!(TomlFilterRegistry::parse_and_compile(
            "schema_version = 99\n[filters.f]\nmatch_command = \"^cmd\"\n",
            "test"
        )
        .is_err());
    }

    #[test]
    fn test_unknown_field_typo_errors() {
        assert!(TomlFilterRegistry::parse_and_compile(
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nstrip_ansi_typo = true\n",
            "test"
        )
        .is_err());
    }

    #[test]
    fn test_empty_filter_passthrough() {
        let f = first_filter("schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\n");
        let input = "line1\nline2\nline3";
        assert_eq!(apply_filter(&f, input), input);
    }

    // --- Built-in registry ---

    #[test]
    fn test_builtin_filters_compile() {
        let result = TomlFilterRegistry::parse_and_compile(BUILTIN_TOML, "builtin");
        assert!(result.is_ok(), "builtin filters failed: {:?}", result);
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_builtin_expected_filters_present() {
        let filters = make_filters(BUILTIN_TOML);
        let names: std::collections::HashSet<&str> =
            filters.iter().map(|f| f.name.as_str()).collect();
        for name in ["df", "du", "make", "ping", "terraform-plan"] {
            assert!(names.contains(name), "built-in filter '{}' missing", name);
        }
    }

    #[test]
    fn test_find_filter_matches_terraform() {
        let filters = make_filters(BUILTIN_TOML);
        let found = find_filter_in("terraform plan -out=tfplan", &filters);
        assert_eq!(found.map(|f| f.name.as_str()), Some("terraform-plan"));
    }

    #[test]
    fn test_find_filter_no_match_returns_none() {
        let filters = make_filters(BUILTIN_TOML);
        assert!(find_filter_in("kubectl get pods", &filters).is_none());
    }

    // --- Token savings (the release floor) ---

    #[test]
    fn test_terraform_savings_above_60pct() {
        let filters = make_filters(BUILTIN_TOML);
        let filter = find_filter_in("terraform plan", &filters).expect("terraform-plan built-in");
        let input = concat!(
            "Acquiring state lock. This may take a few moments...\n",
            "Refreshing state... [id=vpc-0a1b2c3d]\n",
            "Refreshing state... [id=subnet-11111111]\n",
            "Refreshing state... [id=subnet-22222222]\n",
            "Refreshing state... [id=subnet-33333333]\n",
            "Refreshing state... [id=igw-aabbccdd]\n",
            "Refreshing state... [id=rtb-aabbccdd]\n",
            "Refreshing state... [id=sg-00112233]\n",
            "Refreshing state... [id=sg-44556677]\n",
            "Refreshing state... [id=nacl-00aabbcc]\n",
            "Refreshing state... [id=alb-arn:my-alb]\n",
            "Refreshing state... [id=db-ABCDEFGHIJKLMNO]\n",
            "Refreshing state... [id=lambda:my-api-function]\n",
            "Refreshing state... [id=iam-role:my-lambda-role]\n",
            "Refreshing state... [id=s3:::my-app-assets]\n",
            "Refreshing state... [id=cloudfront:ABCDEFGHIJK]\n",
            "Refreshing state... [id=ssm:/my/app/db-url]\n",
            "Refreshing state... [id=secretsmanager:my-secret]\n",
            "Releasing state lock. This may take a few moments...\n",
            "\n",
            "Terraform will perform the following actions:\n",
            "\n",
            "  # aws_instance.web will be created\n",
            "  + resource \"aws_instance\" \"web\" {\n",
            "      + ami           = \"ami-0c55b159cbfafe1f0\"\n",
            "      + instance_type = \"t3.micro\"\n",
            "    }\n",
            "\n",
            "Plan: 1 to add, 0 to change, 0 to destroy.\n",
        );
        let out = apply_filter(filter, input);
        let savings = 100.0
            - (out.split_whitespace().count() as f64 / input.split_whitespace().count() as f64
                * 100.0);
        assert!(
            savings >= 60.0,
            "expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_make_savings_above_60pct() {
        let filters = make_filters(BUILTIN_TOML);
        let filter = find_filter_in("make all", &filters).expect("make built-in");
        let input = r#"make[1]: Entering directory '/home/user/project'
make[2]: Entering directory '/home/user/project/src'
gcc -O2 -Wall -c foo.c -o foo.o

make[2]: Nothing to be done for 'install'.
make[3]: Entering directory '/home/user/project/src/lib'
ar rcs libfoo.a foo.o bar.o baz.o
make[3]: Leaving directory '/home/user/project/src/lib'
make[2]: Leaving directory '/home/user/project/src'

make[1]: Leaving directory '/home/user/project'
gcc -O2 -Wall -c bar.c -o bar.o

gcc -O2 -Wall -c baz.c -o baz.o

make[1]: Entering directory '/home/user/project/test'
make[2]: Entering directory '/home/user/project/test/unit'
./run_tests --verbose
make[2]: Nothing to be done for 'check'.
make[2]: Leaving directory '/home/user/project/test/unit'
make[1]: Leaving directory '/home/user/project/test'

ld -o myapp foo.o bar.o baz.o -lfoo

make[1]: Entering directory '/home/user/project/docs'
doxygen Doxyfile
make[1]: Leaving directory '/home/user/project/docs'
"#;
        let out = apply_filter(filter, input);
        let savings = 100.0
            - (out.split_whitespace().count() as f64 / input.split_whitespace().count() as f64
                * 100.0);
        assert!(
            savings >= 60.0,
            "expected >=60% savings, got {:.1}%",
            savings
        );
    }
}
