//! Run commands through a real `ShellSession` and print what they produced.
//!
//! Two things are hard to see from a unit test, and this shows both.
//!
//! The shim: this binary dispatches `--invoke-bundled` exactly as `mikmik`
//! does, so a bundled utility the shell reaches for re-executes this example
//! rather than the machine's own binary. Run it with an empty environment to
//! watch the bundled copies do the work.
//!
//! Persistence: every argument is one command, and all of them run in the same
//! session, so what one leaves behind the next one sees.
//!
//! ```text
//! cargo build --example shim_check -p mikmik-shell
//! env -i ./target/debug/examples/shim_check 'ls -1 | sort | head -2'
//! ./target/debug/examples/shim_check 'cd /tmp' 'pwd'
//! ```

use std::io::Read;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Some(code) = mikmik_shell::maybe_dispatch() {
        std::process::exit(code);
    }

    let commands: Vec<String> = std::env::args().skip(1).collect();
    let commands = if commands.is_empty() {
        vec!["pwd".to_string()]
    } else {
        commands
    };

    let cwd = std::env::current_dir()?;
    let opened = Instant::now();
    let mut session = mikmik_shell::ShellSession::new(&cwd).await?;
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
