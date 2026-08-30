//! Rust-native command-output filters.
//!
//! Some tools (tsc, pytest, mypy, prettier) produce output a declarative TOML
//! pipeline cannot compress well: the summary needs stateful parsing (group by
//! file, count by error code, collect failure blocks). These filters are the
//! pure `filter(&str) -> String` cores ported from RTK, dispatched by command.

mod mypy;
mod prettier;
mod pytest;
mod tsc;

/// A command that has a Rust-native filter.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Native {
    Tsc,
    Pytest,
    Mypy,
    Prettier,
}

/// Resolve the effective program a command runs, seeing past env assignments
/// (`FOO=bar cmd`), package-runner wrappers (`npx`/`pnpm`/`yarn`/`bunx`), and the
/// Python `-m module` form. Only the first pipeline segment is inspected.
fn resolve_program(command: &str) -> Option<String> {
    let first_seg = command.split(['|', ';', '&']).next().unwrap_or(command);
    let mut toks = first_seg.split_whitespace();
    let mut prog = toks.next()?;
    loop {
        if prog.contains('=') {
            // Leading `FOO=bar` env assignment.
            prog = toks.next()?;
            continue;
        }
        match prog {
            "npx" | "bunx" => {
                prog = toks.next()?;
            }
            "pnpm" | "yarn" | "bun" => {
                let next = toks.next()?;
                prog = if next == "exec" || next == "run" {
                    toks.next()?
                } else {
                    next
                };
            }
            "python" | "python3" | "py" => {
                // `python -m pytest` → the module is the effective program.
                return match toks.next() {
                    Some("-m") => toks.next().map(str::to_string),
                    _ => Some(prog.to_string()),
                };
            }
            _ => return Some(prog.to_string()),
        }
    }
}

fn classify(program: &str) -> Option<Native> {
    match program {
        "tsc" => Some(Native::Tsc),
        "pytest" | "py.test" => Some(Native::Pytest),
        "mypy" => Some(Native::Mypy),
        "prettier" => Some(Native::Prettier),
        _ => None,
    }
}

fn detect(command: &str) -> Option<Native> {
    classify(&resolve_program(command)?)
}

/// Filter a command's output with its Rust-native filter, or `None` when no
/// native filter claims the command.
pub fn try_filter(command: &str, raw: &str) -> Option<String> {
    Some(match detect(command)? {
        Native::Tsc => tsc::filter(raw),
        Native::Pytest => pytest::filter(raw),
        Native::Mypy => mypy::filter(raw),
        Native::Prettier => prettier::filter(raw),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bare_commands() {
        assert_eq!(detect("tsc --noEmit"), Some(Native::Tsc));
        assert_eq!(detect("pytest tests/"), Some(Native::Pytest));
        assert_eq!(detect("mypy src/"), Some(Native::Mypy));
        assert_eq!(detect("prettier --check ."), Some(Native::Prettier));
    }

    #[test]
    fn detects_through_runners() {
        assert_eq!(detect("npx tsc"), Some(Native::Tsc));
        assert_eq!(
            detect("pnpm exec prettier --check ."),
            Some(Native::Prettier)
        );
        assert_eq!(detect("yarn run tsc --noEmit"), Some(Native::Tsc));
        assert_eq!(detect("bunx prettier ."), Some(Native::Prettier));
    }

    #[test]
    fn detects_python_module_form() {
        assert_eq!(detect("python -m pytest -q"), Some(Native::Pytest));
        assert_eq!(detect("python3 -m mypy src"), Some(Native::Mypy));
        assert_eq!(detect("py.test tests/"), Some(Native::Pytest));
    }

    #[test]
    fn skips_env_assignments() {
        assert_eq!(detect("CI=1 pytest tests/"), Some(Native::Pytest));
    }

    #[test]
    fn only_first_segment() {
        // A later pipeline stage is not the program that produced the output.
        assert_eq!(detect("cat log | grep tsc"), None);
        assert_eq!(detect("echo pytest"), None);
    }

    #[test]
    fn unknown_command_is_none() {
        assert_eq!(detect("cargo build"), None);
        assert_eq!(detect("ls -la"), None);
        assert!(try_filter("cargo build", "whatever").is_none());
    }

    #[test]
    fn try_filter_runs_the_right_one() {
        let out =
            try_filter("tsc", "Found 0 errors. Watching for file changes.").expect("tsc claimed");
        assert!(out.contains("No errors found"));
    }
}
