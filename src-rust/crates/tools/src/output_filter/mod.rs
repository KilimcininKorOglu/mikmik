//! Command-output filtering, ported from RTK (Rust Token Killer).
//!
//! Compresses a command's stdout before it reaches the LLM context. A
//! command-aware, TOML-defined pipeline (see `toml_filter`) shrinks noisy output
//! (make, terraform, ping, …) by 60-90% while preserving the parts that matter.
//!
//! Safety nets, all from RTK: the `never_worse` guard reverts to raw output when
//! filtering would grow it, `catch_unwind` reverts when a filter panics, and a
//! command with no matching filter passes through untouched. Filtering can only
//! ever help or no-op.

mod guard;
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
pub fn filter_command_output(command: &str, raw: &str) -> String {
    if !command_matches_filter(command) {
        return raw.to_string();
    }
    let Some(filter) = find_matching_filter(command) else {
        return raw.to_string();
    };
    // A filter is pure regex work, but a pathological pattern could still panic;
    // never let that cost the user their output.
    let filtered = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        apply_filter(filter, raw)
    })) {
        Ok(s) => s,
        Err(_) => return raw.to_string(),
    };
    never_worse(raw, &filtered).to_string()
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
        assert_eq!(filter_command_output("frobnicate --xyz", raw), raw);
    }

    #[test]
    fn matched_command_is_filtered() {
        let raw = "make[1]: Entering directory '/x'\ngcc -c foo.c\nmake[1]: Leaving directory '/x'";
        let out = filter_command_output("make all", raw);
        assert!(!out.contains("Entering directory"));
        assert!(out.contains("gcc -c foo.c"));
    }

    #[test]
    fn never_worse_reverts_growth() {
        // A tiny output a filter could only pad; guard keeps it raw.
        let raw = "ok";
        assert_eq!(filter_command_output("make all", raw), raw);
    }
}
