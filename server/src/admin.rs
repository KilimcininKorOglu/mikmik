//! The `admin` subcommand.
//!
//! Enough to open the first account. Everything after that belongs in the web
//! interface, which needs an account to log in with and so cannot open the
//! first one itself.

use std::io::{IsTerminal, Read};

use crate::accounts;
use crate::store::Store;

const USAGE: &str = "\
Usage: mikmik-server admin <command>

  create <email> [--admin]   Open an account. The password is read from stdin.
  list                       List the accounts.

The password is never taken from the command line, because the shell records
its history and `ps` shows its arguments to every user on the machine.

  echo 'the password' | mikmik-server admin create ayse@firma.com --admin
";

pub fn run(store: &Store, args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("create") => create(store, &args[1..]),
        Some("list") => list(store),
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

fn create(store: &Store, args: &[String]) -> anyhow::Result<()> {
    let is_admin = args.iter().any(|arg| arg == "--admin");
    let Some(email) = args.iter().find(|arg| !arg.starts_with("--")) else {
        anyhow::bail!("`admin create` needs an email address");
    };

    let password = read_password()?;
    let id = accounts::create_user(store, email, &password, is_admin)?;

    let role = if is_admin { "administrator" } else { "user" };
    println!("opened {role} {} ({id})", accounts::normalise_email(email));
    Ok(())
}

/// Read the password from stdin.
///
/// A terminal echoes what is typed, which is why the note says so rather than
/// pretending otherwise. Piping is the way to avoid it.
fn read_password() -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        eprintln!("Enter the password. It will be visible as you type; pipe it in to avoid that.");
    }
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    let password = buffer.trim_end_matches(['\n', '\r']).to_string();
    if password.is_empty() {
        anyhow::bail!("no password was given on stdin");
    }
    Ok(password)
}

fn list(store: &Store) -> anyhow::Result<()> {
    let rows: Vec<(String, i64, Option<i64>)> = store.with(|conn| {
        let mut statement =
            conn.prepare("SELECT email, is_admin, disabled_at FROM users ORDER BY created_at")?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;

    if rows.is_empty() {
        println!("no accounts yet");
        return Ok(());
    }
    for (email, is_admin, disabled_at) in rows {
        let role = if is_admin != 0 { "admin" } else { "user " };
        let state = if disabled_at.is_some() {
            " (disabled)"
        } else {
            ""
        };
        println!("{role}  {email}{state}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_without_an_address_says_so() {
        let store = Store::open_in_memory().expect("store");
        let error = create(&store, &["--admin".to_string()]).expect_err("refused");
        assert!(error.to_string().contains("email address"), "{error}");
    }

    #[test]
    fn the_usage_text_refuses_to_show_a_password_argument() {
        // The one thing this text must never do is teach an operator to put a
        // password where `ps` and the shell history can read it.
        assert!(USAGE.contains("read from stdin"));
        assert!(!USAGE.contains("create <email> <password>"));
    }

    #[test]
    fn an_unknown_command_prints_the_usage_rather_than_failing() {
        let store = Store::open_in_memory().expect("store");
        assert!(run(&store, &["nonsense".to_string()]).is_ok());
        assert!(run(&store, &[]).is_ok());
    }
}
