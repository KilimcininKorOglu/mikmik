//! What the carried utilities answer, against what GNU answers.
//!
//! `bundledUtilities` defaults to `prefer`, so the copy in this binary stands
//! in for the machine's own `ls` and `sort`. The carried set is
//! [uutils](https://github.com/uutils/coreutils), which aims at GNU
//! compatibility rather than claiming it, so the difference is worth measuring
//! rather than assuming.
//!
//! Each case is one call a model actually writes, run twice on the same input:
//! once through the carried copy, once through the machine's GNU binary. A
//! case whose binary is not on this machine skips itself, so the test is
//! useful where GNU is installed and silent where it is not.
//!
//! GNU coreutils is `g`-prefixed on macOS, where the unprefixed names are
//! BSD's. A comparison against BSD would measure the wrong thing, so the
//! prefixed name is tried first and the plain one only where it is GNU.

use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use mikmik_shell::streams::{with_streams, Streams};

/// One call, and what it needs.
struct Case {
    /// The utility's name, as a shell would write it.
    utility: &'static str,
    /// Its arguments, `{dir}` standing for the input directory.
    args: &'static [&'static str],
    /// What to feed its standard input.
    stdin: &'static str,
    /// Paths under the input directory to create before each run, so a
    /// utility that consumes its argument gets the same input twice. A
    /// trailing `/` asks for a directory.
    prepare: &'static [&'static str],
}

/// The calls a model writes most, on input that does not change between runs.
const CASES: &[Case] = &[
    Case {
        utility: "ls",
        args: &["-1", "{dir}"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "ls",
        args: &["-la", "{dir}"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "sort",
        args: &["{dir}/numbers"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "sort",
        args: &["-n", "{dir}/numbers"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "sort",
        args: &["-u", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "sort",
        args: &["-r", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "head",
        args: &["-n", "2", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "head",
        args: &["-c", "5", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "tail",
        args: &["-n", "2", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "wc",
        args: &["-l", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "wc",
        args: &["-w", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "wc",
        args: &["-c", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "cut",
        args: &["-d,", "-f2", "{dir}/csv"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "cut",
        args: &["-c", "1-3", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "tr",
        args: &["a-z", "A-Z"],
        stdin: "hello there\n",
        prepare: &[],
    },
    Case {
        utility: "tr",
        args: &["-d", "aeiou"],
        stdin: "hello there\n",
        prepare: &[],
    },
    Case {
        utility: "uniq",
        args: &["-c", "{dir}/repeats"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "seq",
        args: &["1", "5"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "seq",
        args: &["-w", "1", "10"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "basename",
        args: &["/a/b/c.txt", ".txt"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "dirname",
        args: &["/a/b/c.txt"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "realpath",
        args: &["{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "du",
        args: &["-s", "{dir}"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "cat",
        args: &["{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "grep",
        args: &["-c", "", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "date",
        args: &["-u", "-d", "@0", "+%Y-%m-%d"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "printf",
        args: &["%s-%s\\n", "a", "b"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "sed",
        args: &["s/a/A/g", "{dir}/words"],
        stdin: "",
        prepare: &[],
    },
    // Below here: the calls whose output goes through `print!` and friends
    // rather than through a stream handle. Those macros reach the process's
    // real standard output, so a utility that uses one bypasses the redirect
    // unless the patch covers it. Each of these caught that when it was
    // missing.
    Case {
        utility: "head",
        args: &["-n", "1", "{dir}/words", "{dir}/numbers"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "tsort",
        args: &["{dir}/pairs"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "cp",
        args: &["-v", "{dir}/words", "{dir}/spare"],
        stdin: "",
        prepare: &[],
    },
    Case {
        utility: "mv",
        args: &["-v", "{dir}/spare", "{dir}/moved"],
        stdin: "",
        prepare: &["spare"],
    },
    Case {
        utility: "rm",
        args: &["-v", "{dir}/spare"],
        stdin: "",
        prepare: &["spare"],
    },
    Case {
        utility: "rmdir",
        args: &["-v", "{dir}/hollow"],
        stdin: "",
        prepare: &["hollow/"],
    },
];

/// What one run produced.
#[derive(PartialEq, Eq)]
struct Answer {
    output: String,
    errors: String,
    code: i32,
}

fn scratch(dir: &Path, name: &str) -> Arc<File> {
    Arc::new(
        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(dir.join(name))
            .expect("open"),
    )
}

/// Run the carried copy of `case`.
fn carried(case: &Case, input: &Path, scratch_dir: &Path) -> Option<Answer> {
    let run = *mikmik_shell::bundled::registry().get(case.utility)?;

    std::fs::write(scratch_dir.join("stdin"), case.stdin).expect("write");
    let streams = Streams {
        stdin: Arc::new(File::open(scratch_dir.join("stdin")).expect("open")),
        stdout: scratch(scratch_dir, "out"),
        stderr: scratch(scratch_dir, "err"),
    };

    let mut argv = vec![OsString::from(case.utility)];
    argv.extend(
        case.args
            .iter()
            .map(|arg| OsString::from(arg.replace("{dir}", &input.display().to_string()))),
    );

    let code = with_streams(case.utility, streams, || run(argv));
    Some(Answer {
        output: std::fs::read_to_string(scratch_dir.join("out")).expect("read"),
        errors: std::fs::read_to_string(scratch_dir.join("err")).expect("read"),
        code,
    })
}

/// Run the machine's GNU copy of `case`, if it has one.
fn gnu(case: &Case, input: &Path) -> Option<Answer> {
    let binary = gnu_binary(case.utility)?;
    let args: Vec<String> = case
        .args
        .iter()
        .map(|arg| arg.replace("{dir}", &input.display().to_string()))
        .collect();

    let mut command = std::process::Command::new(&binary);
    command
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // A utility announces itself out of `argv[0]`, which is the path the
    // command was found at. The carried copy is told its plain name, so
    // without this every message would differ by the `/opt/homebrew/bin/g`
    // in front of it, which is the harness rather than the utility.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.arg0(case.utility);
    }

    let mut child = command.spawn().ok()?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take()?;
        stdin.write_all(case.stdin.as_bytes()).ok()?;
    }
    let finished = child.wait_with_output().ok()?;

    Some(Answer {
        output: String::from_utf8_lossy(&finished.stdout).into_owned(),
        errors: String::from_utf8_lossy(&finished.stderr).into_owned(),
        code: finished.status.code().unwrap_or(-1),
    })
}

/// The GNU build of `utility` on this machine.
///
/// `g`-prefixed first: on macOS the plain names are BSD's, and comparing
/// against BSD would measure a difference this fork is not about.
fn gnu_binary(utility: &str) -> Option<std::path::PathBuf> {
    if let Ok(prefixed) = which::which(format!("g{utility}")) {
        return Some(prefixed);
    }
    if cfg!(target_os = "linux") {
        return which::which(utility).ok();
    }
    None
}

/// A directory of inputs that do not change between runs.
fn inputs(dir: &Path) {
    std::fs::write(dir.join("words"), "banana\napple\ncherry\napple\n").expect("write");
    std::fs::write(dir.join("numbers"), "10\n9\n100\n1\n").expect("write");
    std::fs::write(dir.join("csv"), "one,two,three\nfour,five,six\n").expect("write");
    std::fs::write(dir.join("repeats"), "a\na\nb\nc\nc\nc\n").expect("write");
    std::fs::write(dir.join("pairs"), "a b\nb c\nc d\n").expect("write");
}

/// Put back what the case needs, so the second run sees what the first did.
///
/// A utility that consumes its argument would otherwise be compared against
/// itself failing.
fn prepare(case: &Case, dir: &Path) {
    for wanted in case.prepare {
        let path = dir.join(wanted.trim_end_matches('/'));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);
        if wanted.ends_with('/') {
            std::fs::create_dir(&path).expect("mkdir");
        } else {
            std::fs::write(&path, "one\ntwo\n").expect("write");
        }
    }
}

#[test]
fn the_carried_utilities_answer_what_gnu_answers() {
    let held = tempfile::tempdir().expect("tempdir");
    let input = held.path().join("input");
    let scratch_dir = held.path().join("scratch");
    std::fs::create_dir(&input).expect("mkdir");
    std::fs::create_dir(&scratch_dir).expect("mkdir");
    inputs(&input);

    let mut compared = 0;
    let mut differences = Vec::new();

    for case in CASES {
        prepare(case, &input);
        let Some(theirs) = gnu(case, &input) else {
            continue;
        };
        prepare(case, &input);
        let Some(ours) = carried(case, &input, &scratch_dir) else {
            continue;
        };
        compared += 1;

        if ours != theirs {
            differences.push(format!(
                "{} {}\n  carried: code {} out {:?} err {:?}\n  GNU:     code {} out {:?} err {:?}",
                case.utility,
                case.args.join(" "),
                ours.code,
                ours.output,
                ours.errors,
                theirs.code,
                theirs.output,
                theirs.errors,
            ));
        }
    }

    if compared == 0 {
        eprintln!("no GNU coreutils on this machine; nothing compared");
        return;
    }

    assert!(
        differences.is_empty(),
        "{} of {compared} calls answered differently:\n\n{}",
        differences.len(),
        differences.join("\n\n")
    );
}
