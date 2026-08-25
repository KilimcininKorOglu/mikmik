//! Run commands through a real `ShellSession` and print what they produced.
//!
//! Two things are hard to see from a unit test, and this shows both.
//!
//! A whole pipeline: the bundled utilities run in this process, so
//! `ls | sort` streams between threads rather than processes, and that is only
//! visible from a real shell. An empty environment proves the machine needs
//! nothing installed; `MIKMIK_BUNDLED=fallback` reaches for the machine's
//! own copies instead.
//!
//! Persistence: every argument is one command, and all of them run in the same
//! session, so what one leaves behind the next one sees.
//!
//! ```text
//! cargo build --example run_commands -p mikmik-shell
//! env -i ./target/debug/examples/run_commands 'ls -1 | sort | head -2'
//! ./target/debug/examples/run_commands 'cd /tmp' 'pwd'
//! ```

use std::io::Read;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let commands: Vec<String> = std::env::args().skip(1).collect();
    let commands = if commands.is_empty() {
        vec!["pwd".to_string()]
    } else {
        commands
    };

    let cwd = std::env::current_dir()?;
    let opened = Instant::now();
    let bundled = match std::env::var("MIKMIK_BUNDLED").as_deref() {
        Ok("fallback") => mikmik_shell::BundledUtilities::Fallback,
        _ => mikmik_shell::BundledUtilities::Prefer,
    };
    let mut session = mikmik_shell::ShellSession::new(&cwd, bundled).await?;
    println!("--- session opened in {:?} ---", opened.elapsed());

    for command in &commands {
        let started = Instant::now();
        let (mut reader, writer) = std::io::pipe()?;
        let errors = writer.try_clone()?;
        let outcome = session
            .run(command, writer, errors, Duration::from_secs(30))
            .await?;
        let elapsed = started.elapsed();

        let mut text = String::new();
        reader.read_to_string(&mut text)?;
        print!("{text}");
        println!(
            "--- `{command}` exit {} in {elapsed:?} ---",
            outcome.exit_code
        );
    }

    Ok(())
}
