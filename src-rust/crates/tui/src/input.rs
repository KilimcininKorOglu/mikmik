// input.rs — Slash command helpers and input mode types.

/// Check whether a string looks like a slash command (e.g. "/help").
pub fn is_slash_command(input: &str) -> bool {
    input.starts_with('/') && !input.starts_with("//")
}

/// Check whether a string looks like a shell command to run here (e.g. "!ls").
///
/// `!!` escapes it for the same reason `//` escapes a slash command: a line
/// that starts with a bang and is meant for the model has to have some way to
/// say so.
pub fn is_bang_command(input: &str) -> bool {
    input.starts_with('!') && !input.starts_with("!!")
}

/// The command in a bang line, without the bang and without surrounding space.
///
/// Returns `""` for anything that is not a bang line, and for a bare `!`.
pub fn parse_bang_command(input: &str) -> &str {
    if !is_bang_command(input) {
        return "";
    }
    input[1..].trim()
}

/// Parse a slash command into `(command_name, args)`.
/// Returns `("", "")` if the input is not a slash command.
pub fn parse_slash_command(input: &str) -> (&str, &str) {
    if !is_slash_command(input) {
        return ("", "");
    }
    let without_slash = &input[1..];
    if let Some(space_idx) = without_slash.find(' ') {
        (
            &without_slash[..space_idx],
            without_slash[space_idx + 1..].trim(),
        )
    } else {
        (without_slash, "")
    }
}

/// Trim `raw` and push it onto `segments` unless it is empty.
fn push_chain_segment(segments: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

/// Split a slash-command line into `&&`-separated segments.
///
/// Splits on a top-level `&&`, so `/a && /b` runs as two commands. A `&&`
/// inside single or double quotes is left alone, so a command argument may
/// contain it. Segments are trimmed and empties dropped. A line with no `&&`
/// comes back as a single element, so the caller can treat one command and a
/// chain the same way.
///
/// Only meaningful for slash input; a bang line's `&&` is shell syntax, so the
/// caller guards with [`is_slash_command`] before splitting.
pub fn split_command_chain(input: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            '&' if !in_single && !in_double && chars.peek() == Some(&'&') => {
                chars.next(); // consume the second '&'
                push_chain_segment(&mut segments, &current);
                current.clear();
            }
            _ => current.push(c),
        }
    }
    push_chain_segment(&mut segments, &current);
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_detection() {
        assert!(is_slash_command("/help"));
        assert!(is_slash_command("/compact args"));
        assert!(!is_slash_command("//comment"));
        assert!(!is_slash_command("hello"));
        assert!(!is_slash_command(""));
    }

    #[test]
    fn bang_command_detection() {
        assert!(is_bang_command("!ls"));
        assert!(is_bang_command("!ls -la"));
        // A bare bang is a bang line; the run path is what refuses an empty
        // command, so detection must not swallow it here.
        assert!(is_bang_command("!"));
        assert!(!is_bang_command("!!literal"));
        assert!(!is_bang_command("/help"));
        assert!(!is_bang_command("hello!"));
        assert!(!is_bang_command(""));
    }

    #[test]
    fn parse_bang_takes_the_command_and_leaves_the_bang() {
        assert_eq!(parse_bang_command("!ls -la"), "ls -la");
        assert_eq!(parse_bang_command("!  pwd  "), "pwd");
        assert_eq!(parse_bang_command("!"), "");
        assert_eq!(parse_bang_command("!!literal"), "");
        assert_eq!(parse_bang_command("hello"), "");
    }

    #[test]
    fn parse_no_args() {
        let (cmd, args) = parse_slash_command("/help");
        assert_eq!(cmd, "help");
        assert_eq!(args, "");
    }

    #[test]
    fn parse_with_args() {
        let (cmd, args) = parse_slash_command("/compact  --force ");
        assert_eq!(cmd, "compact");
        assert_eq!(args, "--force");
    }

    #[test]
    fn parse_non_slash() {
        let (cmd, args) = parse_slash_command("hello world");
        assert_eq!(cmd, "");
        assert_eq!(args, "");
    }

    #[test]
    fn a_chain_splits_on_top_level_double_ampersand() {
        assert_eq!(split_command_chain("/a && /b"), vec!["/a", "/b"]);
        assert_eq!(
            split_command_chain("/a && /b && /c"),
            vec!["/a", "/b", "/c"]
        );
    }

    #[test]
    fn a_lone_command_comes_back_as_one_segment() {
        assert_eq!(split_command_chain("/help"), vec!["/help"]);
        assert_eq!(
            split_command_chain("/compact --force"),
            vec!["/compact --force"]
        );
    }

    #[test]
    fn segments_are_trimmed_and_empties_dropped() {
        assert_eq!(split_command_chain("  /a  &&  /b  "), vec!["/a", "/b"]);
        // A trailing `&&` leaves nothing after it, so no empty segment appears.
        assert_eq!(split_command_chain("/a && "), vec!["/a"]);
        assert!(split_command_chain("   ").is_empty());
    }

    #[test]
    fn a_double_ampersand_inside_quotes_is_left_alone() {
        // The `&&` belongs to the argument, so the line is one command.
        assert_eq!(
            split_command_chain(r#"/echo "x && y""#),
            vec![r#"/echo "x && y""#]
        );
        assert_eq!(
            split_command_chain("/echo 'a && b'"),
            vec!["/echo 'a && b'"]
        );
        // A real separator after a quoted argument still splits.
        assert_eq!(
            split_command_chain(r#"/echo "x && y" && /b"#),
            vec![r#"/echo "x && y""#, "/b"]
        );
    }

    #[test]
    fn a_single_ampersand_does_not_split() {
        assert_eq!(split_command_chain("/run a & b"), vec!["/run a & b"]);
    }
}
