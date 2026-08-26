//! A PowerShell that outlives one command.
//!
//! Every PowerShell tool call used to spawn `pwsh -Command <script>`, so a
//! variable, a `cd` or an imported module was gone by the next call and every
//! call paid PowerShell's startup. One interpreter now stays open per session
//! and reads its commands from stdin.
//!
//! # Knowing where a command ended
//!
//! `pwsh -Command -` never says so itself: it writes the command's output and
//! then waits for the next line. So each command is followed by a line the
//! session prints on **both** streams:
//!
//! ```text
//! <sentinel>:<0 or 1>:<$LASTEXITCODE>      on stdout
//! <sentinel>                               on stderr
//! ```
//!
//! Both, because stderr carries no result of its own and a reader that only
//! watched stdout would have to guess how long to wait for the error text.
//!
//! The sentinel is 16 random bytes, made fresh for each session. A fixed one
//! would be a word any command could print, and a command that printed it
//! would end the read early and take the rest of its own output with it.
//!
//! # Reading the answer
//!
//! `$?` is captured into `$__mikmik_ok` before anything else runs, because it
//! reports the last command and the sentinel line is itself a command.
//! `$LASTEXITCODE` is cleared before each command, because PowerShell leaves
//! the previous native command's code in it and a later cmdlet failure would
//! otherwise be reported under a number it never produced.
//!
//! PowerShell writes `ESC [ ? 1 h` and `ESC [ ? 1 l` around each result, even
//! into a pipe, so both streams are stripped of escape sequences before
//! anything is matched or answered.
//!
//! # One runtime
//!
//! The interpreter is a `tokio::process::Child`, which belongs to the runtime
//! that started it: reaching it from another one answers that the first is
//! shutting down. MikMik runs one runtime for the whole process, so a session
//! outlives every command in it. A test that builds a runtime of its own needs
//! a session id of its own.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// What one command left behind.
pub(crate) struct Ran {
    /// What it wrote to stdout.
    pub output: String,
    /// What it wrote to stderr.
    pub errors: String,
    /// 0 when the command succeeded. Otherwise the code a native program
    /// answered, or 1 when the failure was a cmdlet's and carried no code.
    pub exit_code: i32,
}

/// An interpreter that stays open.
pub(crate) struct PowerShellSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
    sentinel: String,
}

impl PowerShellSession {
    /// Start an interpreter rooted at `working_dir`.
    ///
    /// The profile is not read, for the reason the embedded shell does not
    /// read `.bashrc`: it names a file the user controls and would run before
    /// every command the model writes.
    pub(crate) fn open(working_dir: &Path) -> anyhow::Result<Self> {
        let mut builder = Command::new(interpreter());
        builder
            .args(["-NoProfile", "-NonInteractive", "-Command", "-"])
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a timeout can take the whole tree without
        // reaching this process.
        mikmik_core::process_tree::spawn_in_own_group(&mut builder);
        let mut child = builder
            .spawn()
            .map_err(|error| anyhow::anyhow!("could not start PowerShell: {error}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("PowerShell gave no standard input"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("PowerShell gave no standard output"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("PowerShell gave no standard error"))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr: BufReader::new(stderr),
            sentinel: sentinel(),
        })
    }

    /// Run `command` and answer what it did.
    ///
    /// An error here means the session is no longer usable: the interpreter is
    /// killed and the caller opens a new one.
    pub(crate) async fn run(&mut self, command: &str, timeout: Duration) -> anyhow::Result<Ran> {
        match tokio::time::timeout(timeout, self.exchange(command)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                // The tree first, then the interpreter: killing the
                // interpreter first orphans what the script started and the
                // tree can no longer be found through it.
                if let Some(pid) = self.child.id() {
                    mikmik_core::process_tree::terminate_tree(pid);
                }
                // It is mid-command and will never see the sentinel, so it
                // cannot be reused.
                let _ = self.child.kill().await;
                Err(anyhow::anyhow!(
                    "PowerShell command timed out after {}ms",
                    timeout.as_millis()
                ))
            }
        }
    }

    /// Write the command and its two sentinels, then read both streams back.
    async fn exchange(&mut self, command: &str) -> anyhow::Result<Ran> {
        let sentinel = self.sentinel.clone();
        let script = format!(
            "$global:LASTEXITCODE = $null\n\
             {command}\n\
             $__mikmik_ok = $?\n\
             [Console]::Error.WriteLine(\"{sentinel}\")\n\
             Write-Output \"{sentinel}:$(if ($__mikmik_ok) {{0}} else {{1}}):$LASTEXITCODE\"\n"
        );
        self.stdin.write_all(script.as_bytes()).await?;
        self.stdin.flush().await?;

        let (output, marker) = read_until(&mut self.stdout, &sentinel).await?;
        let (errors, _) = read_until(&mut self.stderr, &sentinel).await?;

        Ok(Ran {
            output,
            errors,
            exit_code: exit_code(&marker),
        })
    }
}

/// Read lines until the one that starts with `sentinel`.
///
/// Answers everything before it, and that line.
async fn read_until<R>(
    reader: &mut BufReader<R>,
    sentinel: &str,
) -> anyhow::Result<(String, String)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut collected = String::new();
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Err(anyhow::anyhow!(
                "PowerShell ended in the middle of a command"
            ));
        }

        let clean = strip_escapes(line.trim_end_matches(['\r', '\n']));
        if let Some(rest) = clean.strip_prefix(sentinel) {
            return Ok((collected, rest.to_string()));
        }
        collected.push_str(&clean);
        collected.push('\n');
    }
}

/// Read `:<ok>:<code>` from what followed the sentinel.
///
/// `ok` is 0 when the command succeeded; `code` is what a native program
/// answered, and empty when the last command was a cmdlet.
fn exit_code(marker: &str) -> i32 {
    let mut parts = marker.trim_start_matches(':').split(':');
    let succeeded = parts.next().is_some_and(|ok| ok.trim() == "0");
    if succeeded {
        return 0;
    }
    parts
        .next()
        .and_then(|code| code.trim().parse::<i32>().ok())
        .filter(|code| *code != 0)
        .unwrap_or(1)
}

/// Remove the escape sequences PowerShell writes around every result.
///
/// It writes them into a pipe as well as a terminal, so they are in the way of
/// matching the sentinel and would reach the model as noise.
pub(crate) fn strip_escapes(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut characters = text.chars();

    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            clean.push(character);
            continue;
        }
        // CSI: parameters and intermediates, then one final byte.
        if characters.next() != Some('[') {
            continue;
        }
        for inner in characters.by_ref() {
            if inner.is_ascii_alphabetic() || inner == '@' {
                break;
            }
        }
    }

    clean
}

/// A word no command can print, because nothing has seen it before.
fn sentinel() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Only a machine with no random source gets here, and a session that
        // refused to open would be a worse answer than a fixed word.
        return "mikmik-powershell-sentinel".to_string();
    }
    let mut word = String::with_capacity(2 + bytes.len() * 2);
    word.push_str("mk");
    for byte in bytes {
        word.push_str(&format!("{byte:02x}"));
    }
    word
}

/// The interpreter to start.
fn interpreter() -> &'static str {
    // `pwsh` is PowerShell 7 and is what a Unix box has if it has anything.
    // Windows may have only the 5.1 that ships with it.
    if which::which("pwsh").is_ok() {
        "pwsh"
    } else {
        "powershell"
    }
}

/// Whether this machine has a PowerShell at all.
pub(crate) fn available() -> bool {
    which::which("pwsh").is_ok() || (cfg!(windows) && which::which("powershell").is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_are_removed_and_the_text_is_not() {
        assert_eq!(strip_escapes("\u{1b}[?1h\u{1b}[?1lhello"), "hello");
        assert_eq!(strip_escapes("plain"), "plain");
        assert_eq!(strip_escapes("\u{1b}[32mgreen\u{1b}[0m"), "green");
    }

    #[test]
    fn the_marker_says_what_the_command_answered() {
        assert_eq!(exit_code(":0:"), 0);
        // A cmdlet that failed carries no code of its own.
        assert_eq!(exit_code(":1:"), 1);
        // A native program's code is the one to report.
        assert_eq!(exit_code(":1:7"), 7);
        // Success wins even when a stale code is still in the variable.
        assert_eq!(exit_code(":0:7"), 0);
        // Nothing usable still has to be a failure rather than a success.
        assert_eq!(exit_code(""), 1);
    }

    #[test]
    fn every_session_gets_a_word_of_its_own() {
        // A fixed sentinel is a word a command could print, and printing it
        // would end the read early and cut the rest of the output off.
        assert_ne!(sentinel(), sentinel());
        assert_eq!(sentinel().len(), 34);
    }

    /// These need a PowerShell on the machine, and skip themselves without one.
    fn skip_without_powershell() -> bool {
        if available() {
            return false;
        }
        eprintln!("no PowerShell on this machine; skipping");
        true
    }

    #[tokio::test]
    async fn a_variable_outlives_the_command_that_set_it() {
        if skip_without_powershell() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = PowerShellSession::open(dir.path()).expect("open");

        let set = session
            .run("$x = 42", Duration::from_secs(30))
            .await
            .expect("set");
        assert_eq!(set.exit_code, 0);

        let read = session
            .run("$x", Duration::from_secs(30))
            .await
            .expect("read");
        assert_eq!(read.output.trim(), "42");
        assert_eq!(read.exit_code, 0);
    }

    #[tokio::test]
    async fn a_directory_change_outlives_the_command_that_made_it() {
        if skip_without_powershell() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("inner")).expect("mkdir");
        let mut session = PowerShellSession::open(dir.path()).expect("open");

        session
            .run("cd inner", Duration::from_secs(30))
            .await
            .expect("cd");
        let here = session
            .run("(Get-Location).Path", Duration::from_secs(30))
            .await
            .expect("pwd");

        assert!(here.output.trim().ends_with("inner"), "{:?}", here.output);
    }

    #[tokio::test]
    async fn a_command_that_fails_says_so() {
        if skip_without_powershell() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = PowerShellSession::open(dir.path()).expect("open");

        let failed = session
            .run("Write-Error 'on purpose'", Duration::from_secs(30))
            .await
            .expect("run");
        assert_ne!(failed.exit_code, 0);
        assert!(failed.errors.contains("on purpose"), "{:?}", failed.errors);

        // And the session is still usable afterwards.
        let after = session
            .run("Write-Output 'still here'", Duration::from_secs(30))
            .await
            .expect("run");
        assert_eq!(after.exit_code, 0);
        assert_eq!(after.output.trim(), "still here");
    }

    #[tokio::test]
    async fn a_native_programs_own_code_comes_back() {
        if skip_without_powershell() || cfg!(windows) {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = PowerShellSession::open(dir.path()).expect("open");

        let seven = session
            .run("& /bin/sh -c 'exit 7'", Duration::from_secs(30))
            .await
            .expect("run");
        assert_eq!(seven.exit_code, 7);

        // And it does not stay behind for the next command, which is what
        // clearing `$LASTEXITCODE` is for.
        let after = session
            .run("Write-Error 'a cmdlet failure'", Duration::from_secs(30))
            .await
            .expect("run");
        assert_eq!(after.exit_code, 1);
    }

    #[tokio::test]
    async fn a_command_that_prints_a_sentinel_of_its_own_is_read_through() {
        if skip_without_powershell() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = PowerShellSession::open(dir.path()).expect("open");

        // The shape of a real sentinel, with a word this session is not using.
        let run = session
            .run(
                "Write-Output 'mk00000000000000000000000000000000:0:'; Write-Output 'after'",
                Duration::from_secs(30),
            )
            .await
            .expect("run");

        assert_eq!(run.exit_code, 0);
        assert!(run.output.contains("after"), "{:?}", run.output);
    }

    #[tokio::test]
    async fn escape_sequences_do_not_reach_the_answer() {
        if skip_without_powershell() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = PowerShellSession::open(dir.path()).expect("open");

        let run = session
            .run("Write-Output 'clean'", Duration::from_secs(30))
            .await
            .expect("run");

        assert!(!run.output.contains('\u{1b}'), "{:?}", run.output);
        assert_eq!(run.output.trim(), "clean");
    }

    #[tokio::test]
    async fn a_command_that_runs_too_long_ends_the_session() {
        if skip_without_powershell() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = PowerShellSession::open(dir.path()).expect("open");

        let timed_out = session
            .run("Start-Sleep -Seconds 30", Duration::from_millis(500))
            .await;
        assert!(timed_out.is_err());

        // The interpreter is mid-command, so it is killed rather than reused.
        let after = session
            .run("Write-Output 'x'", Duration::from_secs(5))
            .await;
        assert!(after.is_err(), "a killed session must not answer");
    }
}
