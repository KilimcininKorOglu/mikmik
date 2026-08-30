//! pytest output filter: show only failures and the summary line.
//!
//! Ported from RTK. Only the pure `filter(&str) -> String` is kept; RTK's inline
//! `force_tee` hints are dropped because the caller already tees the full raw
//! output once when a native filter runs.

use crate::output_filter::truncate;

const PYTEST_NO_TESTS: &str = "Pytest: No tests collected";
/// Max failures / xfail entries listed before collapsing (RTK's CAP_WARNINGS).
const MAX_LISTED: usize = 10;

#[derive(Debug, PartialEq)]
enum ParseState {
    Header,
    TestProgress,
    Failures,
    Summary,
}

#[derive(Default)]
struct Accum {
    test_files: Vec<String>,
    failures: Vec<String>,
    current_failure: Vec<String>,
    xfail_lines: Vec<String>,
    summary_line: String,
}

impl Accum {
    fn flush_current(&mut self) {
        if !self.current_failure.is_empty() {
            self.failures.push(self.current_failure.join("\n"));
            self.current_failure.clear();
        }
    }
}

#[derive(Default)]
struct PytestCounts {
    passed: usize,
    failed: usize,
    skipped: usize,
    xfailed: usize,
    xpassed: usize,
}

// --- parsing ---------------------------------------------------------------

/// A bare "-q" summary such as "5 failed, 1698 passed in 108.89s".
fn is_quiet_summary(trimmed: &str, summary_line: &str) -> bool {
    summary_line.is_empty()
        && !trimmed.starts_with("===")
        && !trimmed.starts_with("FAILED")
        && !trimmed.starts_with("ERROR")
        && (trimmed.contains(" passed")
            || trimmed.contains(" failed")
            || trimmed.contains(" skipped"))
        && trimmed.contains(" in ")
}

/// Handle a section/summary transition line. Returns true when the line was
/// consumed and the caller should move on.
fn try_transition(trimmed: &str, state: &mut ParseState, acc: &mut Accum) -> bool {
    if trimmed.starts_with("===") {
        if trimmed.contains("test session starts") {
            *state = ParseState::Header;
        } else if trimmed.contains("FAILURES") {
            *state = ParseState::Failures;
        } else if trimmed.contains("short test summary") {
            *state = ParseState::Summary;
            acc.flush_current();
        } else if trimmed.contains("passed")
            || trimmed.contains("failed")
            || trimmed.contains("skipped")
        {
            acc.summary_line = trimmed.to_string();
        } else {
            return false;
        }
        return true;
    }
    if is_quiet_summary(trimmed, &acc.summary_line) {
        acc.summary_line = trimmed.to_string();
        return true;
    }
    false
}

fn handle_failures_line(trimmed: &str, acc: &mut Accum) {
    if trimmed.starts_with("___") {
        acc.flush_current();
        acc.current_failure.push(trimmed.to_string());
    } else if !trimmed.is_empty() && !trimmed.starts_with("===") {
        acc.current_failure.push(trimmed.to_string());
    }
}

fn handle_summary_line(trimmed: &str, acc: &mut Accum) {
    if trimmed.starts_with("FAILED") || trimmed.starts_with("ERROR") {
        acc.failures.push(trimmed.to_string());
    } else if trimmed.starts_with("XFAIL") || trimmed.starts_with("XPASS") {
        acc.xfail_lines.push(trimmed.to_string());
    }
}

fn process_by_state(trimmed: &str, state: &mut ParseState, acc: &mut Accum) {
    match state {
        ParseState::Header => {
            if trimmed.starts_with("collected") {
                *state = ParseState::TestProgress;
            }
        }
        ParseState::TestProgress => {
            if !trimmed.is_empty()
                && !trimmed.starts_with("===")
                && (trimmed.contains(".py") || trimmed.contains("%]"))
            {
                acc.test_files.push(trimmed.to_string());
            }
        }
        ParseState::Failures => handle_failures_line(trimmed, acc),
        ParseState::Summary => handle_summary_line(trimmed, acc),
    }
}

fn parse_summary_line(summary: &str) -> PytestCounts {
    let mut counts = PytestCounts::default();
    for part in summary.split(',') {
        let words: Vec<&str> = part.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            if i == 0 {
                continue;
            }
            let Ok(n) = words[i - 1].parse::<usize>() else {
                continue;
            };
            // Order matters: "xpassed"/"xfailed" contain "passed"/"failed".
            if word.contains("xpassed") {
                counts.xpassed = n;
            } else if word.contains("xfailed") {
                counts.xfailed = n;
            } else if word.contains("passed") {
                counts.passed = n;
            } else if word.contains("failed") {
                counts.failed = n;
            } else if word.contains("skipped") {
                counts.skipped = n;
            }
        }
    }
    counts
}

// --- rendering -------------------------------------------------------------

fn counts_header(counts: &PytestCounts, xfail_lines_present: bool) -> Option<String> {
    let PytestCounts {
        passed,
        failed,
        skipped,
        xfailed,
        xpassed,
    } = *counts;

    if passed == 0 && failed == 0 && skipped == 0 && xfailed == 0 && xpassed == 0 {
        return None;
    }
    let extras = skipped > 0 || xfailed > 0 || xpassed > 0 || xfail_lines_present;
    if failed == 0 && passed > 0 && !extras {
        return Some(format!("Pytest: {} passed", passed));
    }
    let mut line = format!("Pytest: {} passed, {} failed", passed, failed);
    if skipped > 0 {
        line.push_str(&format!(", {} skipped", skipped));
    }
    if xfailed > 0 {
        line.push_str(&format!(", {} xfailed", xfailed));
    }
    if xpassed > 0 {
        line.push_str(&format!(", {} xpassed", xpassed));
    }
    Some(line)
}

fn render_xfail(result: &mut String, xfail_lines: &[String]) {
    if xfail_lines.is_empty() {
        return;
    }
    result.push_str("\nExpected-failure outcomes:\n");
    for line in xfail_lines.iter().take(MAX_LISTED) {
        result.push_str(&format!("  {}\n", truncate(line, 120)));
    }
    if xfail_lines.len() > MAX_LISTED {
        result.push_str(&format!("  … +{} more\n", xfail_lines.len() - MAX_LISTED));
    }
}

/// Render one failure entry (test name plus a few relevant error lines).
fn render_one_failure(result: &mut String, index: usize, failure: &str) {
    let lines: Vec<&str> = failure.lines().collect();
    let Some(first_line) = lines.first() else {
        return;
    };
    if first_line.starts_with("___") {
        let test_name = first_line.trim_matches('_').trim();
        result.push_str(&format!("{}. [FAIL] {}\n", index + 1, test_name));
    } else if first_line.starts_with("FAILED") {
        let parts: Vec<&str> = first_line.split(" - ").collect();
        if let Some(test_path) = parts.first() {
            let test_name = test_path.trim_start_matches("FAILED ");
            result.push_str(&format!("{}. [FAIL] {}\n", index + 1, test_name));
        }
        if parts.len() > 1 {
            result.push_str(&format!("     {}\n", truncate(parts[1], 100)));
        }
        return;
    }
    render_relevant_lines(result, &lines);
}

fn render_relevant_lines(result: &mut String, lines: &[&str]) {
    let mut shown = 0;
    for line in &lines[1..] {
        let lower = line.to_lowercase();
        let relevant = line.trim().starts_with('>')
            || line.trim().starts_with('E')
            || lower.contains("assert")
            || lower.contains("error")
            || line.contains(".py:");
        if relevant && shown < 3 {
            result.push_str(&format!("     {}\n", truncate(line, 100)));
            shown += 1;
        }
    }
}

fn render_failures(result: &mut String, failures: &[String]) {
    if failures.is_empty() {
        return;
    }
    result.push_str("\nFailures:\n");
    for (i, failure) in failures.iter().take(MAX_LISTED).enumerate() {
        render_one_failure(result, i, failure);
        if i < failures.len() - 1 {
            result.push('\n');
        }
    }
    if failures.len() > MAX_LISTED {
        result.push_str(&format!(
            "\n… +{} more failures\n",
            failures.len() - MAX_LISTED
        ));
    }
}

fn build_summary(acc: &Accum) -> String {
    let counts = parse_summary_line(&acc.summary_line);
    let Some(header) = counts_header(&counts, !acc.xfail_lines.is_empty()) else {
        return PYTEST_NO_TESTS.to_string();
    };
    let mut result = header;
    result.push('\n');
    render_xfail(&mut result, &acc.xfail_lines);
    render_failures(&mut result, &acc.failures);
    result.trim().to_string()
}

/// Compress pytest output to its failures and summary line.
pub fn filter(output: &str) -> String {
    let mut state = ParseState::Header;
    let mut acc = Accum::default();
    for line in output.lines() {
        let trimmed = line.trim();
        if try_transition(trimmed, &mut state, &mut acc) {
            continue;
        }
        process_by_state(trimmed, &mut state, &mut acc);
    }
    acc.flush_current();
    build_summary(&acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_pass_compact() {
        let output = "\
=== test session starts ===
collected 5 items
tests/test_a.py .....  [100%]
=== 5 passed in 0.50s ===
";
        let result = filter(output);
        assert_eq!(result, "Pytest: 5 passed");
    }

    #[test]
    fn failures_surfaced() {
        let output = "\
=== test session starts ===
collected 3 items
=== FAILURES ===
___ test_login ___
tests/test_auth.py:12: assert False
=== short test summary info ===
FAILED tests/test_auth.py::test_login - AssertionError
=== 2 passed, 1 failed in 0.30s ===
";
        let result = filter(output);
        assert!(result.contains("Pytest: 2 passed, 1 failed"));
        assert!(result.contains("test_login"));
    }

    #[test]
    fn quiet_mode_summary() {
        let output = "5 failed, 1698 passed, 2 skipped in 108.89s\n";
        let result = filter(output);
        assert!(result.contains("1698 passed"));
        assert!(result.contains("5 failed"));
    }

    #[test]
    fn no_tests_collected() {
        let output =
            "=== test session starts ===\ncollected 0 items\n=== no tests ran in 0.01s ===\n";
        let result = filter(output);
        assert_eq!(result, PYTEST_NO_TESTS);
    }

    #[test]
    fn xpassed_surfaced() {
        let output = "\
=== test session starts ===
collected 4 items
=== short test summary info ===
XPASS tests/test_x.py::test_should_fail
=== 3 passed, 1 xpassed in 0.20s ===
";
        let result = filter(output);
        assert!(result.contains("xpassed"));
        assert!(result.contains("Expected-failure outcomes"));
    }

    #[test]
    fn many_failures_collapse() {
        let mut output = String::from("=== short test summary info ===\n");
        for i in 0..15 {
            output.push_str(&format!(
                "FAILED tests/test_{}.py::test_it - AssertionError\n",
                i
            ));
        }
        output.push_str("=== 0 passed, 15 failed in 1.00s ===\n");
        let result = filter(&output);
        assert!(result.contains("15 failed"));
        assert!(result.contains("… +5 more failures"));
    }
}
