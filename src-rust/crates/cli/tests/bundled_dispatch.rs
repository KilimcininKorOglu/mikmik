//! End-to-end: run the `mikmik` binary as a bundled utility.
//!
//! The shell registers a shim for every utility the machine does not have, and
//! that shim re-executes this binary as `mikmik --invoke-bundled <name>`. The
//! dispatch therefore has to work before anything else in `main` runs: no
//! config is read, no session is opened, no argument parser sees the argv.
//!
//! Runs against the debug binary `cargo build` produces; Cargo supplies its
//! path through `CARGO_BIN_EXE_mikmik`.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn binary_path() -> String {
    env!("CARGO_BIN_EXE_mikmik").to_string()
}

/// Run the binary with `args`, feeding `stdin` to it.
fn run(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(binary_path())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mikmik");

    {
        let mut handle = child.stdin.take().expect("stdin");
        handle.write_all(stdin.as_bytes()).expect("write stdin");
    }

    child.wait_with_output().expect("wait for mikmik")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_bundled_utility_runs_and_reports_success() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("lines.txt");
    std::fs::write(&file, "satir-bir\nsatir-iki\n").expect("write");

    let output = run(
        &["--invoke-bundled", "cat", &file.display().to_string()],
        "",
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout_of(&output), "satir-bir\nsatir-iki\n");
}

#[test]
fn a_bundled_utility_reads_the_standard_input_it_was_given() {
    // The shim puts the utility in the middle of a pipeline, so it has to read
    // whatever the shell connected to it rather than a file it opens itself.
    let output = run(&["--invoke-bundled", "sort"], "b\na\nc\n");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout_of(&output), "a\nb\nc\n");
}

#[test]
fn a_name_nobody_bundled_answers_the_code_a_shell_expects() {
    let output = run(&["--invoke-bundled", "a-command-nobody-bundled"], "");

    // 127 is what a shell answers for a command it cannot find, and the
    // caller here is a shell.
    assert_eq!(output.status.code(), Some(127));
    assert!(output.stdout.is_empty());
}

#[test]
fn a_dispatch_without_a_name_is_a_usage_error() {
    let output = run(&["--invoke-bundled"], "");

    assert_eq!(output.status.code(), Some(2));
    // The message has to come from the dispatch rather than from the argument
    // parser further down, which answers 2 for its own reasons.
    let complaint = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(complaint.contains("needs a command name"), "{complaint}");
}

#[test]
fn a_machine_with_nothing_installed_still_runs_the_utility() {
    // This is the whole point of bundling. With an empty `PATH` there is no
    // `cat`, `sort` or `wc` anywhere, and the utility still runs because it is
    // inside the binary.
    let mut child = Command::new(binary_path())
        .args(["--invoke-bundled", "wc", "-l"])
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mikmik");

    {
        let mut handle = child.stdin.take().expect("stdin");
        handle.write_all(b"a\nb\nc\n").expect("write stdin");
    }

    let output = child.wait_with_output().expect("wait for mikmik");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout_of(&output).trim(), "3");
}

#[test]
fn the_flag_is_read_in_the_first_position_only() {
    // A command line that mentions the token later is an ordinary run. The
    // check here is that the binary does not treat it as a dispatch and exit
    // with 127; `--version` prints and exits 0 as it always did.
    let output = run(&["--version", "--invoke-bundled"], "");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout_of(&output).contains(env!("CARGO_PKG_VERSION")),
        "{}",
        stdout_of(&output)
    );
}
