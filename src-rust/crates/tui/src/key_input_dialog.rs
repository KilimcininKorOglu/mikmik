// key_input_dialog.rs — Masked text input overlay for entering API keys.
//
// Provides a modal dialog that collects an API key from the user with
// masked display (showing only the last 4 characters).

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::overlays::{centered_rect, render_dark_overlay, render_dialog_bg, MIKMIK_PANEL_BG};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Which field the dialog is typing into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInputField {
    Account,
    Key,
}

/// State for the API key input dialog.
pub struct KeyInputDialogState {
    pub visible: bool,
    /// Wire format the credential belongs to, taken from the connect picker.
    pub provider_id: String,
    pub provider_name: String,
    /// Name this credential is stored and addressed under.
    ///
    /// Separate from `provider_id` so the same vendor can hold more than one
    /// account: a work key and a personal key are two accounts speaking one
    /// protocol, and keying by vendor let the second overwrite the first.
    pub account_input: String,
    pub input: String,
    pub cursor_pos: usize,
    pub active_field: KeyInputField,
    /// True when the dialog enters a web-search backend key. The credential is
    /// then stored under the fixed provider id (the search chain reads it by
    /// that id), so the account field is locked and hidden and only the key is
    /// typed.
    pub web_search_key: bool,
}

impl Default for KeyInputDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyInputDialogState {
    pub fn new() -> Self {
        Self {
            visible: false,
            provider_id: String::new(),
            provider_name: String::new(),
            account_input: String::new(),
            input: String::new(),
            cursor_pos: 0,
            active_field: KeyInputField::Account,
            web_search_key: false,
        }
    }

    /// Open the dialog for a specific provider.
    pub fn open(&mut self, provider_id: String, provider_name: String) {
        self.visible = true;
        // Default the account to the vendor's own name, which is what the
        // credential was keyed by before accounts could be named.
        self.account_input = provider_id.clone();
        self.provider_id = provider_id;
        self.provider_name = provider_name;
        self.input.clear();
        self.cursor_pos = 0;
        self.active_field = KeyInputField::Account;
        self.web_search_key = false;
    }

    /// Open the dialog for a web-search backend, whose key is stored under the
    /// fixed provider id. The account field is locked to that id and hidden,
    /// so the user types only the key.
    pub fn open_web_search(&mut self, provider_id: String, provider_name: String) {
        self.open(provider_id, provider_name);
        self.web_search_key = true;
        self.active_field = KeyInputField::Key;
    }

    /// Close and clear the dialog.
    pub fn close(&mut self) {
        self.visible = false;
        self.account_input.clear();
        self.input.clear();
        self.cursor_pos = 0;
        self.active_field = KeyInputField::Account;
        self.web_search_key = false;
    }

    /// Move to the other field. A web-search key has only the key field, so
    /// the focus stays put.
    pub fn toggle_field(&mut self) {
        if self.web_search_key {
            self.active_field = KeyInputField::Key;
            return;
        }
        self.active_field = match self.active_field {
            KeyInputField::Account => KeyInputField::Key,
            KeyInputField::Key => KeyInputField::Account,
        };
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        match self.active_field {
            KeyInputField::Account => self.account_input.push(c),
            KeyInputField::Key => {
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
            }
        }
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        match self.active_field {
            KeyInputField::Account => {
                self.account_input.pop();
            }
            KeyInputField::Key => {
                if self.cursor_pos > 0 {
                    // Find the previous char boundary
                    let prev = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(prev);
                    self.cursor_pos = prev;
                }
            }
        }
    }

    /// Whether the typed account name can be stored and addressed.
    pub fn account_name_is_valid(&self) -> bool {
        mikmik_core::config::account_name_is_valid(&self.account_input)
    }

    /// Whether the dialog holds enough to save an account.
    pub fn can_submit(&self) -> bool {
        !self.input.trim().is_empty() && self.account_name_is_valid()
    }

    /// Take the entered account name and key, then close the dialog.
    ///
    /// Returns `(account, protocol, key)`.
    pub fn take_key(&mut self) -> (String, String, String) {
        let account = self.account_input.trim().to_string();
        let protocol = self.provider_id.clone();
        let key = self.input.clone();
        self.close();
        (account, protocol, key)
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the key input dialog overlay — OpenCode-style: dark overlay, no
/// border, minimal and polished.
pub fn render_key_input_dialog(frame: &mut Frame, state: &KeyInputDialogState, area: Rect) {
    if !state.visible {
        return;
    }

    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let dialog_bg = MIKMIK_PANEL_BG;

    // ── Darken the entire background ──
    render_dark_overlay(frame, area);

    // ── Dialog size ──
    let width = 60u16.min(area.width.saturating_sub(4));
    // Grew by three rows when the account field was added; a web-search key
    // hides that field and shrinks back.
    let height = if state.web_search_key { 9u16 } else { 12u16 };
    let dialog_area = centered_rect(width, height, area);

    // ── Fill dialog background (no border) ──
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    // ── Build lines ──
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Title row: "Connect {provider}" on left, "esc" on right
    let title_text = format!("Connect {}", state.provider_name);
    let title_pad = inner.width.saturating_sub(title_text.len() as u16 + 5) as usize;
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", title_text),
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>width$}", "esc ", width = title_pad),
            Style::default().fg(dim),
        ),
    ]));

    // Blank line
    lines.push(Line::from(""));

    // "Account name:" label and field — hidden for a web-search key, whose
    // credential is stored under the fixed provider id.
    if !state.web_search_key {
        lines.push(Line::from(vec![
            Span::styled(
                " Account name:",
                Style::default().fg(Color::Rgb(180, 180, 180)),
            ),
            Span::styled(
                format!("  (speaks {})", state.provider_id),
                Style::default().fg(dim),
            ),
        ]));
        let account_text = if state.account_input.is_empty() {
            "name this account...".to_string()
        } else {
            state.account_input.clone()
        };
        let account_style = if state.account_input.is_empty() {
            Style::default().fg(dim)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {}", account_text), account_style),
            Span::styled(
                if state.active_field == KeyInputField::Account {
                    "_"
                } else {
                    ""
                },
                Style::default().fg(pink),
            ),
        ]));

        // Blank line
        lines.push(Line::from(""));
    }

    // "API Key:" label
    lines.push(Line::from(vec![Span::styled(
        " API Key:",
        Style::default().fg(Color::Rgb(180, 180, 180)),
    )]));

    // Masked key display (show last 4 chars, mask the rest)
    let masked = if state.input.is_empty() {
        "paste your API key here...".to_string()
    } else {
        let len = state.input.len();
        if len <= 4 {
            state.input.clone()
        } else {
            format!("{}{}", "\u{2022}".repeat(len - 4), &state.input[len - 4..])
        }
    };

    let input_style = if state.input.is_empty() {
        Style::default().fg(dim)
    } else {
        Style::default().fg(Color::White)
    };

    lines.push(Line::from(vec![
        Span::styled(format!(" {}", masked), input_style),
        Span::styled(
            if state.active_field == KeyInputField::Key {
                "_"
            } else {
                ""
            },
            Style::default().fg(pink),
        ),
    ]));

    // Blank line
    lines.push(Line::from(""));

    // Hint row
    let confirm_hint = if state.can_submit() {
        " confirm"
    } else if !state.account_name_is_valid() {
        " name: no spaces or /"
    } else {
        " paste a key"
    };
    if state.web_search_key {
        lines.push(Line::from(vec![
            Span::styled(" enter", Style::default().fg(dim)),
            Span::styled(confirm_hint, Style::default().fg(dim)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(" tab", Style::default().fg(dim)),
            Span::styled(" switch field   enter", Style::default().fg(dim)),
            Span::styled(confirm_hint, Style::default().fg(dim)),
        ]));
    }

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn opened() -> KeyInputDialogState {
        let mut state = KeyInputDialogState::new();
        state.open("openai".to_string(), "OpenAI".to_string());
        state
    }

    fn type_into(state: &mut KeyInputDialogState, text: &str) {
        for c in text.chars() {
            state.insert_char(c);
        }
    }

    #[test]
    fn the_account_defaults_to_the_vendor_name() {
        // Confirming straight through reproduces the behaviour from before
        // accounts could be named, where the key was stored under the vendor.
        let state = opened();
        assert_eq!(state.account_input, "openai");
        assert_eq!(state.active_field, KeyInputField::Account);
    }

    #[test]
    fn tab_moves_between_the_two_fields() {
        let mut state = opened();
        state.toggle_field();
        assert_eq!(state.active_field, KeyInputField::Key);
        state.toggle_field();
        assert_eq!(state.active_field, KeyInputField::Account);
    }

    #[test]
    fn typing_lands_in_the_active_field() {
        let mut state = opened();
        state.account_input.clear();
        type_into(&mut state, "work_openai");
        state.toggle_field();
        type_into(&mut state, "sk-test");

        assert_eq!(state.account_input, "work_openai");
        assert_eq!(state.input, "sk-test");
    }

    #[test]
    fn a_slash_in_the_account_name_is_refused() {
        let mut state = opened();
        state.account_input.clear();
        type_into(&mut state, "work/openai");
        state.toggle_field();
        type_into(&mut state, "sk-test");
        assert!(!state.can_submit());
    }

    #[test]
    fn a_key_is_still_required() {
        let state = opened();
        assert!(!state.can_submit(), "an account with no key saves nothing");
    }

    #[test]
    fn submitting_separates_the_account_from_the_protocol() {
        let mut state = opened();
        state.account_input.clear();
        type_into(&mut state, "work_openai");
        state.toggle_field();
        type_into(&mut state, "sk-test");

        assert!(state.can_submit());
        let (account, protocol, key) = state.take_key();
        assert_eq!(account, "work_openai");
        assert_eq!(protocol, "openai", "the wire format is separate");
        assert_eq!(key, "sk-test");
        assert!(!state.visible);
    }

    #[test]
    fn a_web_search_key_locks_the_account_to_the_provider_id() {
        let mut state = KeyInputDialogState::new();
        state.open_web_search("tavily".to_string(), "Tavily".to_string());
        // The account is fixed to the id and focus starts on the key.
        assert_eq!(state.account_input, "tavily");
        assert_eq!(state.active_field, KeyInputField::Key);
        // Tab cannot move off the key field.
        state.toggle_field();
        assert_eq!(state.active_field, KeyInputField::Key);
        // Only a key is needed; the fixed id is a valid account name.
        type_into(&mut state, "tvly-abc");
        assert!(state.can_submit());
        let (account, protocol, key) = state.take_key();
        assert_eq!(account, "tavily");
        assert_eq!(protocol, "tavily");
        assert_eq!(key, "tvly-abc");
    }

    #[test]
    fn a_web_search_dialog_hides_the_account_field() {
        let mut state = KeyInputDialogState::new();
        state.open_web_search("exa".to_string(), "Exa".to_string());
        type_into(&mut state, "exa-key-9999");
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).expect("terminal");
        terminal
            .draw(|frame| render_key_input_dialog(frame, &state, frame.area()))
            .expect("draw");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Connect Exa"), "title missing");
        assert!(rendered.contains("API Key:"), "key field missing");
        assert!(!rendered.contains("Account name:"), "account field hidden");
        assert!(rendered.contains("9999"), "the last four are shown");
    }

    #[test]
    fn the_dialog_shows_both_fields_and_masks_the_key() {
        let mut state = opened();
        state.account_input.clear();
        type_into(&mut state, "work_openai");
        state.toggle_field();
        type_into(&mut state, "sk-secret-value-1234");

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).expect("terminal");
        terminal
            .draw(|frame| render_key_input_dialog(frame, &state, frame.area()))
            .expect("draw");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("Account name:"), "account field missing");
        assert!(rendered.contains("work_openai"), "typed name missing");
        assert!(rendered.contains("API Key:"), "key field missing");
        assert!(rendered.contains("1234"), "the last four are shown");
        assert!(
            !rendered.contains("sk-secret-value"),
            "the key must not be rendered in the clear"
        );
    }
}
