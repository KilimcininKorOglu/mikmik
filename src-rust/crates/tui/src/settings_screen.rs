// settings_screen.rs — Flat searchable settings interface.
//
// Opened by /config or /settings commands. Shows all editable settings
// in a single scrollable list with live search filtering.
// Changes are persisted via Settings::save_sync() or settings.json writes.

use crate::overlays::{
    centered_rect, modal_search_line, render_dark_overlay, render_dialog_bg, MIKMIK_ACCENT,
    MIKMIK_MUTED, MIKMIK_PANEL_BG,
};
use mikmik_core::config::{Config, Settings};
use mikmik_core::output_styles::find_style;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum SettingKind {
    Bool,
    Enum {
        options: Vec<&'static str>,
    },
    Number,
    /// Free text, edited the same way as `Number` but never parsed.
    Text,
    /// A model, chosen from the same picker `/model` opens rather than typed.
    ///
    /// Enter does not edit the row: it asks the session loop to open the
    /// picker, because the list of models belongs to the accounts and the
    /// settings screen has no way to fetch it.
    ModelPicker,
}

/// Seeded into the SearXNG address prompt. It is the port SearXNG binds in its
/// own `settings.yml` template and in the official `searxng-docker` compose.
pub const DEFAULT_SEARXNG_URL: &str = "http://localhost:8080";

/// What an unset compact model reads as, and the picker row that clears it.
pub const USE_THE_TURNS_MODEL: &str = "Use the turn's model";

#[derive(Debug, Clone)]
pub struct SettingsEntry {
    pub key: String,
    pub label: String,
    pub description: String,
    pub kind: SettingKind,
    pub value: String,
}

pub struct SettingsScreen {
    pub visible: bool,
    pub search_query: String,
    pub selected_idx: usize,
    pub scroll_offset: usize,
    /// Which field is being edited (field name as key).
    pub edit_field: Option<String>,
    /// Current buffer content while editing a field.
    pub edit_value: String,
    /// Snapshot of settings at open time.
    pub settings_snapshot: Settings,
    /// Pending changes (field_name → new_value string).
    pub pending_changes: HashMap<String, String>,
    /// A setting whose value is picked from the model picker, waiting for the
    /// session loop to open it.
    ///
    /// The settings screen cannot open the picker itself: the model list is
    /// fetched per account by the session loop, which owns the registry and
    /// the credentials. Taken with [`SettingsScreen::take_pending_model_picker`].
    pending_model_picker: Option<String>,
    /// Why the last write to `settings.json` failed, if it did.
    ///
    /// A settings screen that reports "true" while the file on disk still says
    /// "false" is worse than one that refuses the change, so every save records
    /// its outcome here and the footer shows it.
    pub save_error: Option<String>,
    /// Number of successful writes, so the session loop can notice one.
    saves: u64,

    // ---- Real settings fields ----
    pub auto_compact: bool,
    pub auto_memory: bool,
    pub agents_md: bool,
    pub claude_md: bool,
    pub rules_enabled: bool,
    pub lsp_auto_detect: bool,
    pub lsp_warmup_on_start: bool,
    pub lsp_diagnostics_on_write: bool,
    pub lsp_format_on_write: bool,
    pub notifications: bool,
    pub notify_on_question: bool,
    pub notify_on_plan_ready: bool,
    pub notify_on_permission: bool,
    pub notify_on_turn_complete: bool,
    pub notify_sound: bool,
    pub show_turn_duration: bool,
    pub show_message_timestamps: bool,
    pub output_style: String,
    pub reduce_motion: bool,
    pub companion_enabled: bool,
    pub terminal_progress_bar: bool,
    pub verbose: bool,
    pub cursor_blink_enabled: bool,
    pub auto_copy_enabled: bool,
    pub mouse_capture: bool,
    pub show_cwd: bool,
    pub show_git_branch: bool,
    pub compact_threshold: String,
    pub auto_commits: bool,
    pub include_ignored_files: bool,
    pub web_search_fallback: bool,
    pub timeline_enabled: bool,
    pub live_tool_output: bool,
    /// Empty when no SearXNG instance is configured.
    pub searxng_url: String,
    /// The model that writes every summary, or empty for "the one this turn
    /// is using". Stored canonically, so it names its account.
    pub compact_model: String,
    pub output_format: String,
    pub disable_claude_mds: bool,
    pub file_injection_enabled: bool,
    pub file_autocomplete_limit: String,
    pub file_autocomplete_show_hidden_files: bool,
    pub file_injection_max_size: String,
}

impl SettingsScreen {
    pub fn new() -> Self {
        let settings_snapshot = Settings::load_sync().unwrap_or_default();
        let mut screen = Self {
            visible: false,
            search_query: String::new(),
            selected_idx: 0,
            scroll_offset: 0,
            edit_field: None,
            edit_value: String::new(),
            settings_snapshot: settings_snapshot.clone(),
            pending_changes: HashMap::new(),
            pending_model_picker: None,
            save_error: None,
            saves: 0,
            auto_compact: false,
            auto_memory: false,
            agents_md: true,
            claude_md: false,
            rules_enabled: true,
            lsp_auto_detect: true,
            lsp_warmup_on_start: false,
            lsp_diagnostics_on_write: true,
            lsp_format_on_write: false,
            notifications: true,
            notify_on_question: true,
            notify_on_plan_ready: true,
            notify_on_permission: true,
            notify_on_turn_complete: true,
            notify_sound: false,
            show_turn_duration: false,
            show_message_timestamps: false,
            output_style: "default".to_string(),
            reduce_motion: false,
            companion_enabled: false,
            terminal_progress_bar: true,
            verbose: false,
            cursor_blink_enabled: false,
            auto_copy_enabled: false,
            mouse_capture: true,
            show_cwd: false,
            show_git_branch: false,
            compact_threshold: "90".to_string(),
            auto_commits: false,
            include_ignored_files: false,
            web_search_fallback: false,
            timeline_enabled: false,
            live_tool_output: false,
            searxng_url: String::new(),
            compact_model: String::new(),
            output_format: "text".to_string(),
            disable_claude_mds: false,
            file_injection_enabled: true,
            file_autocomplete_limit: "15".to_string(),
            file_autocomplete_show_hidden_files: false,
            file_injection_max_size: "100".to_string(),
        };
        // Apply settings from snapshot immediately on initialization
        screen.apply_settings_from_snapshot();
        screen
    }

    /// Apply all settings from the snapshot to the screen fields.
    /// This is called on initialization and when opening the settings screen.
    fn apply_settings_from_snapshot(&mut self) {
        self.auto_compact = self.settings_snapshot.effective_auto_compact();
        self.auto_memory =
            mikmik_core::memdir::is_auto_memory_enabled(self.settings_snapshot.auto_memory_enabled);
        let filenames = mikmik_core::claudemd::MemoryFilenames::from_config(
            &self.settings_snapshot.effective_config(),
        );
        self.agents_md = filenames.agents_md;
        self.claude_md = filenames.claude_md;
        self.rules_enabled = self.settings_snapshot.config.effective_rules_enabled();
        self.lsp_auto_detect = self.settings_snapshot.config.effective_lsp_auto_detect();
        self.lsp_warmup_on_start = self
            .settings_snapshot
            .config
            .effective_lsp_warmup_on_start();
        self.notifications = self.settings_snapshot.notifications;
        self.notify_on_question = self.settings_snapshot.notify_on_question;
        self.notify_on_plan_ready = self.settings_snapshot.notify_on_plan_ready;
        self.notify_on_permission = self.settings_snapshot.notify_on_permission;
        self.notify_on_turn_complete = self.settings_snapshot.notify_on_turn_complete;
        self.notify_sound = self.settings_snapshot.notify_sound;
        self.show_turn_duration = self.settings_snapshot.show_turn_duration;
        self.show_message_timestamps = self.settings_snapshot.show_message_timestamps;
        self.output_style = self
            .settings_snapshot
            .config
            .output_style
            .clone()
            .unwrap_or_else(|| "default".to_string());
        self.reduce_motion = self.settings_snapshot.reduce_motion;
        self.companion_enabled = self
            .settings_snapshot
            .companion
            .as_ref()
            .is_some_and(|companion| companion.enabled);
        self.terminal_progress_bar = self.settings_snapshot.terminal_progress_bar;
        self.verbose = self.settings_snapshot.config.verbose;
        self.cursor_blink_enabled = self.settings_snapshot.config.cursor_blink_enabled;
        self.auto_copy_enabled = self.settings_snapshot.auto_copy_on_highlight;
        self.mouse_capture = self.settings_snapshot.config.mouse_capture_enabled();
        self.show_cwd = self.settings_snapshot.show_cwd;
        self.show_git_branch = self.settings_snapshot.show_git_branch;
        // The effective value, not the raw field: an unset threshold is stored
        // as 0 and showing "0" would read as "compact immediately".
        self.compact_threshold = self
            .settings_snapshot
            .config
            .effective_compact_threshold()
            .to_string();
        self.auto_commits = self.settings_snapshot.config.auto_commits.unwrap_or(false);
        self.include_ignored_files = self
            .settings_snapshot
            .config
            .effective_include_ignored_files();
        self.web_search_fallback = self.settings_snapshot.config.web_search_fallback;
        self.timeline_enabled = self.settings_snapshot.config.timeline_enabled;
        self.live_tool_output = self.settings_snapshot.config.live_tool_output;
        self.searxng_url = self
            .settings_snapshot
            .config
            .searxng_url
            .clone()
            .unwrap_or_default();
        self.compact_model = self
            .settings_snapshot
            .config
            .compact_model
            .clone()
            .unwrap_or_default();
        self.output_format = match &self.settings_snapshot.config.output_format {
            mikmik_core::config::OutputFormat::Text => "text".to_string(),
            mikmik_core::config::OutputFormat::Json => "json".to_string(),
            mikmik_core::config::OutputFormat::StreamJson => "stream_json".to_string(),
        };
        self.disable_claude_mds = self.settings_snapshot.config.disable_claude_mds;
        self.file_injection_enabled = self.settings_snapshot.config.file_injection_is_enabled();
        self.file_autocomplete_limit = self
            .settings_snapshot
            .config
            .effective_file_autocomplete_limit()
            .to_string();
        self.file_autocomplete_show_hidden_files = self
            .settings_snapshot
            .config
            .file_autocomplete_show_hidden_files;
        self.file_injection_max_size = self
            .settings_snapshot
            .config
            .effective_file_injection_max_size()
            .to_string();
    }

    pub fn open(&mut self) {
        self.settings_snapshot = Settings::load_sync().unwrap_or_default();
        self.pending_changes.clear();
        self.save_error = None;
        self.edit_field = None;
        self.edit_value.clear();
        self.search_query.clear();
        self.selected_idx = 0;
        self.scroll_offset = 0;
        self.visible = true;

        // Wire real settings from snapshot
        self.apply_settings_from_snapshot();
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.edit_field = None;
        self.edit_value.clear();
    }

    pub fn push_search_char(&mut self, c: char) {
        self.search_query.push(c);
        self.reset_selection();
    }

    pub fn pop_search_char(&mut self) {
        self.search_query.pop();
        self.reset_selection();
    }

    /// Put the cursor back on the first row and scroll back to it.
    ///
    /// The filtered list is usually shorter than the scroll position the user
    /// left behind, so keeping the old offset renders an empty pane: the rows
    /// exist but every one of them is skipped.
    fn reset_selection(&mut self) {
        self.selected_idx = 0;
        self.scroll_offset = 0;
    }

    pub fn select_prev(&mut self) {
        if self.selected_idx > 0 {
            self.selected_idx -= 1;
        }
    }

    pub fn select_next(&mut self, total_visible: usize) {
        if total_visible > 0 && self.selected_idx + 1 < total_visible {
            self.selected_idx += 1;
        }
    }

    /// Write the snapshot back to `settings.json` and keep the outcome.
    fn persist(&mut self) {
        self.save_error = match self.settings_snapshot.save_sync() {
            Ok(()) => {
                // Counted rather than fired here: the session loop owns the
                // async side and reports the change to the plugins.
                self.saves = self.saves.saturating_add(1);
                None
            }
            Err(error) => Some(error.to_string()),
        };
    }

    /// How many times this screen has written `settings.json`.
    pub fn saves(&self) -> u64 {
        self.saves
    }

    /// The setting waiting for a model to be picked for it, if any.
    ///
    /// Read once: taking it is what stops the picker reopening on every frame.
    pub fn take_pending_model_picker(&mut self) -> Option<String> {
        self.pending_model_picker.take()
    }

    /// Record a model chosen from the picker.
    ///
    /// `None` clears the setting, which for the compact model means the
    /// summary goes back to whichever model the turn is using.
    ///
    /// Writes three times on purpose, like `toggle_or_cycle_current`: to the
    /// screen's own copy so the row redraws, to the snapshot about to be
    /// written to disk, and to `config`, which is the one the running session
    /// reads. Skipping the last leaves a setting that looks changed, saves
    /// correctly, and does nothing until the next launch.
    pub fn set_picked_model(&mut self, key: &str, model: Option<String>, config: &mut Config) {
        let model = model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        match key {
            "compact_model" => {
                self.compact_model = model.clone().unwrap_or_default();
                self.settings_snapshot.config.compact_model = model.clone();
                config.compact_model = model;
            }
            _ => return,
        }
        self.persist();
    }

    /// Start editing a field by name, seeding the buffer with current value.
    pub fn start_edit(&mut self, field: &str, current_value: &str) {
        self.edit_field = Some(field.to_string());
        self.edit_value = current_value.to_string();
    }

    /// Commit the current edit to pending_changes.
    pub fn commit_edit(&mut self) {
        if let Some(field) = self.edit_field.take() {
            let value = std::mem::take(&mut self.edit_value);
            self.pending_changes.insert(field, value);
        }
    }

    /// Discard the current edit.
    pub fn cancel_edit(&mut self) {
        self.edit_field = None;
        self.edit_value.clear();
    }

    /// Apply all pending changes to settings and persist them.
    ///
    /// Each field is written to the caller's live config *and* to the snapshot
    /// that gets saved. Copying the whole live config over the snapshot instead
    /// would drop every toggle made earlier on this screen, because a toggle
    /// only ever writes to the snapshot.
    pub fn apply_and_save(&mut self, config: &mut Config) {
        // Collected rather than written inside the loop: the loop borrows
        // `pending_changes`, and writing a plugin option needs the screen.
        let mut plugin_options: Vec<(String, String, serde_json::Value)> = Vec::new();
        for (field, value) in &self.pending_changes {
            let saved = &mut self.settings_snapshot.config;
            match field.as_str() {
                "max_tokens" => {
                    if let Ok(n) = value.parse::<u32>() {
                        config.max_tokens = Some(n);
                        saved.max_tokens = Some(n);
                    }
                }
                "output_style" => {
                    let style = if value.is_empty() {
                        None
                    } else {
                        Some(value.clone())
                    };
                    config.output_style = style.clone();
                    saved.output_style = style;
                }
                "searxng_url" => {
                    // An empty address is how the user switches SearXNG off
                    // from the edit prompt, so it clears the key rather than
                    // storing a blank one.
                    let trimmed = value.trim();
                    let url = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                    config.searxng_url = url.clone();
                    saved.searxng_url = url;
                    self.searxng_url = trimmed.to_string();
                }
                "compact_threshold" => {
                    if let Ok(n) = value.parse::<u8>() {
                        config.compact_threshold = n;
                        saved.compact_threshold = n;
                        self.compact_threshold = value.clone();
                    }
                }
                "fileAutocompleteLimit" => {
                    if let Ok(n) = value.parse::<usize>() {
                        config.file_autocomplete_limit = Some(n);
                        saved.file_autocomplete_limit = Some(n);
                        self.file_autocomplete_limit = value.clone();
                    }
                }
                "fileInjectionMaxSize" => {
                    if let Ok(n) = value.parse::<usize>() {
                        config.file_injection_max_size = Some(n);
                        saved.file_injection_max_size = Some(n);
                        self.file_injection_max_size = value.clone();
                    }
                }
                key => {
                    if let Some((plugin, option)) = split_plugin_option_key(key) {
                        plugin_options.push((
                            plugin.to_string(),
                            option.to_string(),
                            parse_plugin_option_value(value),
                        ));
                    }
                }
            }
        }
        for (plugin, option, value) in plugin_options {
            self.set_plugin_option(&plugin, &option, value);
        }
        self.persist();
        self.pending_changes.clear();
    }

    /// Record a value for one option a plugin declares.
    ///
    /// An empty string clears the option instead of storing a blank, which is
    /// how the edit prompt takes a value back off a plugin.
    pub fn set_plugin_option(&mut self, plugin: &str, option: &str, value: serde_json::Value) {
        let clears = matches!(&value, serde_json::Value::String(s) if s.is_empty());
        let values = self
            .settings_snapshot
            .plugin_config
            .entry(plugin.to_string())
            .or_default();
        if clears {
            values.remove(option);
        } else {
            values.insert(option.to_string(), value);
        }
        if values.is_empty() {
            self.settings_snapshot.plugin_config.remove(plugin);
        }
    }
}

/// Read an edited plugin option back into JSON.
///
/// A number stays a number and `true`/`false` stay booleans, so a plugin that
/// parses `CLAUDE_PLUGIN_CONFIG` reads the type its manifest declared. Anything
/// else is a string.
fn parse_plugin_option_value(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        return serde_json::Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return serde_json::Value::Bool(false);
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        if let Some(number) = serde_json::Number::from_f64(number) {
            return serde_json::Value::Number(number);
        }
    }
    serde_json::Value::String(trimmed.to_string())
}

impl Default for SettingsScreen {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Settings entries definition
// ---------------------------------------------------------------------------

fn all_entries(screen: &SettingsScreen) -> Vec<SettingsEntry> {
    let mut entries = vec![
        SettingsEntry {
            key: "max_tokens".into(),
            label: "Max Tokens".into(),
            description: "Maximum tokens per response.".into(),
            kind: SettingKind::Number,
            value: screen.settings_snapshot.config.max_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| mikmik_core::constants::DEFAULT_MAX_TOKENS.to_string()),
        },
        SettingsEntry {
            key: "auto_compact".into(),
            label: "Auto-compact".into(),
            description: "Automatically compact turns at threshold.".into(),
            kind: SettingKind::Bool,
            value: if screen.auto_compact { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "rules_enabled".into(),
            label: "Conditional rules".into(),
            description: "Let a rule file with a condition speak when the model writes something \
                          it matches."
                .into(),
            kind: SettingKind::Bool,
            value: if screen.rules_enabled { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "lsp_auto_detect".into(),
            label: "Detect language servers".into(),
            description:
                "Start a bundled language server when the project has its marker and its binary."
                    .into(),
            kind: SettingKind::Bool,
            value: if screen.lsp_auto_detect { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "lsp_warmup_on_start".into(),
            label: "Start language servers early".into(),
            description: "Start the project's servers with the session, so the first request \
                          does not wait for indexing."
                .into(),
            kind: SettingKind::Bool,
            value: if screen.lsp_warmup_on_start { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "lsp_diagnostics_on_write".into(),
            label: "Report problems after a write".into(),
            description: "Append the language server's new problems to the result of a write."
                .into(),
            kind: SettingKind::Bool,
            value: if screen.lsp_diagnostics_on_write { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "lsp_format_on_write".into(),
            label: "Format with the language server".into(),
            description: "Format a file with its language server after writing it.".into(),
            kind: SettingKind::Bool,
            value: if screen.lsp_format_on_write { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "auto_memory".into(),
            label: "Auto memory".into(),
            description: "Keep a memory directory for this project and show it to the model."
                .into(),
            kind: SettingKind::Bool,
            value: if screen.auto_memory { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "agents_md".into(),
            label: "Read AGENTS.md".into(),
            description: "Load AGENTS.md files into the prompt.".into(),
            kind: SettingKind::Bool,
            value: if screen.agents_md { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "claude_md".into(),
            label: "Read CLAUDE.md".into(),
            description: "Load CLAUDE.md files alongside AGENTS.md.".into(),
            kind: SettingKind::Bool,
            value: if screen.claude_md { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "notifications".into(),
            label: "Desktop notifications".into(),
            description: "Master switch for the notification settings below.".into(),
            kind: SettingKind::Bool,
            value: if screen.notifications { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "notify_on_question".into(),
            label: "Notify on question".into(),
            description: "Notify when a question is waiting for an answer.".into(),
            kind: SettingKind::Bool,
            value: if screen.notify_on_question { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "notify_on_plan_ready".into(),
            label: "Notify on plan ready".into(),
            description: "Notify when a plan is waiting for approval.".into(),
            kind: SettingKind::Bool,
            value: if screen.notify_on_plan_ready { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "notify_on_permission".into(),
            label: "Notify on permission".into(),
            description: "Notify when a tool is waiting for permission.".into(),
            kind: SettingKind::Bool,
            value: if screen.notify_on_permission { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "notify_on_turn_complete".into(),
            label: "Notify on turn complete".into(),
            description: "Notify when a turn finishes and the prompt is free.".into(),
            kind: SettingKind::Bool,
            value: if screen.notify_on_turn_complete { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "notify_sound".into(),
            label: "Notification sound".into(),
            description: "Play a short sound with each notification.".into(),
            kind: SettingKind::Bool,
            value: if screen.notify_sound { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "show_turn_duration".into(),
            label: "Show turn duration".into(),
            description: "Display elapsed time per turn in status bar.".into(),
            kind: SettingKind::Bool,
            value: if screen.show_turn_duration { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "show_message_timestamps".into(),
            label: "Show message timestamps".into(),
            description: "Display the local time under each message.".into(),
            kind: SettingKind::Bool,
            value: if screen.show_message_timestamps { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "output_style".into(),
            label: "Output Style".into(),
            description: "Controls the verbosity and format of responses.".into(),
            kind: SettingKind::Enum {
                options: vec!["default", "concise", "explanatory", "learning"],
            },
            value: screen.output_style.clone(),
        },
        SettingsEntry {
            key: "reduce_motion".into(),
            label: "Reduce motion".into(),
            description: "Disable UI animations.".into(),
            kind: SettingKind::Bool,
            value: if screen.reduce_motion { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "companion_enabled".into(),
            label: "Companion".into(),
            description: "Show a small creature beside the input box. See /buddy.".into(),
            kind: SettingKind::Bool,
            value: if screen.companion_enabled { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "terminal_progress_bar".into(),
            label: "Terminal progress bar".into(),
            description: "Show progress during tool use.".into(),
            kind: SettingKind::Bool,
            value: if screen.terminal_progress_bar { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "verbose".into(),
            label: "Verbose logging".into(),
            description: "Log additional debug information. Takes effect on next session.".into(),
            kind: SettingKind::Bool,
            value: if screen.verbose { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "cursor_blink_enabled".into(),
            label: "Cursor blinking".into(),
            description: "Enable cursor blinking in the chat prompt.".into(),
            kind: SettingKind::Bool,
            value: if screen.cursor_blink_enabled { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "auto_copy_enabled".into(),
            label: "Auto-copy on highlight".into(),
            description: "Automatically copy highlighted text to clipboard.".into(),
            kind: SettingKind::Bool,
            value: if screen.auto_copy_enabled { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "mouse_capture".into(),
            label: "Mouse capture".into(),
            description: "Capture the mouse for scroll/right-click/drag-select. Turn off for native terminal text selection. Takes effect on next session.".into(),
            kind: SettingKind::Bool,
            value: if screen.mouse_capture { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "show_cwd".into(),
            label: "Show current directory".into(),
            description: "Display the current working directory in the footer.".into(),
            kind: SettingKind::Bool,
            value: if screen.show_cwd { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "show_git_branch".into(),
            label: "Show git branch".into(),
            description: "Display the current git branch in the footer.".into(),
            kind: SettingKind::Bool,
            value: if screen.show_git_branch { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "compact_threshold".into(),
            label: "Auto-compact threshold".into(),
            description: "Context usage % at which to trigger auto-compact (0-100).".into(),
            kind: SettingKind::Number,
            value: screen.compact_threshold.clone(),
        },
        SettingsEntry {
            key: "auto_commits".into(),
            label: "Auto-commits".into(),
            description: "Automatically snapshot changes to git via shadow-git.".into(),
            kind: SettingKind::Bool,
            value: if screen.auto_commits { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "include_ignored_files".into(),
            label: "Search ignored files".into(),
            description: "Let Glob and Grep search files that .gitignore excludes.".into(),
            kind: SettingKind::Bool,
            value: if screen.include_ignored_files {
                "true"
            } else {
                "false"
            }
            .to_string(),
        },
        SettingsEntry {
            key: "searxng".into(),
            label: "SearXNG".into(),
            description: "Search through a self-hosted SearXNG instance. Turning it on asks for the address.".into(),
            kind: SettingKind::Bool,
            value: if screen.searxng_url.is_empty() {
                "false"
            } else {
                "true"
            }
            .to_string(),
        },
        SettingsEntry {
            key: "compact_model".into(),
            label: "Compact model".into(),
            description:
                "The model that writes every conversation summary. Unset, the summary is written by whichever model the turn is using."
                    .into(),
            kind: SettingKind::ModelPicker,
            value: if screen.compact_model.is_empty() {
                USE_THE_TURNS_MODEL.to_string()
            } else {
                screen.compact_model.clone()
            },
        },
        SettingsEntry {
            key: "searxng_url".into(),
            label: "SearXNG URL".into(),
            description: "Base address of the instance, for example http://localhost:8080. Empty turns SearXNG off.".into(),
            kind: SettingKind::Text,
            value: screen.searxng_url.clone(),
        },
        SettingsEntry {
            key: "timeline_enabled".into(),
            label: "Execution timeline".into(),
            description:
                "Record each tool call and turn, and offer the panel through /timeline and Ctrl+Shift+L."
                    .into(),
            kind: SettingKind::Bool,
            value: if screen.timeline_enabled {
                "true"
            } else {
                "false"
            }
            .to_string(),
        },
        SettingsEntry {
            key: "live_tool_output".into(),
            label: "Live tool output".into(),
            description: "Show a running command's output as it arrives instead of only when it finishes."
                .into(),
            kind: SettingKind::Bool,
            value: if screen.live_tool_output {
                "true"
            } else {
                "false"
            }
            .to_string(),
        },
        SettingsEntry {
            key: "web_search_fallback".into(),
            label: "Web search fallback".into(),
            description: "Let WebSearch continue with Brave or DuckDuckGo when SearXNG is down.".into(),
            kind: SettingKind::Bool,
            value: if screen.web_search_fallback {
                "true"
            } else {
                "false"
            }
            .to_string(),
        },
        SettingsEntry {
            key: "output_format".into(),
            label: "Output format".into(),
            description: "How responses are formatted: text, JSON, or streaming JSON.".into(),
            kind: SettingKind::Enum {
                options: vec!["text", "json", "streamjson"],
            },
            value: screen.output_format.clone(),
        },
        SettingsEntry {
            key: "disable_claude_mds".into(),
            label: "Disable CLAUDE.md".into(),
            description: "Ignore CLAUDE.md files in projects (use defaults instead).".into(),
            kind: SettingKind::Bool,
            value: if screen.disable_claude_mds { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "fileInjectionEnabled".into(),
            label: "File injection (@)".into(),
            description: "Auto-inject @file references into message context.".into(),
            kind: SettingKind::Bool,
            value: if screen.file_injection_enabled { "true" } else { "false" }.to_string(),
        },
    ];

    // Only show these if file injection is enabled
    if screen.file_injection_enabled {
        entries.push(SettingsEntry {
            key: "fileAutocompleteLimit".into(),
            label: "File autocomplete limit".into(),
            description: "Max suggestions shown in @ autocomplete (type more to narrow results)."
                .into(),
            kind: SettingKind::Number,
            value: screen.file_autocomplete_limit.clone(),
        });
        entries.push(SettingsEntry {
            key: "fileAutocompleteShowHiddenFiles".into(),
            label: "Show hidden files".into(),
            description: "Include hidden files (.) in @ autocomplete.".into(),
            kind: SettingKind::Bool,
            value: if screen.file_autocomplete_show_hidden_files {
                "true"
            } else {
                "false"
            }
            .to_string(),
        });
        entries.push(SettingsEntry {
            key: "fileInjectionMaxSize".into(),
            label: "File injection max size".into(),
            description: "Max file size to auto-inject (KB, 0=no limit).".into(),
            kind: SettingKind::Number,
            value: screen.file_injection_max_size.clone(),
        });
    }

    entries.extend(plugin_config_entries(screen));

    entries
}

/// The prefix a plugin option's key carries in this screen and in
/// `pending_changes`.
const PLUGIN_OPTION_PREFIX: &str = "plugin.";

/// Split `plugin.<plugin>.<option>` back into its two names.
pub fn split_plugin_option_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix(PLUGIN_OPTION_PREFIX)?.split_once('.')
}

/// One entry per option the session's plugins declare under `userConfig`.
///
/// Without these the options are parsed out of every manifest and then never
/// shown, so no value can be set and the plugin reads nothing.
fn plugin_config_entries(screen: &SettingsScreen) -> Vec<SettingsEntry> {
    let Some(registry) = mikmik_plugins::global_plugin_registry() else {
        return Vec::new();
    };

    let mut plugins = registry.enabled();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));

    let mut entries = Vec::new();
    for plugin in plugins {
        let mut options: Vec<_> = plugin.manifest.user_config.iter().collect();
        options.sort_by(|a, b| a.0.cmp(b.0));

        for (option_key, option) in options {
            let key = format!("{PLUGIN_OPTION_PREFIX}{}.{option_key}", plugin.name);
            let stored = screen
                .settings_snapshot
                .plugin_config
                .get(&plugin.name)
                .and_then(|values| values.get(option_key));
            let value = match stored.or(option.default.as_ref()) {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => match option.value_type {
                    mikmik_plugins::UserConfigValueType::Boolean => "false".to_string(),
                    _ => String::new(),
                },
            };
            let kind = match option.value_type {
                mikmik_plugins::UserConfigValueType::Boolean => SettingKind::Bool,
                mikmik_plugins::UserConfigValueType::Number => SettingKind::Number,
                _ => SettingKind::Text,
            };

            let mut description = option.description.clone();
            if option.required {
                description.push_str(" (required)");
            }
            if option.sensitive {
                description.push_str(" Stored in settings.json in the clear.");
            }

            entries.push(SettingsEntry {
                key,
                label: format!("{}: {}", plugin.name, option.title),
                description,
                kind,
                value,
            });
        }
    }
    entries
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_settings_screen(frame: &mut Frame, screen: &SettingsScreen, area: Rect) {
    if !screen.visible {
        return;
    }

    render_dark_overlay(frame, area);

    // 80% width, 90% height, centred
    let w = (area.width * 4 / 5)
        .max(60)
        .min(area.width.saturating_sub(2));
    let h = (area.height * 9 / 10)
        .max(20)
        .min(area.height.saturating_sub(2));
    let popup = centered_rect(w, h, area);
    render_dialog_bg(frame, popup);

    // Inset inner area
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };

    if inner.height < 6 {
        return;
    }

    // Split into header + search + spacer + content + description + footer
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Percentage(50),
            Constraint::Length(1),
        ])
        .split(inner);

    let header_area = layout[0];
    let search_area = layout[1];
    let content_area = layout[3];
    let description_area = layout[4];
    let footer_area = layout[5];

    // Header
    let title = Line::from(vec![
        Span::styled(
            " Settings",
            Style::default()
                .fg(MIKMIK_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" — MikMik", Style::default().fg(MIKMIK_MUTED)),
        Span::styled(
            format!(
                "{:>width$}",
                "Esc close",
                width = inner.width.saturating_sub(19) as usize
            ),
            Style::default().fg(MIKMIK_MUTED),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).style(Style::default().bg(MIKMIK_PANEL_BG)),
        header_area,
    );

    // Search
    let search_line = modal_search_line(
        &screen.search_query,
        "Type to search settings...",
        Color::DarkGray,
        MIKMIK_ACCENT,
    );
    frame.render_widget(
        Paragraph::new(search_line).style(Style::default().bg(MIKMIK_PANEL_BG)),
        search_area,
    );

    // Content
    render_settings_list(frame, screen, content_area);

    // Description of selected entry
    let all = all_entries(screen);
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| {
            e.label
                .to_lowercase()
                .contains(&screen.search_query.to_lowercase())
        })
        .collect();

    let desc_text = if let Some(entry) = filtered.get(screen.selected_idx) {
        // For Output Style, show current selection and all available options with descriptions
        if entry.key == "output_style" {
            let mut lines = vec![entry.description.to_string(), String::new()];

            // Every style the session can resolve, not just the built-in
            // ones: the field takes a name, so listing less than the resolver
            // accepts would hide a user's or a plugin's style.
            let all_styles = mikmik_core::output_styles::all_styles_with_runtime(
                &mikmik_core::config::Settings::config_dir(),
            );
            let current_style_name = if screen.output_style.is_empty() {
                "default"
            } else {
                &screen.output_style
            };
            if let Some(current_style) = find_style(&all_styles, current_style_name) {
                lines.push(format!(
                    "Current: {} — {}",
                    current_style.label, current_style.description
                ));
                lines.push(String::new());
            }

            lines.push("Available:".to_string());
            for style in &all_styles {
                lines.push(format!("  {} — {}", style.name, style.description));
            }
            lines.join("\n")
        } else {
            entry.description.to_string()
        }
    } else {
        String::new()
    };
    let desc_para = Paragraph::new(desc_text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Left)
        .block(Block::default().padding(ratatui::widgets::Padding::new(1, 0, 1, 0)));
    frame.render_widget(desc_para, description_area);

    // Footer. A failed write outranks the key hints: the list above already
    // shows the new value, so without this the screen would claim a change
    // that never reached disk.
    let footer = if let Some(error) = &screen.save_error {
        Line::from(vec![
            Span::styled(
                " Not saved ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(error.clone(), Style::default().fg(Color::Red)),
        ])
    } else if screen.edit_field.is_some() {
        Line::from(vec![
            Span::styled(
                " Enter ",
                Style::default()
                    .fg(MIKMIK_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("save  "),
            Span::styled(
                " Esc ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("cancel"),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " ↑↓ ",
                Style::default()
                    .fg(MIKMIK_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("navigate  "),
            Span::styled(
                " Enter ",
                Style::default()
                    .fg(MIKMIK_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("toggle/edit  "),
            Span::styled(
                " Esc ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("close"),
        ])
    };
    let footer_para = Paragraph::new(vec![footer])
        .style(Style::default().fg(MIKMIK_MUTED).bg(MIKMIK_PANEL_BG))
        .alignment(Alignment::Center);
    frame.render_widget(footer_para, footer_area);
}

fn render_settings_list(frame: &mut Frame, screen: &SettingsScreen, area: Rect) {
    let all = all_entries(screen);

    // Filter entries by search query
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| {
            e.label
                .to_lowercase()
                .contains(&screen.search_query.to_lowercase())
        })
        .collect();

    if filtered.is_empty() {
        let para = Paragraph::new("No settings match your search.")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, area);
        return;
    }

    // Build lines
    let mut lines: Vec<Line> = Vec::new();
    let visible_rows = area.height as usize;

    for (i, entry) in filtered.iter().enumerate() {
        let is_selected = i == screen.selected_idx;
        let marker = if is_selected { "►" } else { " " };

        let label_len = 40usize;

        // Show edit value if currently editing this field, otherwise show the entry value
        let value_str = if screen.edit_field.as_deref() == Some(entry.key.as_str()) && is_selected {
            format!("{}_ ", screen.edit_value) // Add cursor indicator
        } else {
            entry.value.clone()
        };

        let row_style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(MIKMIK_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let line = Line::from(vec![
            Span::styled(
                format!("   {} {:<label_len$}", marker, entry.label),
                row_style,
            ),
            Span::styled(value_str, row_style),
        ]);
        lines.push(line);
    }

    // Scroll tracking is handled in update_scroll_offset_for_selection()

    // Apply manual scrolling
    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(screen.scroll_offset)
        .take(visible_rows.max(1))
        .collect();

    let para = Paragraph::new(visible_lines);
    frame.render_widget(para, area);
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub fn handle_settings_key(
    screen: &mut SettingsScreen,
    config: &mut Config,
    key: crossterm::event::KeyEvent,
) -> bool {
    use crossterm::event::KeyCode;

    if !screen.visible {
        return false;
    }

    // Editing mode
    if screen.edit_field.is_some() {
        match key.code {
            KeyCode::Enter => {
                screen.commit_edit();
                screen.apply_and_save(config);
            }
            KeyCode::Esc => {
                screen.cancel_edit();
            }
            KeyCode::Backspace => {
                screen.edit_value.pop();
            }
            KeyCode::Char(c) => {
                screen.edit_value.push(c);
            }
            _ => {}
        }
        return true;
    }

    // Navigation mode
    match key.code {
        KeyCode::Enter => {
            toggle_or_cycle_current(screen, config);
        }
        KeyCode::Esc => {
            if !screen.search_query.is_empty() {
                screen.search_query.clear();
                screen.selected_idx = 0;
            } else {
                screen.close();
            }
        }
        KeyCode::Backspace => {
            screen.pop_search_char();
        }
        KeyCode::Up => {
            screen.select_prev();
            update_scroll_offset_for_selection(screen);
        }
        KeyCode::Down => {
            let all = all_entries(screen);
            let filtered: Vec<_> = all
                .iter()
                .filter(|e| {
                    e.label
                        .to_lowercase()
                        .contains(&screen.search_query.to_lowercase())
                })
                .collect();
            screen.select_next(filtered.len());
            update_scroll_offset_for_selection(screen);
        }
        KeyCode::Char(c) => {
            screen.push_search_char(c);
        }
        _ => {}
    }
    true
}

fn update_scroll_offset_for_selection(screen: &mut SettingsScreen) {
    let visible_rows = 10; // Rough estimate, will be actual in real usage
    if screen.selected_idx < screen.scroll_offset {
        screen.scroll_offset = screen.selected_idx;
    } else if screen.selected_idx >= screen.scroll_offset + visible_rows {
        screen.scroll_offset = screen.selected_idx.saturating_sub(visible_rows - 1);
    }
}

/// Flip the selected setting.
///
/// Writes each value three times on purpose: to the screen's own copy so the
/// row redraws, to the snapshot that is about to be written to disk, and to
/// `config`, which is the one the running session reads. Skipping the last one
/// leaves a setting that looks changed, saves correctly, and does nothing until
/// the next launch.
fn toggle_or_cycle_current(screen: &mut SettingsScreen, config: &mut Config) {
    let all = all_entries(screen);
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| {
            e.label
                .to_lowercase()
                .contains(&screen.search_query.to_lowercase())
        })
        .collect();

    if let Some(entry) = filtered.get(screen.selected_idx) {
        match entry.kind {
            SettingKind::Bool => {
                let new_value = entry.value != "true";
                match entry.key.as_str() {
                    "auto_compact" => {
                        screen.auto_compact = new_value;
                        screen.settings_snapshot.auto_compact = Some(new_value);
                        // The query loop reads the nested key, so write both or
                        // the toggle saves somewhere the session never looks.
                        screen.settings_snapshot.config.auto_compact = Some(new_value);
                    }
                    "rules_enabled" => {
                        screen.rules_enabled = new_value;
                        screen.settings_snapshot.config.rules_enabled = Some(new_value);
                    }
                    "lsp_auto_detect" => {
                        screen.lsp_auto_detect = new_value;
                        // Config-level only: the tool reads the nested key and
                        // there is no flat twin to keep in step.
                        screen.settings_snapshot.config.lsp_auto_detect = Some(new_value);
                    }
                    "lsp_warmup_on_start" => {
                        screen.lsp_warmup_on_start = new_value;
                        screen.settings_snapshot.config.lsp_warmup_on_start = Some(new_value);
                    }
                    "lsp_diagnostics_on_write" => {
                        screen.lsp_diagnostics_on_write = new_value;
                        screen.settings_snapshot.config.lsp_diagnostics_on_write = Some(new_value);
                    }
                    "lsp_format_on_write" => {
                        screen.lsp_format_on_write = new_value;
                        screen.settings_snapshot.config.lsp_format_on_write = Some(new_value);
                    }
                    "auto_memory" => {
                        screen.auto_memory = new_value;
                        screen.settings_snapshot.auto_memory_enabled = Some(new_value);
                        // Both keys again, for the same reason as auto_compact.
                        screen.settings_snapshot.config.auto_memory_enabled = Some(new_value);
                    }
                    "agents_md" => {
                        screen.agents_md = new_value;
                        screen.settings_snapshot.agents_md_enabled = Some(new_value);
                        screen.settings_snapshot.config.agents_md_enabled = Some(new_value);
                    }
                    "claude_md" => {
                        screen.claude_md = new_value;
                        screen.settings_snapshot.claude_md_enabled = Some(new_value);
                        screen.settings_snapshot.config.claude_md_enabled = Some(new_value);
                    }
                    "notifications" => {
                        screen.notifications = new_value;
                        screen.settings_snapshot.notifications = new_value;
                    }
                    "notify_on_question" => {
                        screen.notify_on_question = new_value;
                        screen.settings_snapshot.notify_on_question = new_value;
                    }
                    "notify_on_plan_ready" => {
                        screen.notify_on_plan_ready = new_value;
                        screen.settings_snapshot.notify_on_plan_ready = new_value;
                    }
                    "notify_on_permission" => {
                        screen.notify_on_permission = new_value;
                        screen.settings_snapshot.notify_on_permission = new_value;
                    }
                    "notify_on_turn_complete" => {
                        screen.notify_on_turn_complete = new_value;
                        screen.settings_snapshot.notify_on_turn_complete = new_value;
                    }
                    "notify_sound" => {
                        screen.notify_sound = new_value;
                        screen.settings_snapshot.notify_sound = new_value;
                    }
                    "show_turn_duration" => {
                        screen.show_turn_duration = new_value;
                        screen.settings_snapshot.show_turn_duration = new_value;
                    }
                    "show_message_timestamps" => {
                        screen.show_message_timestamps = new_value;
                        screen.settings_snapshot.show_message_timestamps = new_value;
                    }
                    "reduce_motion" => {
                        screen.reduce_motion = new_value;
                        screen.settings_snapshot.reduce_motion = new_value;
                    }
                    "companion_enabled" => {
                        screen.companion_enabled = new_value;
                        let mut companion = screen
                            .settings_snapshot
                            .companion
                            .take()
                            .unwrap_or_default();
                        companion.enabled = new_value;
                        screen.settings_snapshot.companion = Some(companion);
                    }
                    "terminal_progress_bar" => {
                        screen.terminal_progress_bar = new_value;
                        screen.settings_snapshot.terminal_progress_bar = new_value;
                    }
                    "verbose" => {
                        screen.verbose = new_value;
                        screen.settings_snapshot.config.verbose = new_value;
                        config.verbose = new_value;
                    }
                    "cursor_blink_enabled" => {
                        screen.cursor_blink_enabled = new_value;
                        screen.settings_snapshot.config.cursor_blink_enabled = new_value;
                        config.cursor_blink_enabled = new_value;
                    }
                    "auto_copy_enabled" => {
                        screen.auto_copy_enabled = new_value;
                        screen.settings_snapshot.auto_copy_on_highlight = new_value;
                    }
                    "mouse_capture" => {
                        screen.mouse_capture = new_value;
                        // Persist only the off state; on is the default, so clear the key.
                        screen.settings_snapshot.config.mouse_capture =
                            if new_value { None } else { Some(false) };
                        config.mouse_capture = if new_value { None } else { Some(false) };
                    }
                    "show_cwd" => {
                        screen.show_cwd = new_value;
                        screen.settings_snapshot.show_cwd = new_value;
                    }
                    "show_git_branch" => {
                        screen.show_git_branch = new_value;
                        screen.settings_snapshot.show_git_branch = new_value;
                    }
                    "auto_commits" => {
                        screen.auto_commits = new_value;
                        screen.settings_snapshot.config.auto_commits =
                            if new_value { Some(true) } else { None };
                        config.auto_commits = if new_value { Some(true) } else { None };
                    }
                    "include_ignored_files" => {
                        screen.include_ignored_files = new_value;
                        screen.settings_snapshot.config.include_ignored_files = Some(new_value);
                        config.include_ignored_files = Some(new_value);
                    }
                    "searxng" => {
                        if new_value {
                            // Enabling a backend with no address would configure
                            // nothing, so ask for it and let the edit save.
                            screen.start_edit("searxng_url", DEFAULT_SEARXNG_URL);
                            // The edit buffer is drawn on its own row, so the
                            // cursor has to move there or the prompt is invisible.
                            if let Some(idx) = filtered.iter().position(|e| e.key == "searxng_url")
                            {
                                screen.selected_idx = idx;
                            }
                            return;
                        }
                        screen.searxng_url.clear();
                        screen.settings_snapshot.config.searxng_url = None;
                        config.searxng_url = None;
                    }
                    "web_search_fallback" => {
                        screen.web_search_fallback = new_value;
                        screen.settings_snapshot.config.web_search_fallback = new_value;
                        config.web_search_fallback = new_value;
                    }
                    "live_tool_output" => {
                        screen.live_tool_output = new_value;
                        screen.settings_snapshot.config.live_tool_output = new_value;
                        config.live_tool_output = new_value;
                    }
                    "timeline_enabled" => {
                        screen.timeline_enabled = new_value;
                        screen.settings_snapshot.config.timeline_enabled = new_value;
                        config.timeline_enabled = new_value;
                    }
                    "disable_claude_mds" => {
                        screen.disable_claude_mds = new_value;
                        screen.settings_snapshot.config.disable_claude_mds = new_value;
                        config.disable_claude_mds = new_value;
                    }
                    "fileInjectionEnabled" => {
                        screen.file_injection_enabled = new_value;
                        screen.settings_snapshot.config.file_injection_enabled = Some(new_value);
                        config.file_injection_enabled = Some(new_value);
                    }
                    "fileAutocompleteShowHiddenFiles" => {
                        screen.file_autocomplete_show_hidden_files = new_value;
                        screen
                            .settings_snapshot
                            .config
                            .file_autocomplete_show_hidden_files = new_value;
                        config.file_autocomplete_show_hidden_files = new_value;
                    }
                    key => {
                        if let Some((plugin, option)) = split_plugin_option_key(key) {
                            screen.set_plugin_option(
                                plugin,
                                option,
                                serde_json::Value::Bool(new_value),
                            );
                        }
                    }
                }
                screen.persist();
            }
            SettingKind::Enum { ref options } => {
                let current_idx = options.iter().position(|&o| o == entry.value).unwrap_or(0);
                let next_idx = (current_idx + 1) % options.len();
                let new_value = options[next_idx];

                match entry.key.as_str() {
                    "output_style" => {
                        screen.output_style = new_value.to_string();
                        screen.settings_snapshot.config.output_style = Some(new_value.to_string());
                        config.output_style = Some(new_value.to_string());
                    }
                    "output_format" => {
                        screen.output_format = new_value.to_string();
                        let format = match new_value {
                            "json" => mikmik_core::config::OutputFormat::Json,
                            "stream_json" => mikmik_core::config::OutputFormat::StreamJson,
                            _ => mikmik_core::config::OutputFormat::Text,
                        };
                        screen.settings_snapshot.config.output_format = format.clone();
                        config.output_format = format;
                    }
                    _ => {}
                }
                screen.persist();
            }
            SettingKind::Number | SettingKind::Text => {
                screen.start_edit(&entry.key, &entry.value);
            }
            // Nothing changes here. The session loop owns the picker, and the
            // models in it belong to the accounts, which the settings screen
            // cannot reach.
            SettingKind::ModelPicker => {
                screen.pending_model_picker = Some(entry.key.clone());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_screen_new_has_sensible_defaults() {
        let screen = SettingsScreen::new();
        assert!(!screen.visible);
        assert!(screen.search_query.is_empty());
        assert_eq!(screen.selected_idx, 0);
        assert!(screen.edit_field.is_none());
        assert!(screen.edit_value.is_empty());
    }

    /// The keys the list only carries while file injection is on.
    const INJECTION_DEPENDENT_KEYS: [&str; 3] = [
        "fileAutocompleteLimit",
        "fileAutocompleteShowHiddenFiles",
        "fileInjectionMaxSize",
    ];

    fn entry_keys(screen: &SettingsScreen) -> Vec<String> {
        all_entries(screen).into_iter().map(|e| e.key).collect()
    }

    #[test]
    fn the_file_injection_settings_follow_their_toggle() {
        let mut screen = SettingsScreen::new();

        screen.file_injection_enabled = true;
        let keys = entry_keys(&screen);
        for key in INJECTION_DEPENDENT_KEYS {
            assert!(
                keys.iter().any(|k| k == key),
                "{key} must be editable while injection is on"
            );
        }

        screen.file_injection_enabled = false;
        let keys = entry_keys(&screen);
        for key in INJECTION_DEPENDENT_KEYS {
            assert!(
                !keys.iter().any(|k| k == key),
                "{key} configures a feature that is switched off"
            );
        }
        assert!(
            keys.iter().any(|k| k == "fileInjectionEnabled"),
            "the toggle itself must stay reachable, or injection can never be switched back on"
        );
    }

    #[test]
    fn no_setting_is_listed_twice() {
        // The screen keys its edits by this string, so a repeated key would
        // leave one of the two entries permanently uneditable.
        let screen = SettingsScreen::new();
        let mut keys = entry_keys(&screen);
        let listed = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), listed, "duplicate key among {listed} entries");
    }

    #[test]
    fn a_plugin_option_key_splits_into_plugin_and_option() {
        assert_eq!(
            split_plugin_option_key("plugin.my-plugin.apiKey"),
            Some(("my-plugin", "apiKey"))
        );
        assert_eq!(split_plugin_option_key("max_tokens"), None);
        assert_eq!(split_plugin_option_key("plugin.no-option"), None);
    }

    #[test]
    fn an_edited_option_keeps_the_type_its_manifest_declared() {
        assert_eq!(parse_plugin_option_value("true"), serde_json::json!(true));
        assert_eq!(parse_plugin_option_value(" 42 "), serde_json::json!(42.0));
        assert_eq!(
            parse_plugin_option_value("/srv/data"),
            serde_json::json!("/srv/data")
        );
    }

    #[test]
    fn setting_an_option_stores_it_and_an_empty_value_clears_it() {
        let mut screen = SettingsScreen::new();
        screen.set_plugin_option("acme", "apiKey", serde_json::json!("k-1"));
        assert_eq!(
            screen.settings_snapshot.plugin_config["acme"]["apiKey"],
            serde_json::json!("k-1")
        );

        screen.set_plugin_option("acme", "verbose", serde_json::json!(true));
        assert_eq!(screen.settings_snapshot.plugin_config["acme"].len(), 2);

        screen.set_plugin_option("acme", "apiKey", serde_json::json!(""));
        assert!(!screen.settings_snapshot.plugin_config["acme"].contains_key("apiKey"));

        screen.set_plugin_option("acme", "verbose", serde_json::json!(""));
        assert!(
            !screen.settings_snapshot.plugin_config.contains_key("acme"),
            "a plugin with no values left is dropped rather than kept as an empty object"
        );
    }

    #[test]
    fn search_filters_entries_correctly() {
        let screen = SettingsScreen::new();
        let all = all_entries(&screen);
        let filtered: Vec<_> = all
            .iter()
            .filter(|e| e.label.to_lowercase().contains("token"))
            .collect();
        assert_eq!(
            filtered.len(),
            1,
            "Should find exactly 1 entry matching 'token'"
        );
        assert_eq!(filtered[0].label, "Max Tokens");
    }

    #[test]
    fn toggle_bool_entry_flips_value() {
        // Guarded: `open()` loads the settings file, and without this the row
        // reflects whatever the machine running the test happens to have set.
        let _guard = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.open();
        screen.notifications = true;

        let initial = screen.notifications;
        let all = all_entries(&screen);
        // By key, not by index: a row inserted anywhere above used to move
        // this one and fail a test that has nothing to do with the new row.
        let entry = all
            .iter()
            .find(|e| e.key == "notifications")
            .expect("the notifications row is missing");
        assert_eq!(entry.label, "Desktop notifications");
        assert_eq!(entry.value, "true");

        // Simulate toggle (manually, since toggle_or_cycle_current modifies internal state)
        screen.notifications = !screen.notifications;
        assert_ne!(screen.notifications, initial);
    }

    /// The query loop reads `config.autoMemoryEnabled`, so a toggle that
    /// writes only the top-level key saves somewhere the session never looks.
    #[test]
    fn toggling_auto_memory_writes_both_keys() {
        let _guard = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.open();
        let mut config = Config::default();

        let index = all_entries(&screen)
            .iter()
            .position(|e| e.key == "auto_memory")
            .expect("the auto memory row is missing");
        screen.selected_idx = index;

        assert!(!screen.auto_memory, "the row does not start off");
        toggle_or_cycle_current(&mut screen, &mut config);

        assert!(screen.auto_memory);
        assert_eq!(screen.settings_snapshot.auto_memory_enabled, Some(true));
        assert_eq!(
            screen.settings_snapshot.config.auto_memory_enabled,
            Some(true),
            "the nested key the query loop reads stayed unset"
        );
        assert_eq!(screen.save_error, None);
    }

    /// Two rows, two keys, and each writes the nested key the loader reads.
    #[test]
    fn the_two_memory_filename_rows_toggle_independently() {
        let _guard = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.open();
        let mut config = Config::default();

        let index_of = |screen: &SettingsScreen, key: &str| {
            all_entries(screen)
                .iter()
                .position(|e| e.key == key)
                .unwrap_or_else(|| panic!("the {key} row is missing"))
        };

        // Today's behaviour: AGENTS.md on, CLAUDE.md off.
        assert!(screen.agents_md);
        assert!(!screen.claude_md);

        screen.selected_idx = index_of(&screen, "claude_md");
        toggle_or_cycle_current(&mut screen, &mut config);
        assert!(screen.claude_md);
        assert_eq!(screen.settings_snapshot.claude_md_enabled, Some(true));
        assert_eq!(
            screen.settings_snapshot.config.claude_md_enabled,
            Some(true),
            "the nested key the loader reads stayed unset"
        );
        assert!(screen.agents_md, "one row must not move the other");

        screen.selected_idx = index_of(&screen, "agents_md");
        toggle_or_cycle_current(&mut screen, &mut config);
        assert!(!screen.agents_md);
        assert_eq!(screen.settings_snapshot.agents_md_enabled, Some(false));
        assert_eq!(
            screen.settings_snapshot.config.agents_md_enabled,
            Some(false)
        );
        assert!(screen.claude_md, "the other row stayed where it was");
        assert_eq!(screen.save_error, None);
    }

    /// The sound is a sub-setting of the notification, so it sits with the
    /// three events rather than at the far end of the list.
    #[test]
    fn the_notification_sound_row_follows_the_three_events() {
        let screen = SettingsScreen::new();
        let all = all_entries(&screen);
        let keys: Vec<&str> = all.iter().map(|e| e.key.as_str()).collect();

        let sound = keys
            .iter()
            .position(|key| *key == "notify_sound")
            .expect("the notification sound row is missing");
        let turn_complete = keys
            .iter()
            .position(|key| *key == "notify_on_turn_complete")
            .expect("the turn-complete row is missing");

        assert_eq!(sound, turn_complete + 1);
        assert_eq!(all[sound].label, "Notification sound");
        // Opt-in, so a fresh screen shows it off.
        assert_eq!(all[sound].value, "false");
    }

    /// Through `toggle_or_cycle_current` rather than by assigning the field:
    /// a key missing from that match arm reads as a working row and silently
    /// changes nothing.
    #[test]
    fn toggling_the_sound_row_writes_the_screen_and_the_snapshot() {
        let _guard = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.open();
        let mut config = Config::default();

        let index = all_entries(&screen)
            .iter()
            .position(|e| e.key == "notify_sound")
            .expect("the notification sound row is missing");
        screen.selected_idx = index;

        assert!(!screen.notify_sound);
        toggle_or_cycle_current(&mut screen, &mut config);

        assert!(screen.notify_sound, "the screen's own copy stayed off");
        assert!(
            screen.settings_snapshot.notify_sound,
            "the snapshot written to disk stayed off"
        );
        assert_eq!(screen.save_error, None);

        toggle_or_cycle_current(&mut screen, &mut config);
        assert!(!screen.notify_sound, "the row does not toggle back off");
        assert!(!screen.settings_snapshot.notify_sound);
    }

    #[test]
    fn cycle_enum_entry_wraps_around() {
        let mut screen = SettingsScreen::new();
        screen.output_style = "default".to_string();

        // Simulate cycling through all options
        let options = ["default", "concise", "explanatory", "learning"];
        let mut idx = options.iter().position(|&o| o == "default").unwrap();

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "concise");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "explanatory");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "learning");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "default"); // Wraps around
    }

    #[test]
    fn typing_a_search_scrolls_back_to_the_first_match() {
        // The filtered list is shorter than wherever the user had scrolled to,
        // so an offset left behind skips every row and the pane renders empty.
        let mut screen = SettingsScreen::new();
        screen.scroll_offset = 12;
        screen.selected_idx = 12;

        screen.push_search_char('i');

        assert_eq!(screen.selected_idx, 0);
        assert_eq!(screen.scroll_offset, 0);
    }

    #[test]
    fn clearing_a_search_character_scrolls_back_too() {
        let mut screen = SettingsScreen::new();
        screen.push_search_char('i');
        screen.push_search_char('g');
        screen.scroll_offset = 9;
        screen.selected_idx = 9;

        screen.pop_search_char();

        assert_eq!(screen.selected_idx, 0);
        assert_eq!(screen.scroll_offset, 0);
    }

    #[test]
    fn a_search_matches_a_label_case_insensitively() {
        let mut screen = SettingsScreen::new();
        screen.search_query = "ignored".to_string();
        let matches: Vec<_> = all_entries(&screen)
            .into_iter()
            .filter(|e| {
                e.label
                    .to_lowercase()
                    .contains(&screen.search_query.to_lowercase())
            })
            .collect();

        assert_eq!(matches.len(), 1, "expected one match, got {matches:?}");
        assert_eq!(matches[0].key, "include_ignored_files");
    }

    /// The rows the list actually draws, in order, for the current search.
    fn visible_entries(screen: &SettingsScreen) -> Vec<SettingsEntry> {
        all_entries(screen)
            .into_iter()
            .filter(|e| {
                e.label
                    .to_lowercase()
                    .contains(&screen.search_query.to_lowercase())
            })
            .collect()
    }

    fn entry_value(screen: &SettingsScreen, key: &str) -> String {
        all_entries(screen)
            .into_iter()
            .find(|e| e.key == key)
            .unwrap_or_else(|| panic!("no {key} entry"))
            .value
    }

    #[test]
    fn the_timeline_toggle_is_listed_and_reaches_the_config() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        let entry = all_entries(&screen)
            .into_iter()
            .find(|e| e.key == "timeline_enabled")
            .expect("timeline entry");
        assert_eq!(entry.label, "Execution timeline");
        assert!(matches!(entry.kind, SettingKind::Bool));

        screen.search_query = "Execution timeline".to_string();
        screen.selected_idx = 0;
        let mut config = Config::default();
        toggle_or_cycle_current(&mut screen, &mut config);

        assert!(screen.timeline_enabled);
        assert!(screen.settings_snapshot.config.timeline_enabled);
        assert!(
            config.timeline_enabled,
            "the running session reads this config, so a toggle that misses it \
             looks applied but does nothing until the next launch"
        );
        assert_eq!(screen.save_error, None);
    }

    /// Every boolean that lives in `Config` has to reach the running session,
    /// not just the file. These are the ones the session reads back.
    #[test]
    fn a_config_backed_toggle_reaches_the_running_session() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        /// A label to search for, and the config field its toggle must reach.
        type ToggleCase = (&'static str, fn(&Config) -> bool);

        let cases: Vec<ToggleCase> = vec![
            ("Verbose logging", |config| config.verbose),
            ("Search ignored files", |config| {
                config.include_ignored_files.unwrap_or(false)
            }),
            ("Web search fallback", |config| config.web_search_fallback),
            ("Execution timeline", |config| config.timeline_enabled),
            ("Live tool output", |config| config.live_tool_output),
            ("File injection (@)", |config| {
                config.file_injection_enabled.is_some()
            }),
        ];

        for (label, reads) in cases {
            let mut screen = SettingsScreen::new();
            screen.search_query = label.to_string();
            screen.selected_idx = 0;
            let mut config = Config::default();
            toggle_or_cycle_current(&mut screen, &mut config);

            assert!(
                reads(&config),
                "toggling {label:?} never reached the live config"
            );
        }
    }

    #[test]
    fn web_search_fallback_is_listed_as_a_toggle() {
        let screen = SettingsScreen::new();
        let entry = all_entries(&screen)
            .into_iter()
            .find(|e| e.key == "web_search_fallback")
            .expect("web search fallback entry");

        assert_eq!(entry.label, "Web search fallback");
        assert!(matches!(entry.kind, SettingKind::Bool));
    }

    /// `MIKMIK_HOME` is process-global and `toggle_or_cycle_current` saves, so
    /// the toggle test needs the config root to itself.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct HomeGuard {
        saved: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl HomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let saved = std::env::var_os("MIKMIK_HOME");
            std::env::set_var("MIKMIK_HOME", dir.path());
            Self { saved, _dir: dir }
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

    /// `save_sync` refuses to overwrite a settings file it cannot parse, which
    /// is the failure the screen used to discard.
    #[test]
    fn a_refused_save_is_reported_instead_of_discarded() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();
        let path = mikmik_core::mikmik_home().join("settings.json");
        std::fs::create_dir_all(mikmik_core::mikmik_home()).expect("mkdir");
        std::fs::write(&path, r#"{"config":{"model":"x",}}"#).expect("write");

        let mut screen = SettingsScreen::new();
        screen.search_query = "Web search fallback".to_string();
        screen.selected_idx = 0;

        toggle_or_cycle_current(&mut screen, &mut Config::default());

        let error = screen
            .save_error
            .expect("the refused write must be reported");
        assert!(error.contains("settings"), "unexpected message: {error}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            r#"{"config":{"model":"x",}}"#,
            "the malformed file must be left alone"
        );
    }

    #[test]
    fn a_save_that_lands_leaves_no_error_behind() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.save_error = Some("stale".to_string());
        screen.search_query = "Web search fallback".to_string();
        screen.selected_idx = 0;

        toggle_or_cycle_current(&mut screen, &mut Config::default());

        assert_eq!(screen.save_error, None);
    }

    #[test]
    fn an_edit_keeps_a_toggle_made_earlier_on_the_same_screen() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.search_query = "Web search fallback".to_string();
        toggle_or_cycle_current(&mut screen, &mut Config::default());
        assert!(screen.settings_snapshot.config.web_search_fallback);

        // The caller's live config knows nothing about that toggle, which is
        // what used to overwrite it.
        let mut config = Config::default();
        screen.start_edit("max_tokens", "1000");
        screen.commit_edit();
        screen.apply_and_save(&mut config);

        assert!(screen.settings_snapshot.config.web_search_fallback);
        assert_eq!(screen.settings_snapshot.config.max_tokens, Some(1000));
        assert_eq!(config.max_tokens, Some(1000));
    }

    #[test]
    fn enabling_searxng_asks_for_the_address() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.search_query = "SearXNG".to_string();
        screen.selected_idx = 0;

        toggle_or_cycle_current(&mut screen, &mut Config::default());

        assert_eq!(screen.edit_field.as_deref(), Some("searxng_url"));
        assert_eq!(screen.edit_value, DEFAULT_SEARXNG_URL);
        assert_eq!(
            screen.settings_snapshot.config.searxng_url, None,
            "nothing is stored until the address is confirmed"
        );

        // The edit buffer only renders on the row it belongs to.
        let visible = visible_entries(&screen);
        assert_eq!(
            visible.get(screen.selected_idx).map(|e| e.key.as_str()),
            Some("searxng_url"),
            "the cursor must sit on the address row or the prompt is invisible"
        );
    }

    #[test]
    fn confirming_the_address_writes_it_to_the_file() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();
        let mut config = Config::default();

        let mut screen = SettingsScreen::new();
        screen.search_query = "SearXNG".to_string();
        screen.selected_idx = 0;
        toggle_or_cycle_current(&mut screen, &mut Config::default());
        screen.edit_value = "  http://searx.lan:9000  ".to_string();
        screen.commit_edit();
        screen.apply_and_save(&mut config);

        assert_eq!(screen.save_error, None);
        assert_eq!(screen.searxng_url, "http://searx.lan:9000");
        assert_eq!(config.searxng_url.as_deref(), Some("http://searx.lan:9000"));

        let written = std::fs::read_to_string(mikmik_core::mikmik_home().join("settings.json"))
            .expect("settings.json");
        assert!(
            written.contains("\"searxngUrl\": \"http://searx.lan:9000\""),
            "address missing from the file: {written}"
        );
    }

    #[test]
    fn turning_searxng_off_clears_the_address() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.searxng_url = "http://searx.lan:9000".to_string();
        screen.settings_snapshot.config.searxng_url = Some("http://searx.lan:9000".to_string());
        screen.search_query = "SearXNG".to_string();
        screen.selected_idx = 0;

        toggle_or_cycle_current(&mut screen, &mut Config::default());

        assert!(screen.edit_field.is_none());
        assert!(screen.searxng_url.is_empty());
        assert_eq!(screen.settings_snapshot.config.searxng_url, None);
    }

    #[test]
    fn an_empty_address_turns_searxng_off() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();
        let mut config = Config {
            searxng_url: Some("http://searx.lan:9000".to_string()),
            ..Default::default()
        };

        let mut screen = SettingsScreen::new();
        screen.start_edit("searxng_url", "http://searx.lan:9000");
        screen.edit_value = "   ".to_string();
        screen.commit_edit();
        screen.apply_and_save(&mut config);

        assert_eq!(config.searxng_url, None);
        assert!(screen.searxng_url.is_empty());
    }

    #[test]
    fn toggling_web_search_fallback_reaches_the_config() {
        let _lock = match HOME_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        let mut screen = SettingsScreen::new();
        screen.search_query = "Web search fallback".to_string();
        screen.selected_idx = 0;

        toggle_or_cycle_current(&mut screen, &mut Config::default());

        assert!(screen.web_search_fallback);
        assert!(screen.settings_snapshot.config.web_search_fallback);

        toggle_or_cycle_current(&mut screen, &mut Config::default());

        assert!(!screen.web_search_fallback);
        assert!(!screen.settings_snapshot.config.web_search_fallback);
    }

    #[test]
    fn web_search_fallback_renders_the_state_it_holds() {
        // Built by hand rather than through `new()`, which reads the machine's
        // own settings.json and would make the expected value environmental.
        let mut screen = SettingsScreen::new();

        screen.web_search_fallback = false;
        assert_eq!(entry_value(&screen, "web_search_fallback"), "false");

        screen.web_search_fallback = true;
        assert_eq!(entry_value(&screen, "web_search_fallback"), "true");
    }

    // ---- the compact model row ----------------------------------------------

    #[test]
    fn the_compact_model_row_asks_for_the_picker_rather_than_an_edit_box() {
        let mut screen = SettingsScreen::new();
        let entries = all_entries(&screen);
        let entry = entries
            .iter()
            .find(|e| e.key == "compact_model")
            .expect("the compact model row exists");
        assert!(matches!(entry.kind, SettingKind::ModelPicker));
        assert_eq!(entry.value, USE_THE_TURNS_MODEL, "unset by default");

        screen.selected_idx = entries
            .iter()
            .position(|e| e.key == "compact_model")
            .expect("its index");
        toggle_or_cycle_current(&mut screen, &mut Config::default());

        assert!(
            screen.edit_field.is_none(),
            "the row must not open an edit box: the model list belongs to the accounts"
        );
        assert_eq!(
            screen.take_pending_model_picker().as_deref(),
            Some("compact_model")
        );
        assert!(
            screen.take_pending_model_picker().is_none(),
            "taking it twice would reopen the picker on the next frame"
        );
    }

    #[test]
    fn a_picked_compact_model_reaches_the_running_session() {
        let _guard = HomeGuard::new();
        let mut screen = SettingsScreen::new();
        let mut config = Config::default();

        screen.set_picked_model(
            "compact_model",
            Some("cheap_account/haiku".to_string()),
            &mut config,
        );

        assert_eq!(screen.compact_model, "cheap_account/haiku");
        assert_eq!(
            screen.settings_snapshot.config.compact_model.as_deref(),
            Some("cheap_account/haiku")
        );
        assert_eq!(
            config.compact_model.as_deref(),
            Some("cheap_account/haiku"),
            "the running session reads the live config, not the snapshot"
        );
        assert!(screen.save_error.is_none(), "{:?}", screen.save_error);
    }

    #[test]
    fn clearing_the_compact_model_sends_the_summary_back_to_the_turn() {
        let _guard = HomeGuard::new();
        let mut screen = SettingsScreen::new();
        let mut config = Config {
            compact_model: Some("cheap_account/haiku".to_string()),
            ..Default::default()
        };
        screen.compact_model = "cheap_account/haiku".to_string();

        screen.set_picked_model("compact_model", None, &mut config);

        assert!(screen.compact_model.is_empty());
        assert_eq!(screen.settings_snapshot.config.compact_model, None);
        assert_eq!(config.compact_model, None);
    }
}
