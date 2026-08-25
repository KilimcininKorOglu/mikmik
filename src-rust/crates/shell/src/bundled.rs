//! Utilities that ship inside the MikMik binary.
//!
//! A model writes `ls`, `cat`, `sort`, `head` and `wc` without asking whether
//! the machine has them. On Windows it usually does not, and on a stripped
//! container image neither does Linux. These are compiled in: around eighty
//! coreutils from `uutils`, plus `find`, `xargs`, `sed` and `jq`.
//!
//! ## What "in the binary" does and does not mean
//!
//! It means the machine needs nothing installed. It does not mean no process
//! is started. A `uutils` utility writes to the process's real standard
//! output and ends by calling `std::process::exit`, so it cannot be called in
//! the middle of a pipeline without taking the whole process with it. The way
//! round that, which upstream brush uses and this follows, is to run the
//! bundled utility as a child of the shell: the binary re-executes itself as
//! `mikmik --invoke-bundled <name> <args>`, and redirection, pipes and
//! process-group state then work because the child is an ordinary process.
//!
//! `find` and `jq` are the exceptions. Both are libraries that write through a
//! writer the caller supplies, so they run in this process with no child at
//! all.
//!
//! ## The real binary wins
//!
//! A shim is registered only for a name the machine does not already have on
//! `PATH`. On a Unix box with GNU coreutils installed, nothing here is ever
//! reached and behaviour is exactly what it was. The bundled set is what
//! Windows and a bare container get.
//!
//! The dispatch protocol and the shim are adapted from
//! `brush-shell/src/bundled.rs` by Reuben Olinsky, MIT licensed.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use brush_core::builtins::{BoxFuture, ContentOptions, ContentType, Registration};
use brush_core::commands::{self, CommandArg, ExecutionContext};
use brush_core::extensions::ShellExtensions;
use brush_core::ExecutionExitCode;

mod find;
mod jq;

/// The leading argument that marks a bundled dispatch.
///
/// Recognised in the first position only, so a script that happens to contain
/// the literal token further along is unaffected.
pub const DISPATCH_FLAG: &str = "--invoke-bundled";

/// The shape of a bundled command's entry point, which is `uutils`' own.
pub type BundledFn = fn(args: Vec<OsString>) -> i32;

/// Every bundled command, by the name a shell would call it.
static REGISTRY: OnceLock<HashMap<String, BundledFn>> = OnceLock::new();

/// The path to this executable, worked out once.
static SELF_EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The bundled commands this build carries.
pub fn registry() -> &'static HashMap<String, BundledFn> {
    REGISTRY.get_or_init(build_registry)
}

fn build_registry() -> HashMap<String, BundledFn> {
    let mut commands = brush_coreutils_builtins::bundled_commands();
    // Not coreutils, and each shaped differently upstream, so each gets an
    // adapter to the `uumain` signature the registry is keyed on.
    commands.insert("xargs".to_string(), xargs as BundledFn);
    commands.insert("sed".to_string(), sed as BundledFn);
    commands
}

/// `findutils` takes `&[&str]` and answers an exit code.
fn xargs(args: Vec<OsString>) -> i32 {
    let owned: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
    findutils::xargs::xargs_main(&borrowed)
}

/// `sed` is already `uumain`-shaped; the wrapper is only for the name.
fn sed(args: Vec<OsString>) -> i32 {
    sed_crate::sed::uumain(args.into_iter())
}

/// Run the bundled command this process was invoked for, if it was.
///
/// Answers `Some(code)` when the process was started as
/// `mikmik --invoke-bundled <name> [args]`; the caller exits with it. Exiting
/// in the caller rather than here keeps destructors and tracing guards in the
/// loop.
///
/// Answers `None` when this is an ordinary run.
pub fn maybe_dispatch() -> Option<i32> {
    let rest = dispatch_target(std::env::args_os())?;
    Some(run_dispatch(&rest))
}

/// What a dispatch invocation asks for: the command name and its arguments.
///
/// Answers `None` for an ordinary run. The flag counts in the first position
/// after `argv[0]` only, so `echo --invoke-bundled ls` stays an `echo`.
fn dispatch_target(argv: impl IntoIterator<Item = OsString>) -> Option<Vec<OsString>> {
    let mut raw = argv.into_iter();
    let _argv0 = raw.next()?;
    if raw.next()? != DISPATCH_FLAG {
        return None;
    }
    Some(raw.collect())
}

/// Run the named bundled command, answering the exit code the caller uses.
fn run_dispatch(rest: &[OsString]) -> i32 {
    let Some((name, args)) = rest.split_first() else {
        eprintln!("mikmik: {DISPATCH_FLAG} needs a command name");
        return 2;
    };

    // The registry is keyed by `String`, so a name that is not UTF-8 can never
    // match. Refusing here beats building a lossy key that could collide with
    // a real registration.
    let Some(name) = name.to_str() else {
        return 127;
    };
    let Some(run) = registry().get(name) else {
        eprintln!("mikmik: unknown bundled command: {name}");
        return 127;
    };

    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(OsString::from(name));
    argv.extend(args.iter().cloned());
    run(argv)
}

/// Register a shim for every bundled command the machine does not already
/// have, plus the two that run in this process.
///
/// `register_builtin_if_unset` means brush's own built-ins win: `echo`,
/// `printf`, `test`, `true` and `false` keep the shell's semantics rather than
/// the coreutils ones.
pub(crate) fn register<SE: ShellExtensions>(shell: &mut brush_core::Shell<SE>) {
    if wanted("find") {
        shell.register_builtin_if_unset("find".to_string(), find::registration::<SE>());
    }
    if wanted("jq") {
        shell.register_builtin_if_unset("jq".to_string(), jq::registration::<SE>());
    }
    for name in registry().keys() {
        if wanted(name) {
            shell.register_builtin_if_unset(name.clone(), shim_registration::<SE>());
        }
    }
}

/// Whether the bundled copy of `name` is what this machine should use.
///
/// The machine's own binary wins whenever it has one. It is what the user's
/// scripts were written against, and on Unix it is almost always there, so
/// nothing bundled is reached and behaviour is exactly what it was. The
/// bundled set is what Windows and a stripped container get.
fn wanted(name: &str) -> bool {
    which::which(name).is_err()
}

/// What `help <name>` and `type <name>` say about a bundled command.
#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "the signature is `brush_core::builtins::CommandContentFunc`"
)]
fn shim_content(
    name: &str,
    content_type: ContentType,
    _options: &ContentOptions,
) -> Result<String, brush_core::Error> {
    match content_type {
        ContentType::ShortDescription => Ok(format!("{name} - bundled with mikmik")),
        ContentType::DetailedHelp => Ok(format!(
            "{name} - bundled with mikmik (runs as `mikmik {DISPATCH_FLAG} {name}`)\n"
        )),
        // The utility answers `--help` itself; repeating a summary here would
        // be a second thing to keep true.
        ContentType::ShortUsage | ContentType::ManPage => Ok(String::new()),
    }
}

/// Run a bundled command by re-executing this binary.
///
/// The command name is read from the context, so one registration serves every
/// bundled name. The child is spawned through brush's own external-command
/// path, which is what makes the caller's redirections and pipes apply to it
/// without any of them being reimplemented here.
fn shim_execute<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<brush_core::ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        let Some(exe) = self_exe() else {
            let _ = writeln!(
                context.stderr(),
                "mikmik: cannot find the path to the running executable"
            );
            return Ok(ExecutionExitCode::CannotExecute.into());
        };

        // `args[0]` is dropped by the external path, which takes argv[0] from
        // `argv0` below. The bundled name goes in a fixed slot after the flag
        // so the child's dispatcher does not have to guess.
        let name = context.command_name.clone();
        let mut child_args: Vec<CommandArg> = Vec::with_capacity(args.len() + 2);
        child_args.push(CommandArg::String(String::new()));
        child_args.push(CommandArg::String(DISPATCH_FLAG.to_string()));
        child_args.push(CommandArg::String(name.clone()));
        child_args.extend(args.into_iter().skip(1));

        let mut command = commands::SimpleCommand::new(
            commands::ShellForCommand::ParentShell(context.shell),
            context.params,
            exe.to_string_lossy().into_owned(),
            child_args,
        );
        // The exe path holds a separator, so lookup goes straight to the
        // external path and cannot re-enter this shim. Turning functions off
        // as well means a shell function named after the binary cannot either.
        command.use_functions = false;
        // Without this the child sees the mikmik path as argv[0] and every
        // error it prints is attributed to `mikmik` rather than to `ls`.
        command.argv0 = Some(name);

        let spawned = command.execute().await?;
        Ok(spawned.wait().await?.into())
    })
}

fn shim_registration<SE: ShellExtensions>() -> Registration<SE> {
    Registration {
        execute_func: shim_execute::<SE>,
        content_func: shim_content,
        disabled: false,
        special_builtin: false,
        declaration_builtin: false,
    }
}

fn self_exe() -> Option<&'static PathBuf> {
    SELF_EXE
        .get_or_init(|| std::env::current_exe().ok())
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_carries_what_a_model_reaches_for() {
        let registry = registry();
        for name in [
            "ls", "cat", "head", "tail", "sort", "uniq", "wc", "cut", "tr", "seq", "cp", "mv",
            "rm", "mkdir", "touch", "tee", "du", "df", "env", "basename", "dirname", "realpath",
            "date", "xargs", "sed",
        ] {
            assert!(registry.contains_key(name), "{name} is not bundled");
        }
    }

    #[test]
    fn a_name_nobody_bundled_is_not_in_the_registry() {
        // The registry decides what a shim is registered for. A name that is
        // in it by accident would shadow the machine's own binary on a
        // machine that has none.
        assert!(!registry().contains_key("cargo"));
        assert!(!registry().contains_key("git"));
    }

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_dispatch_flag_is_only_read_in_the_leading_position() {
        // A script that passes the literal token to another command must run
        // normally. Only the argument right after argv[0] turns this process
        // into a bundled command.
        assert_eq!(
            dispatch_target(argv(&["mikmik", DISPATCH_FLAG, "cat", "a.txt"])),
            Some(argv(&["cat", "a.txt"]))
        );
        assert_eq!(
            dispatch_target(argv(&["mikmik", "echo", DISPATCH_FLAG])),
            None
        );
        assert_eq!(dispatch_target(argv(&["mikmik", "--print", "hello"])), None);
        assert_eq!(dispatch_target(argv(&["mikmik"])), None);
    }

    #[test]
    fn a_dispatch_nobody_can_serve_reports_it_rather_than_running_something_else() {
        // 127 is what a shell answers for a command it cannot find, and the
        // shim's caller is a shell.
        assert_eq!(run_dispatch(&argv(&["a-command-nobody-bundled"])), 127);
        // No name at all is a usage error, not a missing command.
        assert_eq!(run_dispatch(&[]), 2);
    }

    #[test]
    fn the_running_binary_is_what_a_shim_re_executes() {
        // The shim spawns this path. Without it there is nothing to run, and
        // the shim answers `CannotExecute` instead.
        let exe = self_exe().expect("the running test binary has a path");
        assert!(exe.is_absolute(), "{}", exe.display());
    }

    #[test]
    fn a_machine_that_has_the_binary_keeps_it() {
        // Nothing bundled is reached on a machine with the real thing. This
        // is what keeps Unix behaviour untouched.
        let present = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(
            !wanted(present),
            "{present} is on PATH; the bundled copy must not win"
        );
        assert!(wanted("a-command-nobody-installed"));
    }
}
