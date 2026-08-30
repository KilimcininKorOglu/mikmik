//! Raw-output recovery: saves the unfiltered output to disk and returns a hint
//! the model can act on to read the part the filter dropped.
//!
//! Ported from RTK (Rust Token Killer), stripped of its per-project config and
//! env overrides. Two triggers write a tee file: a command that failed
//! (`exit_code != 0`), so the model can read the full raw output, and a filter
//! that dropped lines (`Lossiness::Tail` / `Lossiness::Whole`), so the model can
//! recover exactly what was cut. A successful, lossless command writes nothing.

use super::toml_filter::Lossiness;
use mikmik_core::config::Settings;
use std::path::{Path, PathBuf};

/// Outputs smaller than this need no recovery file; the filtered result already
/// carries everything worth keeping.
const MIN_TEE_SIZE: usize = 500;

/// Keep only the most recent files in the tee directory.
const MAX_FILES: usize = 20;

/// Cap a single tee file at 1 MiB.
const MAX_FILE_SIZE: usize = 1_048_576;

// ---------------------------------------------------------------------------
// Filename slug
// ---------------------------------------------------------------------------

/// Sanitize a command slug for use in a filename. Non-alphanumeric characters
/// (except `_` and `-`) become `_`. A long slug (usually an embedded path that
/// duplicates the command) collapses to a short readable prefix plus a short
/// hash, keeping the filename unique but compact.
fn sanitize_slug(slug: &str) -> String {
    let sanitized: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    const MAX_READABLE: usize = 24;
    if sanitized.len() <= MAX_READABLE {
        return sanitized;
    }
    let prefix: String = sanitized.chars().take(8).collect();
    format!("{}_{}", prefix, short_hash(&sanitized))
}

/// First 6 hex chars of the SHA-256 of `s` — a compact tag that keeps shortened
/// slugs distinct. Not collision-resistant alone, but a clash also needs the
/// identical readable prefix and the same epoch second, which scopes it safely.
fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(s.as_bytes()))[..6].to_string()
}

/// The command's first token, used as the readable part of the slug.
fn command_slug(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or("cmd")
}

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// The tee directory under the config root (`<config_dir>/tee`).
fn tee_dir() -> PathBuf {
    Settings::config_dir().join("tee")
}

/// Create a directory owner-only (`0o700` on Unix). A best-effort helper: a
/// failure here surfaces as the caller returning `None`, never a panic.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Open a file owner-only (`0o600` on Unix) for writing, truncating.
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Truncate `raw` to `max_file_size` bytes on a UTF-8 char boundary, appending a
/// marker when a cut happened.
fn cap_content(raw: &str, max_file_size: usize) -> String {
    if raw.len() <= max_file_size {
        return raw.to_string();
    }
    let boundary = raw
        .char_indices()
        .take_while(|(i, _)| *i < max_file_size)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!(
        "{}\n\n--- truncated at {} bytes ---",
        &raw[..boundary],
        max_file_size
    )
}

/// Keep only the last `max_files` `.log` files, deleting the oldest. Filenames
/// begin with the epoch second, so a filename sort is chronological.
fn cleanup_old_files(dir: &Path, max_files: usize) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
        .collect();

    if entries.len() <= max_files {
        return;
    }
    entries.sort_by_key(|e| e.file_name());
    let to_remove = entries.len() - max_files;
    for entry in entries.iter().take(to_remove) {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Write `raw` to `<dir>/<epoch>_<slug>.log`, returning the path on success.
fn write_tee_file(raw: &str, slug: &str, dir: &Path) -> Option<PathBuf> {
    create_private_dir(dir).ok()?;

    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let filepath = dir.join(format!("{}_{}.log", epoch, sanitize_slug(slug)));

    let content = cap_content(raw, MAX_FILE_SIZE);
    let mut file = open_private(&filepath).ok()?;
    use std::io::Write;
    file.write_all(content.as_bytes()).ok()?;

    cleanup_old_files(dir, MAX_FILES);
    Some(filepath)
}

// ---------------------------------------------------------------------------
// Hint formatting (shell-safe path quoting)
// ---------------------------------------------------------------------------

fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(relative) = path.strip_prefix(&home) {
            return format!("~/{}", relative.display());
        }
    }
    path.display().to_string()
}

fn needs_shell_quoting(path: &str) -> bool {
    path.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '\'' | '"'
                    | '\\'
                    | '$'
                    | '`'
                    | '!'
                    | '#'
                    | '&'
                    | '('
                    | ')'
                    | ';'
                    | '<'
                    | '>'
                    | '?'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '|'
                    | '*'
            )
    })
}

fn escape_double_quoted_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '\\' | '"' | '$' | '`') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn display_shell_path(path: &Path) -> String {
    let display = display_path(path);
    if !needs_shell_quoting(&display) {
        return display;
    }
    if let Some(relative) = display.strip_prefix("~/") {
        let relative = relative.replace(std::path::MAIN_SEPARATOR, "/");
        return format!("\"$HOME/{}\"", escape_double_quoted_path(&relative));
    }
    format!("\"{}\"", escape_double_quoted_path(&display))
}

fn full_hint(path: &Path) -> String {
    format!("[full output: {}]", display_shell_path(path))
}

fn tail_hint(path: &Path, line_offset: usize) -> String {
    format!(
        "[see remaining: tail -n +{} {}]",
        line_offset,
        display_shell_path(path)
    )
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// The payload to tee and how to point at it, decided from the loss and exit code.
enum Recovery<'a> {
    /// The whole raw output; the model reads it all.
    Full(&'a str),
    /// The dropped tail of `payload`, recovered with `tail -n +offset`.
    Tail { payload: &'a str, offset: usize },
    /// Nothing to recover.
    None,
}

fn plan_recovery<'a>(raw: &'a str, exit_code: i32, loss: &'a Lossiness) -> Recovery<'a> {
    match loss {
        Lossiness::Tail {
            tee_payload,
            tail_offset,
        } => Recovery::Tail {
            payload: tee_payload,
            offset: *tail_offset,
        },
        Lossiness::Whole => Recovery::Full(raw),
        // Lossless, but a failed command still earns a full-output copy.
        Lossiness::None if exit_code != 0 => Recovery::Full(raw),
        Lossiness::None => Recovery::None,
    }
}

/// Write a recovery file if the loss or the exit code warrants one, returning the
/// hint to append to the filtered output. `None` when nothing was written.
pub fn recover_hint(raw: &str, command: &str, exit_code: i32, loss: &Lossiness) -> Option<String> {
    let (payload, is_tail, offset) = match plan_recovery(raw, exit_code, loss) {
        Recovery::None => return None,
        Recovery::Full(p) => (p, false, 0),
        Recovery::Tail { payload, offset } => (payload, true, offset),
    };
    if payload.len() < MIN_TEE_SIZE {
        return None;
    }
    let path = write_tee_file(payload, command_slug(command), &tee_dir())?;
    Some(if is_tail {
        tail_hint(&path, offset)
    } else {
        full_hint(&path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_slug_basic() {
        assert_eq!(sanitize_slug("cargo_test"), "cargo_test");
        assert_eq!(sanitize_slug("cargo-test"), "cargo-test");
    }

    #[test]
    fn sanitize_slug_shortens_long() {
        let long = format!("grep_0_{}", "a".repeat(50));
        let short = sanitize_slug(&long);
        assert!(
            short.len() < 24,
            "long slug should shorten, got '{}'",
            short
        );
        assert!(short.starts_with("grep_0_a"));
        // Deterministic and collision-free across distinct slugs.
        assert_eq!(sanitize_slug(&long), short);
        let other = sanitize_slug(&format!("grep_1_{}", "a".repeat(50)));
        assert_ne!(other, short);
    }

    #[test]
    fn command_slug_first_token() {
        assert_eq!(command_slug("cargo test --all"), "cargo");
        assert_eq!(command_slug(""), "cmd");
    }

    #[test]
    fn cap_content_truncates_on_boundary() {
        let japanese = "\u{6F22}".repeat(333); // 999 bytes of 3-byte chars
        let capped = cap_content(&japanese, 998);
        assert!(capped.contains("--- truncated at 998 bytes ---"));
        assert!(capped.starts_with(&"\u{6F22}".repeat(332)));
    }

    #[test]
    fn cap_content_keeps_small() {
        assert_eq!(cap_content("small", 100), "small");
    }

    #[test]
    fn write_tee_file_creates_and_reads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let content = "error: test failed\n".repeat(50);
        let path = write_tee_file(&content, "cargo_test", tmp.path()).expect("written");
        assert!(path.exists());
        let read = std::fs::read_to_string(&path).expect("read");
        assert!(read.contains("error: test failed"));
    }

    #[test]
    #[cfg(unix)]
    fn write_tee_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("tee");
        let path = write_tee_file("secret output\n", "grep", &dir).expect("written");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "tee file must be owner-only");
        assert_eq!(mode(&dir), 0o700, "tee dir must be owner-only");
    }

    #[test]
    fn cleanup_keeps_last_n() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        for i in 0..25 {
            let name = format!("{:010}_test.log", 1_000_000 + i);
            std::fs::write(dir.join(&name), "content").expect("write");
        }
        cleanup_old_files(dir, 20);
        let remaining = std::fs::read_dir(dir).expect("read_dir").count();
        assert_eq!(remaining, 20);
        assert!(!dir.join(format!("{:010}_test.log", 1_000_000)).exists());
        assert!(dir.join(format!("{:010}_test.log", 1_000_024)).exists());
    }

    #[test]
    fn display_shell_path_quotes_spaces() {
        let path = PathBuf::from("/tmp/mikmik/Application Support/123_go_test.log");
        assert_eq!(
            display_shell_path(&path),
            "\"/tmp/mikmik/Application Support/123_go_test.log\""
        );
    }

    #[test]
    fn display_shell_path_keeps_simple() {
        let path = PathBuf::from("/tmp/mikmik/tee/123_cargo_test.log");
        assert_eq!(
            display_shell_path(&path),
            "/tmp/mikmik/tee/123_cargo_test.log"
        );
    }

    #[test]
    fn full_hint_format() {
        let hint = full_hint(&PathBuf::from("/tmp/mikmik/tee/123_cargo_test.log"));
        assert!(hint.starts_with("[full output: "));
        assert!(hint.ends_with(']'));
        assert!(hint.contains("123_cargo_test.log"));
    }

    #[test]
    fn tail_hint_format() {
        let hint = tail_hint(&PathBuf::from("/tmp/mikmik/tee/123_make.log"), 22);
        assert!(hint.starts_with("[see remaining: tail -n +22 "));
        assert!(hint.ends_with(']'));
    }

    #[test]
    fn plan_lossless_success_recovers_nothing() {
        assert!(matches!(
            plan_recovery("x".repeat(1000).as_str(), 0, &Lossiness::None),
            Recovery::None
        ));
    }

    #[test]
    fn plan_lossless_failure_takes_full() {
        let raw = "x".repeat(1000);
        assert!(matches!(
            plan_recovery(&raw, 1, &Lossiness::None),
            Recovery::Full(_)
        ));
    }

    #[test]
    fn plan_whole_takes_full() {
        let raw = "x".repeat(1000);
        assert!(matches!(
            plan_recovery(&raw, 0, &Lossiness::Whole),
            Recovery::Full(_)
        ));
    }

    #[test]
    fn plan_tail_takes_tail() {
        let loss = Lossiness::Tail {
            tee_payload: "a\nb\nc".to_string(),
            tail_offset: 3,
        };
        assert!(matches!(
            plan_recovery("ignored", 0, &loss),
            Recovery::Tail { offset: 3, .. }
        ));
    }

    #[test]
    fn recover_hint_skips_small_payload() {
        // Below MIN_TEE_SIZE, no file is written even on failure.
        assert!(recover_hint("tiny", "cargo test", 1, &Lossiness::None).is_none());
    }

    #[test]
    fn recover_hint_lossless_success_none() {
        let raw = "x".repeat(1000);
        assert!(recover_hint(&raw, "cargo test", 0, &Lossiness::None).is_none());
    }
}
