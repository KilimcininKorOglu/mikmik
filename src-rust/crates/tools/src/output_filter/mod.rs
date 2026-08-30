//! Command-output filtering, ported from RTK (Rust Token Killer).
//!
//! Compresses a command's stdout before it reaches the LLM context. A
//! command-aware, TOML-defined pipeline (see `toml_filter`) shrinks noisy output
//! (make, terraform, ping, …) by 60-90% while preserving the parts that matter.
//!
//! Two filter kinds run in order: Rust-native filters (`native`, for tsc/pytest/
//! mypy/prettier, whose summaries need stateful parsing TOML cannot express) are
//! tried first, then the TOML pipeline.
//!
//! Safety nets, all from RTK: the `never_worse` guard reverts to raw output when
//! filtering would grow it, `catch_unwind` reverts when a filter panics, and a
//! command with no matching filter passes through untouched. Filtering can only
//! ever help or no-op.

mod guard;
mod native;
mod tee;
pub mod toml_filter;

pub use guard::never_worse;
pub use toml_filter::{
    apply_filter, apply_filter_with_info, command_matches_filter, find_filter_in,
    find_matching_filter, CompiledFilter, Lossiness,
};

use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Pure helpers (ported from RTK src/core/utils.rs + tracking::estimate_tokens)
// ---------------------------------------------------------------------------

/// Strip ANSI escape codes (colors, styles) from a string.
pub fn strip_ansi(text: &str) -> String {
    static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").expect("ANSI regex literal is valid")
    });
    ANSI_RE.replace_all(text, "").to_string()
}

/// Truncate a string to `max_len` chars (unicode-safe), appending "..." when cut.
pub fn truncate(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_string()
    } else if max_len < 3 {
        "...".to_string()
    } else {
        format!("{}...", s.chars().take(max_len - 3).collect::<String>())
    }
}

/// Estimate a token count as `bytes / 4` (RTK ships no tokenizer; ratios are
/// reliable, absolute counts approximate).
pub fn estimate_tokens(text: &str) -> usize {
    (text.len() as f64 / 4.0).ceil() as usize
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Filter a command's raw output using the built-in filters.
///
/// Returns the raw output unchanged when no filter matches the command, the
/// filter panics, or filtering would produce more tokens than the raw output.
///
/// When filtering drops lines, or the command failed (`exit_code != 0`), a
/// recovery file is written and a hint (`[full output: …]` / `[see remaining:
/// tail -n +N …]`) is appended so the model can read what was cut without
/// re-running the command.
pub fn filter_command_output(command: &str, raw: &str, exit_code: i32) -> String {
    // A filter is pure regex/parse work, but a pathological input could still
    // panic; never let that cost the user their output.
    // 1. Rust-native filters (stateful summaries TOML cannot express). A native
    //    filter always rewrites the whole output, so recovery treats it as Whole.
    if let Ok(Some(filtered)) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        native::try_filter(command, raw)
    })) {
        return finalize(raw, filtered, command, exit_code, Lossiness::Whole);
    }
    // 2. TOML pipeline.
    if !command_matches_filter(command) {
        return raw.to_string();
    }
    let Some(filter) = find_matching_filter(command) else {
        return raw.to_string();
    };
    let (filtered, loss) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        apply_filter_with_info(filter, raw)
    })) {
        Ok(pair) => pair,
        Err(_) => return raw.to_string(),
    };
    finalize(raw, filtered, command, exit_code, loss)
}

/// Apply the never-worse guard and append a recovery hint. When filtering would
/// grow the output, keep the raw text and treat it as lossless — the full output
/// is already present, so no hint is needed (only a failed command still earns a
/// full-output copy).
fn finalize(raw: &str, filtered: String, command: &str, exit_code: i32, loss: Lossiness) -> String {
    let filtered_kept = estimate_tokens(&filtered) <= estimate_tokens(raw);
    let (body, effective_loss): (&str, Lossiness) = if filtered_kept {
        (&filtered, loss)
    } else {
        (raw, Lossiness::None)
    };
    match tee::recover_hint(raw, command, exit_code, &effective_loss) {
        Some(hint) => format!("{body}\n{hint}"),
        None => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_codes() {
        assert_eq!(strip_ansi("\x1b[31mError\x1b[0m"), "Error");
    }

    #[test]
    fn truncate_unicode_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("日本語xyz", 5), "日本...");
        assert_eq!(truncate("abcdef", 2), "...");
    }

    #[test]
    fn estimate_tokens_is_bytes_over_four() {
        assert_eq!(estimate_tokens("hello world"), 3);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn unmatched_command_passes_through() {
        let raw = "some random output\nwith lines";
        assert_eq!(filter_command_output("frobnicate --xyz", raw, 0), raw);
    }

    #[test]
    fn matched_command_is_filtered() {
        let raw = "make[1]: Entering directory '/x'\ngcc -c foo.c\nmake[1]: Leaving directory '/x'";
        let out = filter_command_output("make all", raw, 0);
        assert!(!out.contains("Entering directory"));
        assert!(out.contains("gcc -c foo.c"));
    }

    #[test]
    fn never_worse_reverts_growth() {
        // A tiny output a filter could only pad; guard keeps it raw.
        let raw = "ok";
        assert_eq!(filter_command_output("make all", raw, 0), raw);
    }
}
