//! mypy type-checking output filter, grouping errors by file.
//!
//! Ported from RTK. Only the pure `filter(&str) -> String` is kept.

use crate::output_filter::truncate;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

const MYPY_CLEAN: &str = "mypy: No issues found";

// file.py:12: error: Message [error-code]  (column is optional)
static MYPY_DIAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?):(\d+)(?::\d+)?: (error|warning|note): (.+?)(?:\s+\[(.+)\])?$")
        .expect("mypy diag regex literal is valid")
});

struct MypyError {
    file: String,
    line: usize,
    code: String,
    message: String,
    context_lines: Vec<String>,
}

/// Attach a `note:` line to the preceding error of the same file, or record it
/// as a standalone fileless line. Returns whether it was consumed as a note.
fn absorb_note(
    file: &str,
    message: String,
    raw_line: &str,
    errors: &mut [MypyError],
    fileless: &mut Vec<String>,
) {
    if let Some(last) = errors.last_mut() {
        if last.file == file {
            last.context_lines.push(message);
            return;
        }
    }
    fileless.push(raw_line.to_string());
}

/// Pull the trailing `note:` continuation lines that belong to `err`.
fn take_notes(err: &mut MypyError, lines: &[&str], i: &mut usize) {
    while *i < lines.len() {
        let Some(caps) = MYPY_DIAG.captures(lines[*i]) else {
            break;
        };
        if &caps[3] == "note" && caps[1] == err.file {
            err.context_lines.push(caps[4].to_string());
            *i += 1;
        } else {
            break;
        }
    }
}

fn is_skippable(line: &str) -> bool {
    (line.starts_with("Found ") && line.contains(" error")) || line.starts_with("Success:")
}

fn collect(output: &str) -> (Vec<MypyError>, Vec<String>) {
    let lines: Vec<&str> = output.lines().collect();
    let mut errors: Vec<MypyError> = Vec::new();
    let mut fileless: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if is_skippable(line) {
            i += 1;
            continue;
        }
        let Some(caps) = MYPY_DIAG.captures(line) else {
            if line.contains("error:") && !line.trim().is_empty() {
                fileless.push(line.to_string());
            }
            i += 1;
            continue;
        };
        let file = caps[1].to_string();
        let message = caps[4].to_string();
        if &caps[3] == "note" {
            absorb_note(&file, message, line, &mut errors, &mut fileless);
            i += 1;
            continue;
        }
        let mut err = MypyError {
            file,
            line: caps[2].parse().unwrap_or(0),
            code: caps
                .get(5)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            message,
            context_lines: Vec::new(),
        };
        i += 1;
        take_notes(&mut err, &lines, &mut i);
        errors.push(err);
    }
    (errors, fileless)
}

fn top_codes_line(errors: &[MypyError]) -> Option<String> {
    let mut by_code: HashMap<&str, usize> = HashMap::new();
    for err in errors {
        if !err.code.is_empty() {
            *by_code.entry(err.code.as_str()).or_insert(0) += 1;
        }
    }
    if by_code.len() <= 1 {
        return None;
    }
    let mut counts: Vec<_> = by_code.into_iter().collect();
    counts.sort_by_key(|c| std::cmp::Reverse(c.1));
    let codes_str: Vec<String> = counts
        .iter()
        .take(5)
        .map(|(code, count)| format!("{} ({}x)", code, count))
        .collect();
    Some(format!("Top codes: {}\n\n", codes_str.join(", ")))
}

fn render_error(result: &mut String, err: &MypyError) {
    if err.code.is_empty() {
        result.push_str(&format!(
            "  L{}: {}\n",
            err.line,
            truncate(&err.message, 120)
        ));
    } else {
        result.push_str(&format!(
            "  L{}: [{}] {}\n",
            err.line,
            err.code,
            truncate(&err.message, 120)
        ));
    }
    for ctx in &err.context_lines {
        result.push_str(&format!("    {}\n", truncate(ctx, 120)));
    }
}

/// Compress mypy output into a per-file error summary. Every error is kept.
pub fn filter(output: &str) -> String {
    let (errors, fileless) = collect(output);

    if errors.is_empty() && fileless.is_empty() {
        return MYPY_CLEAN.to_string();
    }

    let mut by_file: HashMap<String, Vec<&MypyError>> = HashMap::new();
    for err in &errors {
        by_file.entry(err.file.clone()).or_default().push(err);
    }

    let mut result = String::new();
    for line in &fileless {
        result.push_str(line);
        result.push('\n');
    }
    if !fileless.is_empty() && !errors.is_empty() {
        result.push('\n');
    }

    if !errors.is_empty() {
        result.push_str(&format!(
            "mypy: {} errors in {} files\n",
            errors.len(),
            by_file.len()
        ));
        if let Some(line) = top_codes_line(&errors) {
            result.push_str(&line);
        }
        let mut files_sorted: Vec<_> = by_file.iter().collect();
        files_sorted.sort_by_key(|b| std::cmp::Reverse(b.1.len()));
        for (file, file_errors) in &files_sorted {
            result.push_str(&format!("{} ({} errors)\n", file, file_errors.len()));
            for err in *file_errors {
                render_error(&mut result, err);
            }
            result.push('\n');
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_grouped_by_file() {
        let output = "\
src/server/auth.py:12: error: Incompatible return value type  [return-value]
src/server/auth.py:15: error: Argument 1 has incompatible type  [arg-type]
src/models/user.py:8: error: Name \"foo\" is not defined  [name-defined]
src/models/user.py:10: error: Incompatible types in assignment  [assignment]
src/models/user.py:20: error: Missing return statement  [return]
Found 5 errors in 2 files (checked 10 source files)
";
        let result = filter(output);
        assert!(result.contains("mypy: 5 errors in 2 files"));
        let user_pos = result.find("user.py").expect("user.py present");
        let auth_pos = result.find("auth.py").expect("auth.py present");
        assert!(user_pos < auth_pos, "user.py (3) before auth.py (2)");
    }

    #[test]
    fn column_numbers_and_code() {
        let output = "src/api.py:10:5: error: Incompatible return value type  [return-value]\n";
        let result = filter(output);
        assert!(result.contains("L10:"));
        assert!(result.contains("[return-value]"));
    }

    #[test]
    fn top_codes_when_multiple() {
        let output = "\
a.py:1: error: Error one  [return-value]
a.py:2: error: Error two  [return-value]
a.py:3: error: Error three  [return-value]
b.py:1: error: Error four  [name-defined]
c.py:1: error: Error five  [arg-type]
Found 5 errors in 3 files
";
        let result = filter(output);
        assert!(result.contains("Top codes:"));
        assert!(result.contains("return-value (3x)"));
    }

    #[test]
    fn single_code_no_summary() {
        let output = "\
a.py:1: error: Error one  [return-value]
b.py:1: error: Error two  [return-value]
";
        let result = filter(output);
        assert!(!result.contains("Top codes:"));
    }

    #[test]
    fn note_continuation() {
        let output = "\
src/app.py:10: error: Incompatible types in assignment  [assignment]
src/app.py:10: note: Expected type \"int\"
src/app.py:10: note: Got type \"str\"
src/app.py:20: error: Missing return statement  [return]
";
        let result = filter(output);
        assert!(result.contains("Expected type \"int\""));
        assert!(result.contains("Got type \"str\""));
        assert!(result.contains("L20:"));
    }

    #[test]
    fn fileless_errors_first() {
        let output = "\
mypy: error: No module named 'nonexistent'
src/api.py:10: error: Name \"foo\" is not defined  [name-defined]
Found 1 error in 1 file
";
        let result = filter(output);
        let fileless_pos = result.find("No module named").expect("fileless present");
        let grouped_pos = result.find("api.py").expect("grouped present");
        assert!(fileless_pos < grouped_pos);
    }

    #[test]
    fn no_issues() {
        let result = filter("Success: no issues found in 5 source files\n");
        assert_eq!(result, MYPY_CLEAN);
    }
}
