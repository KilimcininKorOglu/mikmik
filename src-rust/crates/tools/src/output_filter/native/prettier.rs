//! Prettier output filter: show only the files that need formatting.
//!
//! Ported from RTK. Only the pure `filter(&str) -> String` is kept.

/// Max files listed before collapsing to a "+N more" line (RTK's CAP_WARNINGS).
const MAX_PRETTIER_FILES: usize = 10;

const FORMATTABLE_EXTS: [&str; 8] = [
    ".ts", ".tsx", ".js", ".jsx", ".json", ".md", ".css", ".scss",
];

/// A non-empty line naming a source file prettier flagged.
fn is_file_line(trimmed: &str) -> bool {
    if trimmed.is_empty()
        || trimmed.starts_with("Checking")
        || trimmed.starts_with("All matched")
        || trimmed.starts_with("Code style")
        || trimmed.contains("[warn]")
        || trimmed.contains("[error]")
    {
        return false;
    }
    FORMATTABLE_EXTS.iter().any(|ext| trimmed.ends_with(ext))
}

/// Files prettier flagged, and how many it reported as already formatted.
fn collect(output: &str) -> (Vec<String>, usize) {
    let mut files: Vec<String> = Vec::new();
    let mut checked = 0;
    for line in output.lines() {
        let trimmed = line.trim();
        if is_file_line(trimmed) {
            files.push(trimmed.to_string());
        }
        if trimmed.contains("All matched files use Prettier") {
            if let Some(count) = trimmed
                .split_whitespace()
                .next()
                .and_then(|w| w.parse().ok())
            {
                checked = count;
            }
        }
    }
    (files, checked)
}

fn render_check_mode(files: &[String], checked: usize) -> String {
    if files.is_empty() {
        return "Prettier: All files formatted correctly".to_string();
    }
    let mut result = format!("Prettier: {} files need formatting\n", files.len());
    for (i, file) in files.iter().take(MAX_PRETTIER_FILES).enumerate() {
        result.push_str(&format!("{}. {}\n", i + 1, file));
    }
    if files.len() > MAX_PRETTIER_FILES {
        result.push_str(&format!(
            "\n... +{} more files\n",
            files.len() - MAX_PRETTIER_FILES
        ));
    }
    if checked > files.len() {
        result.push_str(&format!(
            "\n{} files already formatted\n",
            checked - files.len()
        ));
    }
    result.trim().to_string()
}

/// Compress prettier output to the files that still need formatting.
pub fn filter(output: &str) -> String {
    if output.trim().is_empty() {
        return "Error: prettier produced no output".to_string();
    }

    let (files, checked) = collect(output);

    if files.is_empty() && output.contains("All matched files use Prettier") {
        return "Prettier: All files formatted correctly".to_string();
    }

    // Write mode: prettier reformatted files in place.
    if output.contains("modified") || output.contains("formatted") {
        return format!("Prettier: {} files formatted", files.len());
    }

    render_check_mode(&files, checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_formatted() {
        let output = "Checking formatting...\nAll matched files use Prettier code style!";
        let result = filter(output);
        assert!(result.contains("All files formatted correctly"));
    }

    #[test]
    fn files_need_formatting() {
        let output = "\
Checking formatting...
src/components/ui/button.tsx
src/lib/auth/session.ts
src/pages/dashboard.tsx
Code style issues found in the above file(s). Forgot to run Prettier?";
        let result = filter(output);
        assert!(result.contains("3 files need formatting"));
        assert!(result.contains("button.tsx"));
        assert!(result.contains("session.ts"));
    }

    #[test]
    fn many_files_collapse() {
        let mut output = String::from("Checking formatting...\n");
        for i in 0..15 {
            output.push_str(&format!("src/file{}.ts\n", i));
        }
        let result = filter(&output);
        assert!(result.contains("15 files need formatting"));
        assert!(result.contains("... +5 more files"));
    }

    #[test]
    fn empty_output_is_error() {
        assert!(filter("").contains("Error"));
        assert!(!filter("").contains("All files formatted"));
    }

    #[test]
    fn whitespace_only_is_error() {
        let result = filter("   \n\n  ");
        assert!(result.contains("Error"));
    }
}
