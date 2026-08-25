//! Utilities that ship inside the MikMik binary.
//!
//! A model writes `ls`, `cat`, `sort`, `head` and `wc` without asking whether
//! the machine has them. On Windows it usually does not, and on a stripped
//! container image neither does Linux. These are compiled in: 83 coreutils
//! from `uutils`, plus `find`, `xargs`, `sed` and `jq`.
//!
//! ## They run in this process
//!
//! Nothing is spawned. The published `uutils` crates could not manage that,
//! because each obtains its output with `std::io::stdout()`, the process's
//! real standard output, and a utility called that way would write over
//! whatever else the process is printing. The source is forked under
//! `vendor/coreutils/` and patched so the streams can be redirected per call;
//! see that directory's README.
//!
//! Each utility runs on its own thread with the shell's descriptors installed
//! for that thread. A `uumain` is synchronous throughout, so it cannot hop
//! threads mid-run, which is what makes a per-thread override the right scope.
//!
//! The thread also keeps a synchronous utility off a tokio worker, and leaves
//! the call cancellable so a timeout can stop waiting for it. It is not what
//! makes a pipeline stream: brush already gives every stage of a pipeline its
//! own blocking thread.
//!
//! ## Which copy wins
//!
//! [`crate::BundledUtilities`] decides. `Prefer` registers every bundled name
//! and is the default, because the bundled copy is in this process and the
//! machine's is a fork and an exec. `Fallback` registers only a name `which`
//! cannot find, which leaves a Unix box with GNU coreutils behaving exactly as
//! it did.
//!
//! brush's own built-ins win either way, so `echo`, `printf`, `test`, `true`
//! and `false` keep the shell's semantics rather than the coreutils ones.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, OnceLock};

use brush_core::builtins::{BoxFuture, ContentOptions, ContentType, Registration};
use brush_core::commands::{CommandArg, ExecutionContext};
use brush_core::extensions::ShellExtensions;

use crate::streams::{self, Streams};
use crate::BundledUtilities;

mod find;
mod jq;

/// The shape of a bundled command's entry point, which is `uutils`' own.
pub type BundledFn = fn(args: Vec<OsString>) -> i32;

/// Every bundled command, by the name a shell would call it.
static REGISTRY: OnceLock<HashMap<String, BundledFn>> = OnceLock::new();

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

/// Register the bundled utilities on `shell`.
///
/// `find` and `jq` are separate: both are libraries that write through a
/// writer the caller supplies, so they need no stream override at all.
pub(crate) fn register<SE: ShellExtensions>(
    shell: &mut brush_core::Shell<SE>,
    policy: BundledUtilities,
) {
    if wanted("find", policy) {
        shell.register_builtin_if_unset("find".to_string(), find::registration::<SE>());
    }
    if wanted("jq", policy) {
        shell.register_builtin_if_unset("jq".to_string(), jq::registration::<SE>());
    }
    for name in registry().keys() {
        if wanted(name, policy) {
            shell.register_builtin_if_unset(name.clone(), native_registration::<SE>());
        }
    }
}

/// Whether the bundled copy of `name` is what this shell should use.
fn wanted(name: &str, policy: BundledUtilities) -> bool {
    match policy {
        BundledUtilities::Prefer => true,
        BundledUtilities::Fallback => which::which(name).is_err(),
    }
}

/// What `help <name>` and `type <name>` say about a bundled command.
#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "the signature is `brush_core::builtins::CommandContentFunc`"
)]
fn native_content(
    name: &str,
    content_type: ContentType,
    _options: &ContentOptions,
) -> Result<String, brush_core::Error> {
    match content_type {
        ContentType::ShortDescription => Ok(format!("{name} - bundled with mikmik")),
        ContentType::DetailedHelp => Ok(format!("{name} - bundled with mikmik\n")),
        // The utility answers `--help` itself; repeating a summary here would
        // be a second thing to keep true.
        ContentType::ShortUsage | ContentType::ManPage => Ok(String::new()),
    }
}

/// Run a bundled utility in this process.
fn native_execute<SE: ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<brush_core::ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        let name = context.command_name.clone();
        let Some(run) = registry().get(name.as_str()).copied() else {
            let _ = writeln!(context.stderr(), "mikmik: {name}: not bundled");
            return Ok(brush_core::ExecutionResult::new(127));
        };

        let streams = match descriptors(&context) {
            Ok(streams) => streams,
            Err(error) => {
                let _ = writeln!(context.stderr(), "{name}: {error}");
                return Ok(brush_core::ExecutionResult::new(125));
            }
        };

        // The utility resolves `sort list` and a bare `ls` against the
        // process's working directory, and brush keeps the shell's separately.
        // The borrow makes the two agree for the length of the call.
        let directory = context.shell.working_dir().to_path_buf();
        let borrowed = match crate::cwd::borrow(&directory) {
            Ok(borrowed) => borrowed,
            Err(error) => {
                let _ = writeln!(context.stderr(), "{name}: {error}");
                return Ok(brush_core::ExecutionResult::new(125));
            }
        };

        // `args[0]` is the command name the shell resolved; the utility wants
        // it as its own argv[0], which is what it prints its errors under.
        let mut argv: Vec<OsString> = Vec::with_capacity(args.len());
        argv.push(OsString::from(name.clone()));
        argv.extend(args.into_iter().skip(1).map(|arg| match arg {
            CommandArg::String(value) => OsString::from(value),
            other => OsString::from(other.to_string()),
        }));

        // Its own thread, for three reasons. The stream override is per
        // thread. A `uumain` is synchronous, so running it here would hold a
        // tokio worker for as long as it takes. And awaiting the thread rather
        // than joining it leaves this future cancellable, which is what a
        // timeout needs.
        let announced = name.clone();
        let code = match tokio::task::spawn_blocking(move || {
            // The borrow is released on this thread, once the utility has
            // finished with the directory.
            let _borrowed = borrowed;
            streams::with_streams(&announced, streams, || run(argv))
        })
        .await
        {
            Ok(code) => code,
            // A panicking utility is a bug in the fork rather than something
            // the script did. 70 is what a shell answers for an internal
            // software error.
            Err(error) => {
                let _ = writeln!(context.stderr(), "{name}: stopped unexpectedly: {error}");
                70
            }
        };

        Ok(brush_core::ExecutionResult::new(
            u8::try_from(code).unwrap_or(1),
        ))
    })
}

/// The three descriptors the shell wants this command to use.
///
/// Each is duplicated rather than borrowed, because the utility keeps them for
/// its whole run on another thread while the shell keeps its own copies.
fn descriptors<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
) -> Result<Streams, brush_core::Error> {
    Ok(Streams {
        stdin: duplicate(context, brush_core::openfiles::OpenFiles::STDIN_FD)?,
        stdout: duplicate(context, brush_core::openfiles::OpenFiles::STDOUT_FD)?,
        stderr: duplicate(context, brush_core::openfiles::OpenFiles::STDERR_FD)?,
    })
}

/// A private copy of the file behind one of the shell's descriptors.
///
/// A descriptor the shell did not set answers the null device, which is what
/// the shell itself gives a command in that position.
fn duplicate<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    fd: brush_core::ShellFd,
) -> Result<Arc<File>, brush_core::Error> {
    let open = match context.params.try_fd(context.shell, fd) {
        Some(open) => open,
        None => brush_core::openfiles::null()?,
    };
    let owned = open.try_borrow_as_fd()?.try_clone_to_owned()?;
    Ok(Arc::new(File::from(owned)))
}

fn native_registration<SE: ShellExtensions>() -> Registration<SE> {
    Registration {
        execute_func: native_execute::<SE>,
        content_func: native_content,
        disabled: false,
        special_builtin: false,
        declaration_builtin: false,
    }
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
        // The registry decides what a builtin is registered for. A name that
        // is in it by accident would shadow the machine's own binary.
        assert!(!registry().contains_key("cargo"));
        assert!(!registry().contains_key("git"));
    }

    #[test]
    fn prefer_takes_the_bundled_copy_even_when_the_machine_has_one() {
        // The bundled copy runs in this process and the machine's costs a fork
        // and an exec, so preferring it is the faster answer as well as the
        // one that behaves the same everywhere.
        let present = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(wanted(present, BundledUtilities::Prefer));
        assert!(wanted(
            "a-command-nobody-installed",
            BundledUtilities::Prefer
        ));
    }

    #[test]
    fn fallback_leaves_the_machines_own_binary_alone() {
        let present = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(!wanted(present, BundledUtilities::Fallback));
        assert!(wanted(
            "a-command-nobody-installed",
            BundledUtilities::Fallback
        ));
    }
}
