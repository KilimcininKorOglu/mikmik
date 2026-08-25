//! `jq`, running in this process.
//!
//! `jaq` is a library, so the filter is compiled and run here and the results
//! are written straight into whatever the shell redirected the command to. No
//! child process, and no `jq` binary needs to be installed.
//!
//! What is supported is what `jaq` supports: the filter language, the standard
//! library and the JSON built-ins. The command-line flags are the ones a model
//! actually reaches for.

use std::io::{Read, Write};

use brush_core::builtins::{BoxFuture, ContentOptions, ContentType, Registration};
use brush_core::commands::{CommandArg, ExecutionContext};
use brush_core::extensions::ShellExtensions;

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::Val;

/// What the caller asked for.
struct Options {
    filter: String,
    /// `-r`: a string result is printed as its text rather than quoted.
    raw_output: bool,
    /// `-c`: one line per result.
    compact: bool,
    /// `-n`: run the filter against `null` instead of reading input.
    null_input: bool,
}

/// Read the arguments, refusing anything that is not understood.
///
/// A flag that is silently ignored produces output nobody asked for, and the
/// model has no way to tell that from a filter that answered differently.
fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        filter: String::new(),
        raw_output: false,
        compact: false,
        null_input: false,
    };
    let mut filter_seen = false;
    for arg in args {
        match arg.as_str() {
            "-r" | "--raw-output" => options.raw_output = true,
            "-c" | "--compact-output" => options.compact = true,
            "-n" | "--null-input" => options.null_input = true,
            // Bundled flags, the way every command-line tool takes them.
            other if other.starts_with('-') && other.len() > 1 && !other.starts_with("--") => {
                for flag in other.chars().skip(1) {
                    match flag {
                        'r' => options.raw_output = true,
                        'c' => options.compact = true,
                        'n' => options.null_input = true,
                        _ => return Err(format!("unknown option: -{flag}")),
                    }
                }
            }
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            other if !filter_seen => {
                options.filter = other.to_string();
                filter_seen = true;
            }
            other => return Err(format!("reading a file is not supported: {other}")),
        }
    }
    if !filter_seen {
        return Err("no filter given".to_string());
    }
    Ok(options)
}

/// Run `filter` over `input`, answering the rendered results.
fn evaluate(options: &Options, input: &str) -> Result<Vec<String>, String> {
    // All three definition sets: `jaq_core` for the language, `jaq_std` for
    // the standard library (`length`, `map`, `select`) and `jaq_json` for the
    // JSON built-ins. Leaving one out makes a filter that uses it fail to
    // compile, which reads as a syntax error nobody made.
    let loader = Loader::new(
        jaq_core::defs()
            .chain(jaq_std::defs())
            .chain(jaq_json::defs()),
    );
    let arena = Arena::default();
    let modules = loader
        .load(
            &arena,
            File {
                code: options.filter.as_str(),
                path: (),
            },
        )
        .map_err(|_| format!("could not read the filter: {}", options.filter))?;

    let filter = Compiler::default()
        .with_funs(
            jaq_core::funs()
                .chain(jaq_std::funs())
                .chain(jaq_json::funs()),
        )
        .compile(modules)
        .map_err(|_| format!("could not compile the filter: {}", options.filter))?;

    let inputs: Vec<Val> = if options.null_input {
        vec![Val::Null]
    } else {
        read_values(input)?
    };

    let mut rendered = Vec::new();
    for value in inputs {
        let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
        for result in filter.id.run((ctx, value)).map(unwrap_valr) {
            let value = result.map_err(|error| format!("{error}"))?;
            rendered.push(render(&value, options));
        }
    }
    Ok(rendered)
}

/// Every JSON value in `input`, which may hold more than one.
fn read_values(input: &str) -> Result<Vec<Val>, String> {
    jaq_json::read::parse_many(input.as_bytes())
        .map(|value| value.map_err(|error| format!("could not read the input: {error}")))
        .collect()
}

fn render(value: &Val, options: &Options) -> String {
    // `-r` only changes a string result. Anything else is JSON either way,
    // which is what jq does.
    if options.raw_output {
        if let Val::TStr(text) | Val::BStr(text) = value {
            return String::from_utf8_lossy(text).into_owned();
        }
    }
    let mut pp = jaq_json::write::Pp::default();
    if !options.compact {
        pp.indent = Some("  ".to_string());
    }
    let mut out = Vec::new();
    match jaq_json::write::write(&mut out, &pp, 0, value) {
        Ok(()) => String::from_utf8_lossy(&out).into_owned(),
        // Writing into a `Vec` cannot fail for want of space; answering the
        // error text beats dropping a result the filter produced.
        Err(error) => format!("<could not render: {error}>"),
    }
}

pub(super) fn registration<SE: ShellExtensions>() -> Registration<SE> {
    Registration {
        execute_func: execute::<SE>,
        content_func: content,
        disabled: false,
        special_builtin: false,
        declaration_builtin: false,
    }
}

fn execute<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<brush_core::ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        let owned: Vec<String> = args
            .iter()
            .skip(1)
            .map(|arg| match arg {
                CommandArg::String(value) => value.clone(),
                other => other.to_string(),
            })
            .collect();

        let options = match parse(&owned) {
            Ok(options) => options,
            Err(message) => {
                let _ = writeln!(context.stderr(), "jq: {message}");
                return Ok(brush_core::ExecutionResult::new(2));
            }
        };

        let mut input = String::new();
        if !options.null_input {
            let mut source = context.stdin();
            if let Err(error) = source.read_to_string(&mut input) {
                let _ = writeln!(context.stderr(), "jq: could not read the input: {error}");
                return Ok(brush_core::ExecutionResult::new(2));
            }
        }

        match evaluate(&options, &input) {
            Ok(lines) => {
                let mut out = context.stdout();
                for line in lines {
                    let _ = writeln!(out, "{line}");
                }
                let _ = out.flush();
                Ok(brush_core::ExecutionResult::new(0))
            }
            Err(message) => {
                let _ = writeln!(context.stderr(), "jq: {message}");
                Ok(brush_core::ExecutionResult::new(5))
            }
        }
    })
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "the signature is `brush_core::builtins::CommandContentFunc`"
)]
fn content(
    name: &str,
    content_type: ContentType,
    _options: &ContentOptions,
) -> Result<String, brush_core::Error> {
    match content_type {
        ContentType::ShortDescription => Ok(format!("{name} - filter JSON")),
        ContentType::DetailedHelp => Ok(format!(
            "{name} - filter JSON. Flags: -r raw output, -c compact, -n null input.\n"
        )),
        ContentType::ShortUsage | ContentType::ManPage => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(filter: &str, input: &str) -> Vec<String> {
        let options = parse(&[filter.to_string(), "-c".to_string()]).expect("options");
        evaluate(&options, input).expect("evaluated")
    }

    #[test]
    fn a_field_is_read_out_of_an_object() {
        assert_eq!(run(".a", r#"{"a": 1, "b": 2}"#), vec!["1"]);
    }

    #[test]
    fn a_filter_can_answer_more_than_once() {
        assert_eq!(run(".[]", "[1,2,3]"), vec!["1", "2", "3"]);
    }

    #[test]
    fn the_standard_library_is_available() {
        // Without `jaq_std` in the loader, `length` would not resolve and the
        // filter would fail to compile.
        assert_eq!(run("length", "[1,2,3]"), vec!["3"]);
        assert_eq!(run("map(. * 2)", "[1,2]"), vec!["[2,4]"]);
    }

    #[test]
    fn raw_output_unquotes_a_string_and_leaves_the_rest_alone() {
        let raw = parse(&[".a".to_string(), "-r".to_string(), "-c".to_string()]).expect("options");
        assert_eq!(
            evaluate(&raw, r#"{"a": "text"}"#).expect("evaluated"),
            vec!["text"]
        );
        assert_eq!(
            evaluate(&raw, r#"{"a": 12}"#).expect("evaluated"),
            vec!["12"]
        );

        let quoted = parse(&[".a".to_string(), "-c".to_string()]).expect("options");
        assert_eq!(
            evaluate(&quoted, r#"{"a": "text"}"#).expect("evaluated"),
            vec!["\"text\""]
        );
    }

    #[test]
    fn null_input_runs_without_reading_anything() {
        let options =
            parse(&["1 + 1".to_string(), "-n".to_string(), "-c".to_string()]).expect("options");
        assert_eq!(evaluate(&options, "").expect("evaluated"), vec!["2"]);
    }

    #[test]
    fn more_than_one_value_in_the_input_is_read() {
        assert_eq!(run(".", "1 2 3"), vec!["1", "2", "3"]);
    }

    #[test]
    fn a_flag_nobody_implemented_is_refused_rather_than_ignored() {
        // A silently ignored flag produces output nobody asked for, and the
        // caller cannot tell that from a filter that answered differently.
        // Each case carries a filter, so the refusal can only come from the
        // flag and not from the missing-filter check below it.
        assert!(parse(&["--slurp".to_string(), ".".to_string()]).is_err());
        assert!(parse(&[".".to_string(), "-z".to_string()]).is_err());
        assert!(parse(&[".".to_string(), "--sort-keys".to_string()]).is_err());
        // A second positional is a file to read, which is not supported.
        assert!(parse(&[".".to_string(), "data.json".to_string()]).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn bundled_short_flags_are_read() {
        let options = parse(&[".".to_string(), "-rc".to_string()]).expect("options");
        assert!(options.raw_output);
        assert!(options.compact);
    }

    #[test]
    fn a_filter_that_does_not_compile_is_reported() {
        // Two separate stages answer an error, and both have to reach the
        // caller: the loader refuses text that is not a filter, and the
        // compiler refuses a filter that names something nobody defined.
        let unreadable = parse(&["...(".to_string()]).expect("options");
        assert!(evaluate(&unreadable, "{}").is_err());

        let uncompilable = parse(&["no_such_function_xyz".to_string()]).expect("options");
        assert!(evaluate(&uncompilable, "{}").is_err());
    }
}
