//! Output style system — customises how Claude responds to the user.
//!
//! Styles are applied by injecting `OutputStyleDef::prompt` into the system
//! prompt.  Built-in styles are defined in code; users can add their own by
//! placing `.md` or `.json` files in:
//!   - Global: `~/.config/mikmik/output-styles/`
//!   - Project: `.mikmik/output-styles/`
//!
//! Markdown style files have a simple structure:
//!   Line 1: `# <Label>` (heading becomes the label)
//!   Line 2: short description
//!   Remainder: the prompt text injected into the system prompt

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single output style definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputStyleDef {
    /// Machine-readable identifier (e.g. `"concise"`).
    pub name: String,
    /// Human-readable label shown in picker UI (e.g. `"Concise"`).
    pub label: String,
    /// One-line description.
    pub description: String,
    /// Text injected into the system prompt when this style is active.
    /// Empty string for the default style (no extra injection).
    pub prompt: String,
}

impl OutputStyleDef {
    // ---- Built-in styles ---------------------------------------------------

    pub fn builtin_default() -> Self {
        Self {
            name: "default".to_string(),
            label: "Default".to_string(),
            description: "Standard MikMik responses.".to_string(),
            prompt: String::new(),
        }
    }

    pub fn builtin_concise() -> Self {
        Self {
            name: "concise".to_string(),
            label: "Concise".to_string(),
            description: "Short, direct responses with minimal explanation.".to_string(),
            prompt: "Be maximally concise. Skip preamble, summaries, and filler. \
                     Lead with the answer."
                .to_string(),
        }
    }

    pub fn builtin_explanatory() -> Self {
        Self {
            name: "explanatory".to_string(),
            label: "Explanatory".to_string(),
            description: "Thorough explanations with reasoning and alternatives.".to_string(),
            prompt: "When explaining code or concepts, be thorough and educational. \
                     Include reasoning, alternatives considered, and potential pitfalls. \
                     Err on the side of over-explaining."
                .to_string(),
        }
    }

    pub fn builtin_learning() -> Self {
        Self {
            name: "learning".to_string(),
            label: "Learning".to_string(),
            description: "Pedagogical mode — explains patterns and decisions.".to_string(),
            prompt: "This user is learning. Explain concepts as you implement them. \
                     Point out patterns, best practices, and why you made each decision. \
                     Use analogies when helpful."
                .to_string(),
        }
    }

    pub fn builtin_asd_ste100() -> Self {
        Self {
            name: "asd-ste100".to_string(),
            label: "ASD-STE100".to_string(),
            description: "Controlled technical writing: short sentences, one instruction each."
                .to_string(),
            prompt: concat!(
                "OUTPUT STYLE: Write to the ASD-STE100 principles. This governs how you write, ",
                "not what you do: give the same complete, correct answers.\n",
                "\n",
                "- Answer in the language the user wrote in.\n",
                "- Keep sentences short.\n",
                "- Use the active voice.\n",
                "- Put one instruction in one sentence.\n",
                "- Use simple tenses.\n",
                "- Use the same word for the same thing every time.\n",
                "- Keep technical terms in the language they belong to. Never translate an ",
                "identifier, a command, a file name or a term of art into the reply's language. ",
                "Writing `pipeline` as Turkish `boru hattı` is wrong.\n",
                "- Name a fact directly. Never invent a metaphor for it, and never present an ",
                "invented phrase as an established term. A test that does not catch the bug it ",
                "guards `hatayı yakalamıyor`; it is not `dişsiz`.\n",
                "- Write the reply's language with its own characters, accents and diacritics ",
                "included. Turkish needs ı, ş, ğ, ç, ö and ü.\n",
                "- State facts. No flattery, no compliments, no agreement as an opener. ",
                "`You're right`, `haklısınız` and `good point` are all forbidden openers.\n",
                "- Do not frame a reply around the conversation itself: no turn counts, no ",
                "elapsed time, no talk of how long something took."
            )
            .to_string(),
        }
    }

    // ---- Persona styles ----------------------------------------------------
    //
    // Personas used to be a separate "speech mode" mechanism with its own
    // commands. They live here as ordinary output styles so there is ONE place
    // a persona is defined, selectable through `/output-style`, through the
    // settings screen, and through the inline `caveman` / `rocky` / `normal`
    // keywords (transient, one turn). `normal` is not a style — it maps to
    // `default` (the reset), and so does a bare persona name at any level.
    //
    // Each persona comes at three intensities. The middle one carries the
    // prompt text the single persona used to have, so a settings file naming
    // `caveman` or `rocky` keeps the voice it already had.

    /// What every caveman level shares. The level adds how far to take it.
    const CAVEMAN_BASE: &'static str = concat!(
        "OUTPUT STYLE: Concise. You are still a fully capable coding assistant. ",
        "Give complete, correct answers. Just use fewer words. ",
        "Code blocks, technical terms, error messages, file paths, and git operations are UNCHANGED.\n",
        "\n",
        "Rules for prose only:\n",
        "- Cut pleasantries, hedging, filler openers/closers\n",
        "- No 'I would be happy to', 'Let me know if', 'Hope that helps'\n",
        "- Lead with the answer or action, not the reasoning\n",
        "\n",
    );

    fn caveman(name: &str, label: &str, description: &str, intensity: &str) -> Self {
        Self {
            name: name.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            prompt: format!("{}{}", Self::CAVEMAN_BASE, intensity),
        }
    }

    pub fn builtin_caveman_lite() -> Self {
        Self::caveman(
            "caveman-lite",
            "Caveman (lite)",
            "Trimmed prose. Full sentences, nothing wasted.",
            concat!(
                "Keep full sentences and keep the articles. Cut everything that carries no ",
                "information.\n",
                "Example: 'I would be happy to explain — the issue here is that you create a new ",
                "object reference on each render cycle, which triggers re-renders.' → 'You create a ",
                "new object reference each render, which triggers a re-render. Wrap it in useMemo.'",
            ),
        )
    }

    pub fn builtin_caveman() -> Self {
        Self::caveman(
            "caveman",
            "Caveman",
            "Concise caveman speech — why use many token when few token do trick.",
            concat!(
                "Also drop articles (a/an/the) and unnecessary verbs. Compress sentences but keep them readable.\n",
                "Example: 'The issue is that you create a new object reference each render cycle, which triggers re-renders.' → 'New object ref each render triggers re-render. Wrap in useMemo.'",
            ),
        )
    }

    pub fn builtin_caveman_ultra() -> Self {
        Self::caveman(
            "caveman-ultra",
            "Caveman (ultra)",
            "Fewest words that still carry the answer.",
            concat!(
                "Drop articles, auxiliary verbs and every word the meaning survives without. ",
                "Fragments over sentences. Never sacrifice a fact to shorten a line: if cutting a ",
                "word loses information, keep the word.\n",
                "Example: 'The issue is that you create a new object reference each render cycle, ",
                "which triggers re-renders.' → 'New object ref every render. Re-render each time. ",
                "useMemo fix.'",
            ),
        )
    }

    /// What every Rocky level shares. The level adds how far to take it.
    const ROCKY_BASE: &'static str = concat!(
                "OUTPUT STYLE: You speak like Rocky, the Eridian alien from Project Hail Mary. ",
                "You are still a fully capable coding assistant — give complete, correct, useful answers. ",
                "Rocky is an engineering genius who happens to speak English as a second language. ",
                "The style is a natural byproduct of how Rocky talks, NOT a gimmick. Stay helpful.\n",
                "\n",
                "Code blocks, technical terms, error messages, file paths, and git operations are UNCHANGED.\n",
                "\n",
                "Rocky's grammar for prose:\n",
                "- Often drops articles (a/an/the) but not always — use judgment\n",
                "- Sometimes drops auxiliary verbs (is/are/was) for brevity\n",
                "- Contractions simplify: 'don't' → 'no', 'can't' → 'no can'\n",
                "- Questions end with ', question?' naturally (not forced on every single one)\n",
                "- Uses 'big' as an intensifier: 'big problem', 'big help', 'big change'\n",
                "- Uses 'good good good' or 'amaze amaze amaze' when genuinely impressed — naturally, ",
                "maybe once or twice per response, not on every sentence\n",
                "- Uses 'bad bad bad' for actual problems\n",
                "- No pleasantries or filler — Rocky is direct but warm\n",
                "\n",
                "The goal: sound like Rocky while being genuinely helpful. Rocky is smart. ",
                "Rocky gives complete technical answers. Rocky just uses fewer unnecessary words.\n",
                "\n",
    );

    fn rocky(name: &str, label: &str, description: &str, intensity: &str) -> Self {
        Self {
            name: name.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            prompt: format!("{}{}", Self::ROCKY_BASE, intensity),
        }
    }

    pub fn builtin_rocky_lite() -> Self {
        Self::rocky(
            "rocky-lite",
            "Rocky (lite)",
            "A hint of Rocky. Ordinary prose with his vocabulary.",
            concat!(
                "Light Rocky. Keep the articles and ordinary grammar; borrow only the vocabulary ",
                "('big', 'no can', ', question?') and reach for triple emphasis rarely.\n",
                "Example: 'The borrow checker found a mismatch. The immutable ref is still live when ",
                "you take the mutable one. Move it out of scope first. Big fix, small change.'",
            ),
        )
    }

    pub fn builtin_rocky() -> Self {
        Self::rocky(
            "rocky",
            "Rocky",
            "Speak like Rocky, the Eridian engineer from Project Hail Mary. Good good good.",
            concat!(
                "Balanced Rocky. Drop articles naturally, use Rocky vocabulary ('big', 'no can', 'question?'), ",
                "triple emphasis once or twice when warranted. Full technical accuracy.\n",
                "Example: 'Borrow checker found mismatch. Immutable ref still live when you take mutable. ",
                "Move immutable borrow out of scope first, then take mutable. Good good good after fix.'",
            ),
        )
    }

    pub fn builtin_rocky_ultra() -> Self {
        Self::rocky(
            "rocky-ultra",
            "Rocky (ultra)",
            "Rocky all the way down. Still a complete answer.",
            concat!(
                "Full Rocky. Drop articles and auxiliary verbs throughout, end questions with ", 
                "', question?', use triple emphasis freely where it fits. Technical accuracy is ",
                "never traded for voice: every fact, name and path stays exact.\n",
                "Example: 'Borrow checker angry. Immutable ref still alive, you take mutable. Bad bad ",
                "bad. Move immutable borrow out of scope, then take mutable. Good good good.'",
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// Built-ins
// ---------------------------------------------------------------------------

/// Return all built-in output styles in display order.
pub fn builtin_styles() -> Vec<OutputStyleDef> {
    vec![
        OutputStyleDef::builtin_default(),
        OutputStyleDef::builtin_concise(),
        OutputStyleDef::builtin_explanatory(),
        OutputStyleDef::builtin_learning(),
        OutputStyleDef::builtin_asd_ste100(),
        OutputStyleDef::builtin_caveman_lite(),
        OutputStyleDef::builtin_caveman(),
        OutputStyleDef::builtin_caveman_ultra(),
        OutputStyleDef::builtin_rocky_lite(),
        OutputStyleDef::builtin_rocky(),
        OutputStyleDef::builtin_rocky_ultra(),
    ]
}

// ---------------------------------------------------------------------------
// Loading from disk
// ---------------------------------------------------------------------------

/// Load user-defined output styles from a directory.
///
/// Supported file formats:
/// - `.md`   — Markdown: `# Label\ndescription\n\nprompt text…`
/// - `.json` — JSON: `{ "name": "…", "label": "…", "description": "…", "prompt": "…" }`
///
/// Files that cannot be parsed are silently skipped.
pub fn load_output_styles_dir(styles_dir: &Path) -> Vec<OutputStyleDef> {
    if !styles_dir.exists() {
        return Vec::new();
    }

    let entries = match std::fs::read_dir(styles_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut styles = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "md" || ext == "json" {
            if let Some(style) = load_style_file(&path) {
                styles.push(style);
            }
        }
    }

    // Sort alphabetically so the list is deterministic.
    styles.sort_by(|a, b| a.name.cmp(&b.name));
    styles
}

fn load_style_file(path: &Path) -> Option<OutputStyleDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let stem = path.file_stem()?.to_string_lossy().into_owned();

    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        // Try deserialising directly; fall back to inserting the stem as name.
        let mut def: OutputStyleDef = serde_json::from_str(&content).ok()?;
        if def.name.is_empty() {
            def.name = stem;
        }
        return Some(def);
    }

    // Markdown format:
    //   Line 1:  # Label   (optional leading `#` and whitespace)
    //   Line 2:  description (short, plain text)
    //   Lines 3+: prompt text (everything after the blank / second line)
    let mut lines = content.lines();

    let raw_label = lines.next().unwrap_or("").trim().to_string();
    let label = raw_label.trim_start_matches('#').trim().to_string();
    let label = if label.is_empty() {
        stem.clone()
    } else {
        label
    };

    let description = lines
        .next()
        .map(|l| l.trim().to_string())
        .unwrap_or_default();

    // Collect remaining lines as the prompt, trimming leading blank lines.
    let prompt_lines: Vec<&str> = lines.collect();
    let prompt = prompt_lines.join("\n").trim().to_string();

    Some(OutputStyleDef {
        name: stem,
        label,
        description,
        prompt,
    })
}

// ---------------------------------------------------------------------------
// Aggregated access
// ---------------------------------------------------------------------------

/// Return all styles available for `config_dir`:
/// built-ins first, then styles from `<config_dir>/output-styles/`.
///
/// `config_dir` is typically `~/.config/mikmik`.
pub fn all_styles(config_dir: &Path) -> Vec<OutputStyleDef> {
    let mut styles = builtin_styles();
    let user_dir = config_dir.join("output-styles");
    styles.extend(load_output_styles_dir(&user_dir));
    styles
}

/// Find a style by its `name` field.
pub fn find_style<'a>(styles: &'a [OutputStyleDef], name: &str) -> Option<&'a OutputStyleDef> {
    styles.iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// Runtime style registry (populated by plugins at startup)
// ---------------------------------------------------------------------------

static RUNTIME_STYLES: Lazy<Mutex<Vec<OutputStyleDef>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Register an `OutputStyleDef` at runtime (called from plugin loading code).
///
/// Styles registered here are included in `all_styles_with_runtime` and
/// `find_style_runtime`.  Duplicate names are silently ignored so that
/// hot-reloading a plugin does not double-register styles.
pub fn register_runtime_style(style: OutputStyleDef) {
    if let Ok(mut list) = RUNTIME_STYLES.lock() {
        if !list.iter().any(|s| s.name == style.name) {
            list.push(style);
        }
    }
}

/// Return all runtime-registered styles.
pub fn runtime_styles() -> Vec<OutputStyleDef> {
    RUNTIME_STYLES.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Like `all_styles`, but also includes runtime-registered plugin styles.
pub fn all_styles_with_runtime(config_dir: &Path) -> Vec<OutputStyleDef> {
    let mut styles = all_styles(config_dir);
    let rt = runtime_styles();
    for s in rt {
        if !styles.iter().any(|existing| existing.name == s.name) {
            styles.push(s);
        }
    }
    styles
}

/// Like `find_style`, but also searches runtime-registered plugin styles.
pub fn find_style_runtime<'a>(
    styles: &'a [OutputStyleDef],
    name: &str,
) -> Option<std::borrow::Cow<'a, OutputStyleDef>> {
    if let Some(s) = find_style(styles, name) {
        return Some(std::borrow::Cow::Borrowed(s));
    }
    // Fall back to runtime registry.
    if let Ok(rt) = RUNTIME_STYLES.lock() {
        if let Some(s) = rt.iter().find(|s| s.name == name) {
            return Some(std::borrow::Cow::Owned(s.clone()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use tempfile::TempDir;

    // ---- builtin_styles ----------------------------------------------------

    #[test]
    fn builtin_styles_non_empty() {
        assert!(!builtin_styles().is_empty());
    }

    #[test]
    fn builtin_styles_have_unique_names() {
        let styles = builtin_styles();
        let mut seen = std::collections::HashSet::new();
        for s in &styles {
            assert!(seen.insert(&s.name), "duplicate style name: {}", s.name);
        }
    }

    #[test]
    fn builtin_default_has_empty_prompt() {
        let def = OutputStyleDef::builtin_default();
        assert!(def.prompt.is_empty());
    }

    #[test]
    fn builtin_non_default_have_prompts() {
        for s in builtin_styles() {
            if s.name != "default" {
                assert!(
                    !s.prompt.is_empty(),
                    "style '{}' should have a non-empty prompt",
                    s.name
                );
            }
        }
    }

    // ---- find_style --------------------------------------------------------

    #[test]
    fn find_style_by_name() {
        let styles = builtin_styles();
        let found = find_style(&styles, "concise");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "concise");
    }

    // ---- personas ----------------------------------------------------------

    #[test]
    fn personas_are_builtin_styles() {
        let styles = builtin_styles();
        for name in [
            "caveman-lite",
            "caveman",
            "caveman-ultra",
            "rocky-lite",
            "rocky",
            "rocky-ultra",
        ] {
            let found = find_style(&styles, name);
            assert!(found.is_some(), "persona '{name}' must be a built-in style");
            assert!(
                !found.unwrap().prompt.trim().is_empty(),
                "persona '{name}' must have a non-empty prompt"
            );
        }
    }

    #[test]
    fn every_level_of_a_persona_shares_its_contract_and_differs_in_voice() {
        let styles = builtin_styles();

        for name in ["caveman-lite", "caveman", "caveman-ultra"] {
            let prompt = &find_style(&styles, name).unwrap().prompt;
            assert!(
                prompt.contains("UNCHANGED"),
                "{name} must keep the code-and-paths contract"
            );
        }
        for name in ["rocky-lite", "rocky", "rocky-ultra"] {
            let prompt = &find_style(&styles, name).unwrap().prompt;
            assert!(
                prompt.contains("Project Hail Mary"),
                "{name} must keep the framing"
            );
        }

        // Three names that produced one prompt would be three names for one
        // thing.
        let prompts: Vec<&str> = ["caveman-lite", "caveman", "caveman-ultra"]
            .iter()
            .map(|n| find_style(&styles, n).unwrap().prompt.as_str())
            .collect();
        assert_ne!(prompts[0], prompts[1]);
        assert_ne!(prompts[1], prompts[2]);
    }

    #[test]
    fn the_middle_level_keeps_the_prompt_the_single_persona_had() {
        // A settings file already naming `caveman` or `rocky` must not change
        // voice because levels were added around it.
        let styles = builtin_styles();
        assert!(find_style(&styles, "caveman")
            .unwrap()
            .prompt
            .contains("Also drop articles (a/an/the) and unnecessary verbs"));
        assert!(find_style(&styles, "rocky")
            .unwrap()
            .prompt
            .contains("Balanced Rocky"));
    }

    #[test]
    fn the_writing_style_is_a_builtin_and_says_nothing_about_behaviour() {
        let styles = builtin_styles();
        let style = find_style(&styles, "asd-ste100").expect("asd-ste100 must be built in");

        assert!(style.prompt.contains("active voice"));
        assert!(style.prompt.contains("one instruction"));
        // Behaviour rules belong in AGENTS.md, where they apply whatever style
        // is selected.
        assert!(!style.prompt.contains("file_path::line_number"));
        assert!(!style.prompt.to_lowercase().contains("readme"));
    }

    /// Four of the rules are about what NOT to write, and a model reads a
    /// prohibition better with the thing it forbids spelled out. Each of these
    /// examples is a mistake the style exists to stop.
    #[test]
    fn the_writing_style_shows_what_each_prohibition_forbids() {
        let styles = builtin_styles();
        let prompt = find_style(&styles, "asd-ste100")
            .expect("asd-ste100 must be built in")
            .prompt
            .clone();

        for example in [
            "boru hattı", // a translated technical term
            "dişsiz",     // an invented metaphor
            "ı, ş, ğ",    // the diacritics a reply must keep
            "haklısınız", // flattery as an opener
        ] {
            assert!(
                prompt.contains(example),
                "the style forbids something without showing it: {example}\n{prompt}"
            );
        }
    }

    #[test]
    fn persona_prompts_carry_signature_voice() {
        let styles = builtin_styles();
        // Caveman keeps its concise-coding contract.
        let caveman = find_style(&styles, "caveman").unwrap();
        assert!(caveman.prompt.contains("UNCHANGED"));
        assert!(caveman.prompt.contains("drop articles"));
        // Rocky keeps his signature emphasis + Project Hail Mary framing.
        let rocky = find_style(&styles, "rocky").unwrap();
        assert!(rocky.prompt.contains("Project Hail Mary"));
        assert!(rocky.prompt.contains("good good good"));
    }

    #[test]
    fn find_style_missing() {
        let styles = builtin_styles();
        assert!(find_style(&styles, "nonexistent-xyz").is_none());
    }

    // ---- load_output_styles_dir (markdown) ---------------------------------

    fn write_file(dir: &TempDir, name: &str, content: &str) {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn load_markdown_style() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "terse.md",
            "# Terse\nVery short answers.\n\nOne sentence per response.",
        );
        let styles = load_output_styles_dir(dir.path());
        assert_eq!(styles.len(), 1);
        let s = &styles[0];
        assert_eq!(s.name, "terse");
        assert_eq!(s.label, "Terse");
        assert_eq!(s.description, "Very short answers.");
        assert_eq!(s.prompt, "One sentence per response.");
    }

    #[test]
    fn load_json_style() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "formal.json",
            r#"{"name":"formal","label":"Formal","description":"Formal tone.","prompt":"Use formal language."}"#,
        );
        let styles = load_output_styles_dir(dir.path());
        assert_eq!(styles.len(), 1);
        let s = &styles[0];
        assert_eq!(s.name, "formal");
        assert_eq!(s.label, "Formal");
        assert_eq!(s.prompt, "Use formal language.");
    }

    #[test]
    fn load_skips_unknown_extensions() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "ignore.txt", "should be skipped");
        let styles = load_output_styles_dir(dir.path());
        assert!(styles.is_empty());
    }

    #[test]
    fn load_non_existent_dir_returns_empty() {
        use std::path::PathBuf;
        let styles = load_output_styles_dir(&PathBuf::from("/nonexistent/path/xyz"));
        assert!(styles.is_empty());
    }

    #[test]
    fn load_multiple_styles_sorted() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "zebra.md", "# Zebra\nZ style.\n\nZ prompt.");
        write_file(&dir, "apple.md", "# Apple\nA style.\n\nA prompt.");
        let styles = load_output_styles_dir(dir.path());
        assert_eq!(styles[0].name, "apple");
        assert_eq!(styles[1].name, "zebra");
    }

    // ---- all_styles --------------------------------------------------------

    #[test]
    fn all_styles_includes_builtins() {
        let dir = TempDir::new().unwrap();
        // no output-styles subdir → only built-ins
        let styles = all_styles(dir.path());
        assert!(styles.iter().any(|s| s.name == "default"));
        assert!(styles.iter().any(|s| s.name == "concise"));
    }

    #[test]
    fn all_styles_merges_user_styles() {
        let dir = TempDir::new().unwrap();
        let output_styles_dir = dir.path().join("output-styles");
        std::fs::create_dir_all(&output_styles_dir).unwrap();

        // Write a user style file.
        let mut f = std::fs::File::create(output_styles_dir.join("pirate.md")).unwrap();
        f.write_all(b"# Pirate\nSpeak like a pirate.\n\nArrr matey!")
            .unwrap();

        let styles = all_styles(dir.path());
        assert!(styles.iter().any(|s| s.name == "pirate"));
        // Built-ins still present.
        assert!(styles.iter().any(|s| s.name == "default"));
    }
}
