//! TypeScript compiler (tsc) output filter, grouping errors by file and code.
//!
//! Ported from RTK. Only the pure `filter(&str) -> String` is kept; RTK's
//! process-spawning `run()` and streaming `BlockHandler` are dropped because the
//! Bash tool already hands us the whole output.

use crate::output_filter::truncate;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

static TSC_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?)\((\d+),(\d+)\):\s+(error|warning)\s+(TS\d+):\s+(.+)$")
        .expect("tsc error regex literal is valid")
});

struct TsError {
    file: String,
    line: usize,
    code: String,
    message: String,
    context_lines: Vec<String>,
}

/// Parse tsc diagnostics into grouped-by-file errors.
fn collect_errors(output: &str) -> Vec<TsError> {
    let mut errors: Vec<TsError> = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let Some(caps) = TSC_ERROR.captures(lines[i]) else {
            i += 1;
            continue;
        };
        let mut err = TsError {
            file: caps[1].to_string(),
            line: caps[2].parse().unwrap_or(0),
            code: caps[5].to_string(),
            message: caps[6].to_string(),
            context_lines: Vec::new(),
        };
        // Capture indented continuation context from tsc.
        i += 1;
        while i < lines.len() {
            let next = lines[i];
            if !next.is_empty()
                && (next.starts_with("  ") || next.starts_with('\t'))
                && !TSC_ERROR.is_match(next)
            {
                err.context_lines.push(next.trim().to_string());
                i += 1;
            } else {
                break;
            }
        }
        errors.push(err);
    }
    errors
}

/// Render the top-error-codes line when 2+ distinct codes are present.
fn top_codes_line(errors: &[TsError]) -> Option<String> {
    let mut by_code: HashMap<&str, usize> = HashMap::new();
    for err in errors {
        *by_code.entry(err.code.as_str()).or_insert(0) += 1;
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

/// Compress tsc output into a per-file error summary. Every error is kept.
pub fn filter(output: &str) -> String {
    let errors = collect_errors(output);

    if errors.is_empty() {
        if output.contains("Found 0 errors") {
            return "TypeScript: No errors found".to_string();
        }
        return "TypeScript compilation completed".to_string();
    }

    let mut by_file: HashMap<String, Vec<&TsError>> = HashMap::new();
    for err in &errors {
        by_file.entry(err.file.clone()).or_default().push(err);
    }

    let mut result = format!(
        "TypeScript: {} errors in {} files\n",
        errors.len(),
        by_file.len()
    );
    if let Some(line) = top_codes_line(&errors) {
        result.push_str(&line);
    }

    // Files sorted by error count, most first.
    let mut files_sorted: Vec<_> = by_file.iter().collect();
    files_sorted.sort_by_key(|b| std::cmp::Reverse(b.1.len()));

    for (file, file_errors) in &files_sorted {
        result.push_str(&format!("{} ({} errors)\n", file, file_errors.len()));
        for err in *file_errors {
            result.push_str(&format!(
                "  L{}: {} {}\n",
                err.line,
                err.code,
                truncate(&err.message, 120)
            ));
            for ctx in &err.context_lines {
                result.push_str(&format!("    {}\n", truncate(ctx, 120)));
            }
        }
        result.push('\n');
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_errors_by_file() {
        let output = "\
src/server/api/auth.ts(12,5): error TS2322: Type 'string' is not assignable to type 'number'.
src/server/api/auth.ts(15,10): error TS2345: Argument of type 'number' is not assignable to parameter of type 'string'.
src/components/Button.tsx(8,3): error TS2339: Property 'onClick' does not exist on type 'ButtonProps'.
src/components/Button.tsx(10,5): error TS2322: Type 'string' is not assignable to type 'number'.

Found 4 errors in 2 files.
";
        let result = filter(output);
        assert!(result.contains("TypeScript: 4 errors in 2 files"));
        assert!(result.contains("auth.ts (2 errors)"));
        assert!(result.contains("Button.tsx (2 errors)"));
        assert!(result.contains("TS2322"));
        assert!(!result.contains("Found 4 errors"));
    }

    #[test]
    fn every_error_message_shown() {
        let output = "\
src/api.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.
src/api.ts(20,5): error TS2322: Type 'boolean' is not assignable to type 'string'.
src/api.ts(30,5): error TS2322: Type 'null' is not assignable to type 'object'.
";
        let result = filter(output);
        assert!(result.contains("L10:"));
        assert!(result.contains("L20:"));
        assert!(result.contains("L30:"));
    }

    #[test]
    fn continuation_lines_preserved() {
        let output = "\
src/app.tsx(10,3): error TS2322: Type '{ children: Element; }' is not assignable to type 'Props'.
  Property 'children' does not exist on type 'Props'.
src/app.tsx(20,5): error TS2345: Argument of type 'number' is not assignable to parameter of type 'string'.
";
        let result = filter(output);
        assert!(result.contains("Property 'children' does not exist on type 'Props'"));
        assert!(result.contains("L10:"));
        assert!(result.contains("L20:"));
    }

    #[test]
    fn no_file_limit() {
        let mut output = String::new();
        for i in 1..=15 {
            output.push_str(&format!(
                "src/file{}.ts({},1): error TS2322: Error in file {}.\n",
                i, i, i
            ));
        }
        let result = filter(&output);
        assert!(result.contains("15 errors in 15 files"));
        for i in 1..=15 {
            assert!(
                result.contains(&format!("file{}.ts", i)),
                "file{}.ts missing",
                i
            );
        }
    }

    #[test]
    fn no_errors() {
        let result = filter("Found 0 errors. Watching for file changes.");
        assert!(result.contains("No errors found"));
    }
}
