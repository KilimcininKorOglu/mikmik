//! `mikmik workspace <login|logout|status|sync|restore>`.
//!
//! Connects this installation to an organisation's configuration server. The
//! address is written to the user's global `settings.json`; the session token
//! goes to `auth.json`, which is written `0o600`.

use std::io::{IsTerminal, Read};

use mikmik_core::config::{ProjectRunnables, Settings, WorkspaceSettings};
use mikmik_core::workspace_server::{
    policy, providers, search_providers, session, sync, BackupWrite, WorkspaceClient,
    WorkspaceError,
};
use mikmik_core::AuthStore;

pub const USAGE: &str = "\
Usage:
  mikmik workspace login <url> --email <address>   Sign in; the password is read from stdin
  mikmik workspace logout                          End the session, drop the company providers
  mikmik workspace status                          Server, account, providers, policy, sync
  mikmik workspace sync                            Upload this machine's settings now
  mikmik workspace restore                         Bring the stored settings back

The address must be https, unless it is localhost.
";

/// Run one `workspace` subcommand.
pub async fn run(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("login") => login(&args[1..]).await,
        Some("logout") => logout().await,
        Some("status") => status().await,
        Some("sync") => upload().await,
        Some("restore") => restore().await,
        Some(other) => {
            eprintln!("mikmik: unknown workspace subcommand `{other}`\n\n{USAGE}");
            std::process::exit(2);
        }
        None => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

/// The value of a `--name value` flag.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

/// Read the password from stdin.
///
/// Never from an argument: the shell records its history, and `ps` shows every
/// argument to every user on the machine. A terminal echoes what is typed,
/// which is why the note says so rather than pretending otherwise.
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

/// The server this installation is signed in to, with a live session.
///
/// Every subcommand but `login` needs both, and the two failures need
/// different words: one is "you never joined", the other is "log in again".
fn connected(settings: &Settings) -> anyhow::Result<(WorkspaceSettings, WorkspaceClient)> {
    let Some(workspace) = settings.workspace.clone() else {
        anyhow::bail!("no workspace server is configured; run `mikmik workspace login <url>`");
    };
    let Some(token) = AuthStore::load()
        .workspace_session(workspace.base())
        .map(str::to_string)
    else {
        anyhow::bail!(
            "there is no live session for {}; run `mikmik workspace login {} --email <address>`",
            workspace.base(),
            workspace.base()
        );
    };
    let client = WorkspaceClient::new(&workspace)?.with_token(token);
    Ok((workspace, client))
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

async fn login(args: &[String]) -> anyhow::Result<()> {
    let Some(url) = args.first().filter(|arg| !arg.starts_with('-')) else {
        eprintln!("mikmik: workspace login needs the server address\n\n{USAGE}");
        std::process::exit(2);
    };
    let Some(email) = flag(args, "--email") else {
        eprintln!("mikmik: workspace login needs --email\n\n{USAGE}");
        std::process::exit(2);
    };

    // Keep whatever sync triggers the user already chose, so signing in again
    // against the same server does not quietly turn them back on.
    let mut settings = Settings::load_sync().unwrap_or_default();
    let existing_sync = settings
        .workspace
        .as_ref()
        .filter(|workspace| workspace.base() == url.trim().trim_end_matches('/'))
        .map(|workspace| workspace.sync.clone())
        .unwrap_or_default();
    let workspace = WorkspaceSettings {
        url: url.to_string(),
        sync: existing_sync,
    };

    let client = WorkspaceClient::new(&workspace)?;
    let password = read_password()?;
    let session = client.login(email, &password).await?;

    // The token is written before the address, so a login that succeeded is
    // never followed by settings naming a server this machine cannot reach.
    AuthStore::load().set_workspace_session(client.base(), &session.token, session.expires_in);
    settings.workspace = Some(workspace.clone());
    settings.save_sync()?;

    println!("Signed in to {} as {}.", client.base(), session.user.email);
    if session.user.is_admin {
        println!("This account administers the server.");
    }

    let client = WorkspaceClient::new(&workspace)?.with_token(&session.token);
    pull(&client).await?;

    // The backup is not applied here. It can carry a hook, and the password
    // has just eaten stdin, so there is nothing left to answer a prompt with.
    match client.backup().await {
        Ok(Some(stored)) => println!(
            "A settings backup is on the server (version {}). \
             Run `mikmik workspace restore` to bring it back.",
            stored.version
        ),
        Ok(None) => {
            println!("No settings backup is stored yet. `mikmik workspace sync` makes one.")
        }
        Err(error) => eprintln!("mikmik: the backup could not be read ({error})."),
    }
    Ok(())
}

/// Take the organisation's providers and its policy.
///
/// Neither touches anything the user wrote, so this runs without asking.
async fn pull(client: &WorkspaceClient) -> anyhow::Result<()> {
    // Through `session` rather than by hand, so what a login writes and what a
    // running session writes cannot drift into being two different things.
    let applied = match session::pull_providers(client).await {
        Ok(applied) => applied,
        Err(error) => {
            // The accounts already on the machine keep working. Saying so
            // beats a bare failure that reads as "your providers are gone".
            eprintln!(
                "mikmik: the company providers could not be read ({error}); \
                 whatever is already configured is untouched."
            );
            return Ok(());
        }
    };

    if !applied.written.is_empty() {
        println!("Company providers: {}", applied.written.join(", "));
    }
    if !applied.withdrawn.is_empty() {
        println!("No longer assigned: {}", applied.withdrawn.join(", "));
    }
    for name in &applied.refused {
        println!(
            "The company offers a provider named `{name}`, and this machine already has one \
             under that name. Yours was kept. Rename it to take the company's."
        );
    }

    match policy::refresh(client).await {
        Ok(cached) => match cached.settings.as_ref() {
            Some(policy) => {
                let keys = policy::decided_keys(policy);
                if !keys.is_empty() {
                    println!("The company policy decides: {}", keys.join(", "));
                }
            }
            None => println!("The company sets no policy."),
        },
        Err(error) => eprintln!("mikmik: the policy could not be read ({error})."),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

async fn logout() -> anyhow::Result<()> {
    let mut settings = Settings::load_sync().unwrap_or_default();
    let Some(workspace) = settings.workspace.clone() else {
        println!("No workspace server is configured.");
        return Ok(());
    };

    // Tell the server first, then forget the token. The other order leaves a
    // session open on the server that nothing can end any more.
    if let Some(token) = AuthStore::load().workspace_session(workspace.base()) {
        match WorkspaceClient::new(&workspace) {
            Ok(client) => {
                if let Err(error) = client.with_token(token).logout().await {
                    eprintln!(
                        "mikmik: the server was not told ({error}); the token is being \
                         forgotten here anyway."
                    );
                }
            }
            Err(error) => eprintln!("mikmik: {error}"),
        }
    }

    let mut auth = AuthStore::load();
    auth.clear_workspace_session();
    let mut gone = providers::forget(&mut settings, &mut auth, workspace.base());
    gone.extend(search_providers::forget(&mut auth, workspace.base()));
    gone.sort();
    settings.save_sync()?;
    auth.save();
    policy::clear_cache();

    println!("Signed out of {}.", workspace.base());
    if !gone.is_empty() {
        println!("Removed the company providers: {}", gone.join(", "));
    }
    println!("Your own providers and settings are untouched.");
    println!("The address stays in settings.json; `workspace login` uses it again.");
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn status() -> anyhow::Result<()> {
    let settings = Settings::load_sync().unwrap_or_default();
    let Some(workspace) = settings.workspace.clone() else {
        println!("No workspace server is configured.");
        print!("{USAGE}");
        return Ok(());
    };

    println!("Server:  {}", workspace.base());
    println!(
        "Sync:    on change {}, at startup {}, timer {}",
        on_off(workspace.sync.on_change),
        on_off(workspace.sync.pull_at_startup),
        match workspace.sync.interval_minutes {
            Some(minutes) => format!("every {minutes} minutes"),
            None => "off".to_string(),
        }
    );

    let managed = providers::managed_by(&settings, workspace.base());
    println!(
        "Company: {}",
        if managed.is_empty() {
            "no providers assigned".to_string()
        } else {
            managed.join(", ")
        }
    );

    let search = search_providers::managed(&AuthStore::load(), workspace.base());
    println!(
        "Search:  {}",
        if search.is_empty() {
            "no search providers assigned".to_string()
        } else {
            search.join(", ")
        }
    );

    match policy::load_cached(workspace.base()).settings {
        Some(policy) => println!("Policy:  {}", policy::decided_keys(&policy).join(", ")),
        None => println!("Policy:  none"),
    }

    let Some(token) = AuthStore::load()
        .workspace_session(workspace.base())
        .map(str::to_string)
    else {
        println!(
            "Session: none. Run `mikmik workspace login {} --email <address>`.",
            workspace.base()
        );
        return Ok(());
    };

    let client = WorkspaceClient::new(&workspace)?.with_token(token);
    match client.me().await {
        Ok(identity) => {
            println!("Account: {}", identity.email);
            if identity.is_admin {
                println!("Role:    administrator");
            }
            let groups: Vec<&str> = identity
                .groups
                .iter()
                .map(|group| group.name.as_str())
                .collect();
            println!(
                "Groups:  {}",
                if groups.is_empty() {
                    "none".to_string()
                } else {
                    groups.join(", ")
                }
            );
        }
        // A stored token the server no longer accepts has to say so plainly:
        // the difference decides whether the user logs in again or waits for
        // the network.
        Err(WorkspaceError::Unauthorized) => {
            println!("Session: rejected by the server. Log in again.");
        }
        Err(error) => println!("Session: held, but the server did not answer ({error})."),
    }
    Ok(())
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

// ---------------------------------------------------------------------------
// sync and restore
// ---------------------------------------------------------------------------

async fn upload() -> anyhow::Result<()> {
    let settings = Settings::load_sync().unwrap_or_default();
    let (_, client) = connected(&settings)?;

    // The same call the background triggers make, so `workspace sync` and an
    // automatic upload cannot exclude different fields.
    match session::upload(&client).await? {
        BackupWrite::Stored { version, .. } => {
            println!("Uploaded. The stored backup is now version {version}.");
            Ok(())
        }
        // Never swallowed: the two machines hold different settings, and only
        // the person using them can say which is right.
        BackupWrite::Conflict { current_version } => anyhow::bail!(
            "another machine wrote version {current_version} while this one was reading. \
             Nothing was uploaded. Run `mikmik workspace restore` to see what it stored, \
             or `mikmik workspace sync` again to overwrite it with this machine's settings."
        ),
    }
}

async fn restore() -> anyhow::Result<()> {
    let mut settings = Settings::load_sync().unwrap_or_default();
    let (workspace, client) = connected(&settings)?;

    let Some(stored) = client.backup().await? else {
        println!("No settings backup is stored for this account.");
        return Ok(());
    };

    let restore = sync::read(&stored.settings)?;
    for key in &restore.refused {
        println!("The backup names `{key}`, which belongs to the machine that wrote it. Skipped.");
    }

    let gate = sync::gate(&restore, workspace.base());
    let runnables = if gate.is_clear() {
        ProjectRunnables::Allow
    } else {
        println!("\nThis backup asks to run:");
        for line in gate.gated.describe() {
            println!("  {line}");
        }
        println!("\nEvery command above will run in your sessions if you accept.");
        if ask("Accept these and restore everything?")? {
            sync::approve(workspace.base(), &gate.fingerprint)?;
            ProjectRunnables::Allow
        } else {
            println!("Restoring everything except those.");
            ProjectRunnables::Deny
        }
    };

    let mut auth = AuthStore::load();
    sync::apply(&restore, &mut settings, &mut auth, runnables);
    settings.save_sync()?;
    auth.save();

    println!(
        "Restored version {} from {}.",
        stored.version,
        workspace.base()
    );
    Ok(())
}

/// Ask a yes-or-no question on the terminal.
///
/// Anything but an explicit yes is a no, and a stdin that is not a terminal is
/// a no as well: a script that pipes nothing must not be read as consent to
/// run a command nobody saw.
fn ask(question: &str) -> anyhow::Result<bool> {
    use std::io::{BufRead, Write};
    if !std::io::stdin().is_terminal() {
        println!("{question} [y/N] no (nothing is attached to answer)");
        return Ok(false);
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn a_flag_reads_the_value_after_it() {
        assert_eq!(
            flag(
                &args(["login", "--email", "ayse@firma.com"].as_ref()),
                "--email"
            ),
            Some("ayse@firma.com")
        );
    }

    #[test]
    fn a_flag_with_nothing_after_it_reads_nothing() {
        assert_eq!(flag(&args(["login", "--email"].as_ref()), "--email"), None);
        assert_eq!(flag(&args(["login"].as_ref()), "--email"), None);
    }

    /// The one rule the usage text has to state, because getting it wrong
    /// means a password in the shell history and in every `ps` listing.
    #[test]
    fn the_usage_says_where_the_password_comes_from() {
        assert!(USAGE.contains("read from stdin"));
        assert!(!USAGE.contains("--password"));
    }

    #[test]
    fn a_missing_server_and_a_dead_session_say_different_things() {
        // One means "you never joined", the other means "log in again", and a
        // user can only act on the difference.
        let nothing = connected(&Settings::default()).err().expect("no server");
        assert!(nothing.to_string().contains("no workspace server"));

        let configured = Settings {
            workspace: Some(WorkspaceSettings {
                url: "https://mikmik.firma.com".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let stale = connected(&configured).err().expect("no session");
        assert!(stale.to_string().contains("no live session"), "{stale}");
    }

    #[test]
    fn an_unreachable_address_is_refused_before_a_password_is_read() {
        // `WorkspaceClient::new` validates, and `login` builds the client
        // before it touches stdin.
        let plain_http = WorkspaceSettings {
            url: "http://mikmik.firma.com".to_string(),
            ..Default::default()
        };
        assert!(WorkspaceClient::new(&plain_http).is_err());
    }
}
