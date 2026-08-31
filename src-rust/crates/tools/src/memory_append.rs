//! Shared append-with-dedup logic for the `Learn` and `Retain` tools.
//!
//! Both record one short entry into a memory file: newest first, no
//! duplicates, a bounded number of entries, credentials masked on the way in.
//! `Learn` writes lessons to `learned.md`; `Retain` writes facts to
//! `facts.md`. Only the file, the frontmatter, and the noun differ, so the
//! machinery lives here once and both tools call `append_entry`.

use crate::ToolResult;
use std::path::Path;

/// One recorded entry, as it appears in the file.
pub struct Entry {
    /// The whole block, heading included, without a trailing newline.
    pub text: String,
    /// The item line, normalised for comparison.
    pub key: String,
}

/// The fixed shape of one append-style memory file.
pub struct AppendConfig {
    /// File written inside the memory directory, e.g. `learned.md`.
    pub filename: &'static str,
    /// Frontmatter written once, when the file is created.
    pub frontmatter: &'static str,
    /// Characters kept from the item text.
    pub max_item_chars: usize,
    /// Characters kept from the optional context.
    pub max_context_chars: usize,
    /// Entries kept; the oldest drops when a new one arrives.
    pub cap: usize,
    /// Singular noun used in the report and the log ("lesson", "fact").
    pub noun: &'static str,
}

/// Lower-case, whitespace collapsed. Two items that differ only in spacing or
/// capitalisation are the same item.
fn normalise(line: &str) -> String {
    line.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Keep at most `limit` characters, on a character boundary.
fn clip(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!("{}…", kept.trim_end())
}

/// Clipped-then-masked text plus the credential classes that fired.
struct Masked {
    text: String,
    classes: Vec<&'static str>,
}

/// Clip to `limit`, then mask any credential. Masked rather than refused: the
/// tool is the model recording what it understood, and refusing would lose the
/// whole entry over a value that is not the point of it.
fn clip_and_mask(raw: &str, limit: usize) -> Masked {
    let redacted = mikmik_core::redact::redact_secrets(&clip(raw, limit));
    Masked {
        text: redacted.text,
        classes: redacted.classes,
    }
}

/// Split a file body into entries, newest first.
///
/// An entry is a `## ` heading and everything under it. Anything before the
/// first heading is dropped: the frontmatter is handled separately, and a
/// stray note between entries has no item line to compare.
pub fn parse_entries(body: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut current: Option<Vec<&str>> = None;

    for line in body.lines() {
        if line.starts_with("## ") {
            if let Some(lines) = current.take() {
                entries.push(build_entry(&lines));
            }
            current = Some(vec![line]);
        } else if let Some(lines) = current.as_mut() {
            lines.push(line);
        }
    }
    if let Some(lines) = current {
        entries.push(build_entry(&lines));
    }

    entries
}

/// The item is the first non-empty line under the heading.
fn build_entry(lines: &[&str]) -> Entry {
    let key = lines
        .iter()
        .skip(1)
        .find(|line| !line.trim().is_empty())
        .map(|line| normalise(line))
        .unwrap_or_default();
    Entry {
        text: lines.join("\n").trim_end().to_string(),
        key,
    }
}

/// Render the file from its entries.
fn render(frontmatter: &str, entries: &[Entry]) -> String {
    let mut out = String::from(frontmatter);
    for entry in entries {
        out.push('\n');
        out.push_str(&entry.text);
        out.push('\n');
    }
    out
}

/// Everything after the frontmatter block, or the whole text when there is none.
pub fn body_after_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    match rest.find("\n---\n") {
        Some(end) => &rest[end + "\n---\n".len()..],
        None => text,
    }
}

/// A dated heading, with the topic appended when one is given.
fn build_heading(topic: Option<&str>) -> String {
    match topic.map(str::trim) {
        Some(topic) if !topic.is_empty() => {
            format!("## {} — {}", chrono::Local::now().format("%Y-%m-%d"), topic)
        }
        _ => format!("## {}", chrono::Local::now().format("%Y-%m-%d")),
    }
}

/// The union of credential classes masked across the item and its context.
fn all_classes(item: &Masked, context: &Option<Masked>) -> Vec<&'static str> {
    let mut masked = item.classes.clone();
    if let Some(context) = context {
        for class in &context.classes {
            if !masked.contains(class) {
                masked.push(class);
            }
        }
    }
    masked
}

/// The block for one entry: heading, item text, and the context line.
fn build_block(topic: Option<&str>, item: &str, context: &Option<Masked>) -> String {
    let mut text = format!("{}\n{}", build_heading(topic), item);
    if let Some(context) = context {
        if !context.text.is_empty() {
            text.push_str(&format!("\n\n_context: {}_", context.text));
        }
    }
    text
}

/// The success line, plus the cap and masking notes when they apply.
fn build_report(
    path: &Path,
    count: usize,
    dropped: usize,
    masked: &[&'static str],
    cfg: &AppendConfig,
) -> String {
    let mut report = format!("Recorded in {} ({count} {}s).", path.display(), cfg.noun);
    if dropped > 0 {
        report.push_str(&format!(
            " The {dropped} oldest dropped at the {}-{} cap.",
            cfg.cap, cfg.noun
        ));
    }
    if !masked.is_empty() {
        report.push_str(&format!(
            " A credential was masked before writing ({}).",
            masked.join(", ")
        ));
    }
    report
}

/// Append one item to a memory file: clip, mask, dedup, cap, and write.
///
/// `item` and `context` are the raw model-supplied strings; this function does
/// the clipping and masking so the two tools share one policy.
pub async fn append_entry(
    memory_dir: &Path,
    cfg: &AppendConfig,
    item: &str,
    topic: Option<&str>,
    context: Option<&str>,
) -> ToolResult {
    let path = memory_dir.join(cfg.filename);
    let item = clip_and_mask(item, cfg.max_item_chars);
    let context = context.map(|text| clip_and_mask(text, cfg.max_context_chars));

    let masked = all_classes(&item, &context);
    if !masked.is_empty() {
        tracing::warn!(
            classes = %masked.join(", "),
            path = %path.display(),
            "Masked a credential on its way into a memory entry"
        );
    }

    let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let mut entries = parse_entries(body_after_frontmatter(&existing));

    let key = normalise(&item.text);
    if entries.iter().any(|entry| entry.key == key) {
        return ToolResult::success(format!(
            "Already recorded, so nothing was written. {} holds this {}.",
            path.display(),
            cfg.noun
        ));
    }

    entries.insert(
        0,
        Entry {
            text: build_block(topic, &item.text, &context),
            key,
        },
    );
    let dropped = entries.len().saturating_sub(cfg.cap);
    entries.truncate(cfg.cap);

    if let Err(error) = tokio::fs::create_dir_all(memory_dir).await {
        return ToolResult::error(format!(
            "Failed to create {}: {error}",
            memory_dir.display()
        ));
    }
    if let Err(error) = tokio::fs::write(&path, render(cfg.frontmatter, &entries)).await {
        return ToolResult::error(format!("Failed to write {}: {error}", path.display()));
    }

    ToolResult::success(build_report(&path, entries.len(), dropped, &masked, cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: AppendConfig = AppendConfig {
        filename: "notes.md",
        frontmatter: "---\nname: Notes\ndescription: d\ntype: project\n---\n",
        max_item_chars: 2000,
        max_context_chars: 400,
        cap: 100,
        noun: "note",
    };

    /// The file has to survive a round trip, or a second call would lose what
    /// the first one wrote.
    #[test]
    fn parsing_what_was_rendered_gives_the_same_entries() {
        let entries = vec![
            Entry {
                text: "## 2026-01-01\nthird".to_string(),
                key: "third".to_string(),
            },
            Entry {
                text: "## 2026-01-01\nsecond".to_string(),
                key: "second".to_string(),
            },
            Entry {
                text: "## 2026-01-01\nfirst".to_string(),
                key: "first".to_string(),
            },
        ];
        let rendered = render(CONFIG.frontmatter, &entries);
        let parsed = parse_entries(body_after_frontmatter(&rendered));
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].key, "third");
        assert_eq!(parsed[2].key, "first");
    }

    #[test]
    fn normalise_collapses_case_and_whitespace() {
        assert_eq!(normalise("  Cargo   RUNS here "), "cargo runs here");
    }

    #[test]
    fn clip_keeps_a_character_boundary_and_marks_the_cut() {
        let long = "é".repeat(10);
        let clipped = clip(&long, 4);
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().filter(|c| *c == 'é').count(), 4);
    }
}
