// render.rs — All ratatui rendering logic.

use std::cell::RefCell;

use crate::agents_view::render_agents_menu;
use crate::app::{App, ContextMenuKind, SystemAnnotation, SystemMessageStyle, ToolStatus};
use crate::ask_user_dialog::render_ask_user_dialog;
use crate::bypass_permissions_dialog::render_bypass_permissions_dialog;
use crate::context_viz::render_context_viz;
use crate::custom_provider_dialog::render_custom_provider_dialog;
use crate::desktop_upsell_startup::render_desktop_upsell_startup;
use crate::device_auth_dialog::render_device_auth_dialog;
use crate::dialog_select::render_dialog_select;
use crate::dialogs::{
    render_mcp_approval_dialog, render_permission_dialog, render_project_trust_dialog,
};
use crate::diff_viewer::render_diff_dialog;
use crate::elicitation_dialog::render_elicitation_dialog;
use crate::export_dialog::render_export_dialog;
use crate::feedback_survey::render_feedback_survey;
use crate::figures;
use crate::file_injection_dialog::render_file_injection_dialog;
use crate::hooks_config_menu::render_hooks_config_menu;
use crate::import_config_dialog::render_import_config_dialog;
use crate::invalid_config_dialog::render_invalid_config_dialog;
use crate::key_input_dialog::render_key_input_dialog;
use crate::mcp_view::render_mcp_view;
use crate::memory_file_selector::render_memory_file_selector;
use crate::memory_update_notification::render_memory_update_notification;
use crate::messages::{
    render_thinking_live_content, render_transcript_assistant_message_tagged,
    render_transcript_assistant_meta, render_transcript_live_text, render_transcript_user_message,
    RenderContext,
};
use crate::mikmik::{mikmik_lines, MIKMIK_NAME, MIKMIK_WIDTH};
use crate::model_picker::render_model_picker;
use crate::notifications::{render_notification_banner, Notification, NotificationKind};
use crate::onboarding_dialog::render_onboarding_dialog;
use crate::overage_upsell::render_overage_upsell;
use crate::overlays::{
    render_global_search, render_help_overlay, render_history_search_overlay, render_rewind_flow,
    MIKMIK_ACCENT,
};
use crate::plugin_views::render_plugin_hints;
use crate::prompt_input::{input_height, render_prompt_input, InputMode, TypeaheadSource, VimMode};
use crate::session_branching::render_session_branching;
use crate::session_browser::render_session_browser;
use crate::settings_screen::render_settings_screen;
use crate::stats_dialog::render_stats_dialog;
use crate::tasks_overlay::render_tasks_overlay;
use crate::theme_screen::render_theme_screen;
use crate::transcript_turn::{build_transcript_turns, TranscriptTurn};
use crate::virtual_list::{VirtualItem, VirtualList};
use crate::voice_mode_notice::render_voice_mode_notice;
use mikmik_core::constants::{APP_VERSION, CONTEXT_CRITICAL_FRACTION, CONTEXT_WARNING_FRACTION};
use mikmik_core::timeline::{TimelineRow, TimelineStatus};
use mikmik_core::types::Role;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

// Spinner frames matching the TypeScript SpinnerGlyph: platform-specific base
// characters mirrored (forward + reverse) for a smooth pulse effect.
// Windows uses '*' instead of '✳'/'✽' for better font coverage.
#[cfg(target_os = "windows")]
const SPINNER: &[char] = &[
    '\u{00b7}', '\u{2722}', '*', '\u{2736}', '\u{273b}', '\u{273d}', '\u{273d}', '\u{273b}',
    '\u{2736}', '*', '\u{2722}', '\u{00b7}',
];
#[cfg(not(target_os = "windows"))]
const SPINNER: &[char] = &[
    '\u{00b7}', '\u{2722}', '\u{2733}', '\u{2736}', '\u{273b}', '\u{273d}', '\u{273d}', '\u{273b}',
    '\u{2736}', '\u{2733}', '\u{2722}', '\u{00b7}',
];
const CLAUDE_ORANGE: Color = Color::Rgb(233, 30, 99);
const WELCOME_BOX_HEIGHT: u16 = 9;
/// How many lines of a running command's output stay on screen.
///
/// A tail, because a build prints thousands of lines and the block would push
/// the conversation off the screen while it ran.
const LIVE_OUTPUT_TAIL_LINES: usize = 10;
/// The rule drawn between a tool's header line and what the tool printed.
///
/// A fixed width rather than the pane's: the transcript is built into
/// `Line`s before a width is known, and a rule that reached the edge would
/// dominate a block whose output is one short line.
const TOOL_OUTPUT_RULE: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";
/// How long a one-line result may be and still sit on the header line.
///
/// The header already carries the icon and what was asked, so a longer answer
/// would wrap and cost the line it saved.
const FOLDED_RESULT_LIMIT: usize = 60;
const STATUS_THINKING: &str = "thinking";
const STATUS_THINKING_ELLIPSIS: &str = "thinking\u{2026}";

fn spinner_char(frame_count: u64) -> char {
    SPINNER[(frame_count as usize) % SPINNER.len()]
}

/// Returns the colour to use for the streaming spinner: mikmik red normally,
/// brightening to a hot red when no stream data has arrived for over 3 seconds.
fn spinner_color(app: &App) -> Color {
    if let Some(start) = app.stall_start {
        if start.elapsed() > std::time::Duration::from_secs(3) {
            return Color::Rgb(255, 70, 70);
        }
    }
    CLAUDE_ORANGE
}

fn is_modal_open(app: &App) -> bool {
    app.any_modal_open()
}

// -----------------------------------------------------------------------
// Error modal rendering
// -----------------------------------------------------------------------

/// Render an error modal dialog with wrapped content.
fn render_error_modal(
    frame: &mut Frame,
    area: Rect,
    notification: &Notification,
    _scroll_offset: usize,
    footer_area: Rect,
    is_welcome_screen: bool,
) {
    // When the footer anchor is inside the welcome box (y < WELCOME_BOX_HEIGHT), or explicitly on
    // the welcome screen, center the modal so it doesn't awkwardly overlap the welcome box.
    let anchored_in_welcome_box = footer_area.width > 0 && footer_area.y < WELCOME_BOX_HEIGHT;
    let modal_area = if is_welcome_screen || anchored_in_welcome_box {
        let modal_width = (area.width * 2 / 3).max(40).min(area.width);
        let modal_height = (area.height / 3).max(8).min(area.height.saturating_sub(2));
        Rect {
            x: area.x + (area.width.saturating_sub(modal_width)) / 2,
            y: area.y + (area.height.saturating_sub(modal_height)) / 2,
            width: modal_width,
            height: modal_height,
        }
    } else if footer_area.width > 0 {
        let desired_height = (area.height / 3)
            .max(8)
            .min(area.height.saturating_sub(footer_area.y));
        Rect {
            x: footer_area.x,
            y: footer_area.y,
            width: footer_area.width,
            height: desired_height,
        }
    } else {
        let modal_width = area.width / 2;
        let modal_height = area.height.saturating_sub(4);
        Rect {
            x: area.x + modal_width,
            y: area.y,
            width: modal_width,
            height: modal_height,
        }
    };

    frame.render_widget(Clear, modal_area);

    let modal_block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .style(Style::default().fg(Color::Red));
    frame.render_widget(modal_block, modal_area);

    let header_bg_area = Rect {
        x: modal_area.x + 1,
        y: modal_area.y + 1,
        width: modal_area.width.saturating_sub(2),
        height: 1,
    };
    let header_style = Style::default().bg(Color::Rgb(60, 15, 15)).fg(Color::Red);
    let header_para =
        Paragraph::new("  ⚠ Error  ").style(header_style.add_modifier(Modifier::BOLD));
    frame.render_widget(header_para, header_bg_area);

    let sep_area = Rect {
        x: modal_area.x + 1,
        y: modal_area.y + 2,
        width: modal_area.width.saturating_sub(2),
        height: 1,
    };
    let sep_line = Paragraph::new(Line::from(Span::styled(
        "─".repeat(sep_area.width as usize),
        Style::default().fg(Color::Rgb(80, 20, 20)),
    )));
    frame.render_widget(sep_line, sep_area);

    // Chrome: border(1) + header(1) + sep(1) + blank(1) + border(1) = 5 rows
    let body_start_y = modal_area.y + 4;
    let body_height = modal_area.height.saturating_sub(5).max(1);
    let body_area = Rect {
        x: modal_area.x + 2,
        y: body_start_y,
        width: modal_area.width.saturating_sub(4),
        height: body_height,
    };

    let body_para = Paragraph::new(notification.message.as_str())
        .style(Style::default().fg(Color::Rgb(220, 220, 220)))
        .wrap(Wrap { trim: true });
    frame.render_widget(body_para, body_area);
}

// -----------------------------------------------------------------------
// Text truncation helpers
// -----------------------------------------------------------------------

/// Relative timestamp for the welcome screen's recent-activity list.
///
/// Clock skew (mtime in the future) reads as "just now": an mtime ahead of the
/// clock has no elapsed time to report, and `format_relative_time` already
/// answers that for a timestamp it cannot look back on.
fn relative_mtime(mtime: std::time::SystemTime) -> String {
    let ms = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    mikmik_core::format_utils::format_relative_time(ms)
}

/// Build the body lines for the welcome box's "Recent activity" section.
///
/// Renders up to five recent sessions as `<label> <relative-time>` (the label
/// truncated to fit `width`), or a single dimmed "No recent activity" line when
/// there are none. Split out from [`render_welcome_box`] so it can be unit
/// tested from controlled state without the surrounding layout.
fn recent_activity_lines(recent: &[crate::app::RecentSession], width: usize) -> Vec<Line<'static>> {
    if recent.is_empty() {
        return vec![Line::from(Span::styled(
            "No recent activity",
            Style::default().fg(Color::DarkGray),
        ))];
    }

    recent
        .iter()
        .take(5)
        .map(|s| {
            let when = relative_mtime(s.mtime);
            // Reserve room for the trailing " <time>" so the label truncates
            // instead of wrapping onto a second line.
            let label_w = width.saturating_sub(when.chars().count() + 1);
            let label = truncate_end(&s.label, label_w.max(1));
            Line::from(vec![
                Span::styled(label, Style::default().fg(Color::Gray)),
                Span::raw(" "),
                Span::styled(when, Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect()
}

fn truncate_end(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "\u{2026}".to_string();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]));
        if width + ch_width >= max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push('\u{2026}');
    out
}

fn truncate_middle(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return truncate_end(text, max_width);
    }
    let keep_each_side = (max_width.saturating_sub(1)) / 2;
    let left: String = text.chars().take(keep_each_side).collect();
    let right: String = text
        .chars()
        .rev()
        .take(keep_each_side)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{left}\u{2026}{right}")
}

fn truncate_text(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let next = format!("{out}{ch}");
        if next.width() > max_width {
            if max_width > 1 && out.width() < max_width {
                out.push('\u{2026}');
            }
            break;
        }
        out.push(ch);
    }
    out
}

// -----------------------------------------------------------------------
// Startup notice helpers
// -----------------------------------------------------------------------

fn startup_notice_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let max_width = width.saturating_sub(10) as usize;

    match &app.bridge_state {
        crate::bridge_state::BridgeConnectionState::Connected { peer_count, .. } => {
            let label = if *peer_count > 0 {
                format!(
                    "Remote session active \u{00b7} {} peer{}",
                    peer_count,
                    if *peer_count == 1 { "" } else { "s" }
                )
            } else {
                "Remote session active".to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(CLAUDE_ORANGE)),
                Span::styled(label, Style::default().fg(Color::DarkGray)),
            ]));
        }
        crate::bridge_state::BridgeConnectionState::Reconnecting { attempt } => {
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("Reconnecting remote session (attempt #{attempt})"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        crate::bridge_state::BridgeConnectionState::Failed { reason } => {
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(Color::Red)),
                Span::styled(
                    truncate_end(reason, max_width),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        _ => {}
    }

    if let Some(url) = app.remote_session_url.as_deref() {
        lines.push(Line::from(vec![
            Span::styled(" link ", Style::default().fg(CLAUDE_ORANGE)),
            Span::styled(
                truncate_end(url, max_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    // Additional directories (from --add-dir), under the names path arguments
    // address them by.
    let project_dir = app
        .config
        .project_dir
        .clone()
        .or_else(|| app.current_dir.as_deref().map(std::path::PathBuf::from))
        .unwrap_or_default();
    for (name, dir) in mikmik_core::workspace::generate_root_names(
        &project_dir,
        &app.config.additional_dirs,
        &app.config.workspace_paths,
    ) {
        if name == mikmik_core::workspace::MAIN_ROOT {
            continue;
        }
        let label = format!(" &{name} ");
        let width = max_width.saturating_sub(label.chars().count());
        lines.push(Line::from(vec![
            Span::styled(label, Style::default().fg(Color::Cyan)),
            Span::styled(
                truncate_end(&dir.display().to_string(), width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    lines
}

fn render_startup_notices(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let lines = startup_notice_lines(app, area.width);
    if lines.is_empty() {
        return;
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[derive(Clone)]
struct RenderedLineItem {
    line: Line<'static>,
    search_text: String,
    is_header: bool,
    message_index: Option<usize>,
    /// If this line is the clickable header of a thinking block, its hash.
    thinking_hash: Option<u64>,
}

impl VirtualItem for RenderedLineItem {
    fn measure_height(&self, _width: u16) -> u16 {
        1
    }

    fn render(&self, area: Rect, buf: &mut Buffer, _selected: bool) {
        Paragraph::new(vec![self.line.clone()]).render(area, buf);
    }

    fn search_text(&self) -> String {
        self.search_text.clone()
    }

    fn is_section_header(&self) -> bool {
        self.is_header
    }
}

fn flatten_line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.to_string())
        .collect::<Vec<_>>()
        .join("")
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MessageLinesCacheKey {
    width: u16,
    transcript_version: u64,
    messages_ptr: usize,
    messages_len: usize,
    annotations_ptr: usize,
    annotations_len: usize,
    thinking_expanded_len: usize,
    // Toggling `showMessageTimestamps` mid-session changes every rendered turn
    // without touching `transcript_version`, so it has to be part of the key.
    show_timestamps: bool,
    // And `showToolDuration`, for the same reason: it adds or removes a line
    // on every finished tool block without changing a message.
    show_tool_duration: bool,
    // Same reasoning for the advisor model, which is printed on advisor tool
    // blocks. Hashed rather than owned so building a key stays allocation-free
    // on the per-frame path.
    advisor_model_hash: u64,
    // And for the palette, which colours the error state of a tool block. A
    // theme switch changes no message and no version, so without this the
    // cached lines would keep the colours of the theme that was in force when
    // they were built.
    palette: crate::theme_colors::ColorPalette,
}

#[derive(Clone)]
struct MessageLinesCache {
    key: MessageLinesCacheKey,
    lines: Vec<RenderedLineItem>,
}

/// Cache key for the *committed prefix* served during streaming: all messages
/// before the live (actively-streaming) turn.
///
/// Deliberately keyed by message/annotation identity — NOT by
/// `transcript_version`, which bumps on every streaming token and would churn
/// the entry away each frame (issue #222). During streaming the committed
/// messages do not change, so `messages_ptr`/`messages_len` stay stable and the
/// prefix is a cache hit every frame; when the committed set changes (a turn
/// completes, session switch/fork/revert/compaction) the pointer, length, or
/// `prefix_len` shifts and the entry is rebuilt. `prefix_len` is the number of
/// committed messages that precede the live turn, so growing the transcript by
/// one turn re-keys the prefix cleanly.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CompletedMsgCacheKey {
    width: u16,
    prefix_len: usize,
    messages_ptr: usize,
    messages_len: usize,
    annotations_ptr: usize,
    annotations_len: usize,
    thinking_expanded_len: usize,
    // See `MessageLinesCacheKey::show_timestamps`.
    show_timestamps: bool,
    // See `MessageLinesCacheKey::show_tool_duration`.
    show_tool_duration: bool,
    // See `MessageLinesCacheKey::advisor_model_hash`.
    advisor_model_hash: u64,
    // See `MessageLinesCacheKey::palette`.
    palette: crate::theme_colors::ColorPalette,
}

/// Hash the configured advisor model so a change invalidates the transcript
/// caches without holding an owned copy in the key.
fn advisor_model_hash(app: &App) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    app.config.advisor_model.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone)]
struct CompletedMsgCache {
    key: CompletedMsgCacheKey,
    lines: Vec<RenderedLineItem>,
}

thread_local! {
    static MESSAGE_LINES_CACHE: RefCell<Option<MessageLinesCache>> = const { RefCell::new(None) };
    /// Stores rendered lines for the committed prefix (all messages before the
    /// live turn); valid and reused across streaming deltas.
    static COMPLETED_MSG_CACHE: RefCell<Option<CompletedMsgCache>> = const { RefCell::new(None) };
}

// Instrumentation so tests can prove the committed prefix is served from cache
// (a hit) rather than rebuilt on every streaming frame. Compiled out of release
// builds.
#[cfg(test)]
thread_local! {
    static PREFIX_CACHE_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static PREFIX_CACHE_MISSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_prefix_cache_hit() {
    PREFIX_CACHE_HITS.with(|c| c.set(c.get() + 1));
}
#[cfg(test)]
fn record_prefix_cache_miss() {
    PREFIX_CACHE_MISSES.with(|c| c.set(c.get() + 1));
}
#[cfg(not(test))]
#[inline(always)]
fn record_prefix_cache_hit() {}
#[cfg(not(test))]
#[inline(always)]
fn record_prefix_cache_miss() {}

/// Test-only: `(hits, misses)` for the committed-prefix cache.
#[cfg(test)]
fn prefix_cache_counts() -> (u64, u64) {
    (
        PREFIX_CACHE_HITS.with(|c| c.get()),
        PREFIX_CACHE_MISSES.with(|c| c.get()),
    )
}

/// Test-only: reset the render caches and counters so a test starts clean and
/// is not affected by cache state left over from a previous render on this
/// thread.
#[cfg(test)]
fn reset_render_caches() {
    MESSAGE_LINES_CACHE.with(|c| *c.borrow_mut() = None);
    COMPLETED_MSG_CACHE.with(|c| *c.borrow_mut() = None);
    PREFIX_CACHE_HITS.with(|c| c.set(0));
    PREFIX_CACHE_MISSES.with(|c| c.set(0));
}

// -----------------------------------------------------------------------
// Top-level layout
// -----------------------------------------------------------------------

/// Render the entire application into the current frame.
pub fn render_app(frame: &mut Frame, app: &App) {
    let size = frame.area();
    app.last_selectable_area.set(size);

    // Fill the entire frame with a black background so the terminal's default
    // color (blue on Windows) doesn't bleed through cells not covered by widgets.
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black).fg(Color::White)),
        size,
    );

    let prompt_focused = app.permission_request.is_none() && !app.history_search_overlay.visible;
    // Suggestions popup tracks whether the prompt accepts input, not whether
    // it is the focused widget. Text entry is allowed during streaming so the
    // user can queue the next message, so the typeahead popup must follow
    // that same affordance.
    let suggestions_visible =
        app.permission_request.is_none() && !app.history_search_overlay.visible;
    let status_visible = should_render_status_row(app);
    // One blank separator row above the status/input area when status is active,
    // matching the visual breathing room in the TS layout.
    let separator_height: u16 = if status_visible { 1 } else { 0 };
    let status_height: u16 = if status_visible {
        if app.is_streaming {
            // The spinner row is always a short single line.
            1
        } else if let Some(text) = app.status_message.as_deref() {
            // Measure how many terminal rows the message needs so that long
            // error strings (e.g. "Error: overloaded_error (529): …") wrap
            // instead of overflowing the input area.  Cap at 3 lines.
            let usable_width = size.width.max(1) as usize;
            let char_count = text.chars().count();
            char_count.div_ceil(usable_width).clamp(1, 3) as u16
        } else {
            1
        }
    } else {
        0
    };
    let suggestions_height = if suggestions_visible && !app.prompt_input.suggestions.is_empty() {
        app.prompt_input.suggestions.len().min(5) as u16
    } else {
        0
    };
    // The prompt body width is the terminal width minus the prompt prefix
    // ("> ") and the right-margin padding used inside `render_prompt_input`.
    // Keep this in sync with prefix_width=2 + right_pad=2 there.
    let prompt_text_width = size.width.saturating_sub(4);
    // While the `/effort` selector is open it DOCKS into the prompt area, fully
    // replacing the prompt box, so the row budget follows the docked panel height
    // (clamped by the layout below) instead of the prompt's own line count.
    let prompt_height = if app.effort_picker.visible {
        crate::effort_picker::DOCK_HEIGHT
    } else {
        // +1 for the model/mode status line, +1 more while the companion has
        // something to say.
        let bubble = u16::from(app.companion_bubble.is_some());
        input_height(&app.prompt_input, prompt_text_width) + 1 + bubble
    };

    // The external status line takes its own rows directly above the footer.
    // It yields while the suggestion popup or a permission prompt is up, so a
    // multi-line script cannot squeeze out what the user is answering.
    let status_line_rows = if suggestions_height > 0 || app.permission_request.is_some() {
        Vec::new()
    } else {
        status_line_lines(app, size)
    };
    let status_line_height = status_line_rows.len() as u16;

    // The find bar takes one row directly above the prompt so the transcript it
    // is searching stays on screen.
    let find_bar_height = u16::from(app.transcript_find.visible);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(separator_height),
            Constraint::Length(status_height),
            Constraint::Length(find_bar_height),
            Constraint::Length(prompt_height),
            Constraint::Length(suggestions_height),
            Constraint::Length(status_line_height),
            Constraint::Length(1),
        ])
        .split(size);

    // The timeline panel takes its share out of the transcript area, so the
    // transcript rewraps at the narrower width (the render caches key on that
    // width, so the change invalidates them on its own).
    let (transcript_area, timeline_area) = split_area_for_timeline(chunks[0], app.timeline_visible);
    render_messages(frame, app, transcript_area);
    if let Some(timeline_area) = timeline_area {
        render_timeline_panel(frame, app, timeline_area);
    }
    // chunks[1] is the blank separator — intentionally left empty
    if status_height > 0 {
        render_status_row(frame, app, chunks[2]);
    }
    if find_bar_height > 0 {
        render_find_bar(frame, app, chunks[3]);
    }
    // The `/effort` selector replaces the prompt box while open: render it into
    // the input area (full width) and SKIP the prompt input. The prompt returns
    // when the picker closes on confirm/cancel.
    if app.effort_picker.visible {
        crate::effort_picker::render_effort_picker(
            frame,
            &app.effort_picker,
            chunks[4],
            app.frame_count,
        );
    } else {
        render_input(frame, app, chunks[4], prompt_focused);
    }
    app.last_input_area.set(chunks[4]);
    if suggestions_height > 0 {
        render_prompt_suggestions(frame, app, chunks[5]);
    }
    if status_line_height > 0 {
        frame.render_widget(Paragraph::new(status_line_rows), chunks[6]);
    }
    render_footer(frame, app, chunks[7]);

    // Overlays (rendered on top in Z-order)

    // Permission dialog (highest priority)
    if let Some(ref pr) = app.permission_request {
        render_permission_dialog(frame, pr, size);
    }

    // Rewind flow (takes over screen)
    if app.rewind_flow.visible {
        render_rewind_flow(frame, &app.rewind_flow, size);
    }

    // Tasks overlay (Ctrl+T)
    if app.tasks_overlay.visible {
        render_tasks_overlay(frame, &app.tasks_overlay, size);
    }

    // New help overlay
    if app.help_overlay.visible {
        render_help_overlay(frame, &app.help_overlay, size);
    } else if app.show_help {
        // Legacy fallback — render the simple help overlay
        render_simple_help_overlay(frame, size);
    }

    // History search overlay
    if app.history_search_overlay.visible {
        render_history_search_overlay(
            frame,
            &app.history_search_overlay,
            &app.prompt_input.history,
            size,
        );
    } else if let Some(ref hs) = app.history_search {
        // Legacy history search rendering
        render_legacy_history_search(frame, hs, app, size);
    }

    // Settings screen (highest-priority full-screen overlay)
    if app.settings_screen.visible {
        render_settings_screen(frame, &app.settings_screen, size);
    }

    // Theme picker overlay
    if app.theme_screen.visible {
        render_theme_screen(frame, &app.theme_screen, size);
    }

    if app.stats_dialog.visible {
        render_stats_dialog(&app.stats_dialog, size, frame.buffer_mut());
    }

    if app.mcp_view.visible {
        render_mcp_view(&app.mcp_view, size, frame.buffer_mut());
    }

    if app.agents_menu.visible {
        render_agents_menu(&app.agents_menu, size, frame.buffer_mut());
    }

    if app.diff_viewer.visible {
        let mut state = app.diff_viewer.clone();
        render_diff_dialog(&mut state, size, frame.buffer_mut());
    }

    if app.paste_viewer.visible {
        crate::paste_viewer::render_paste_viewer_buf(&app.paste_viewer, size, frame.buffer_mut());
    }

    if app.global_search.visible {
        render_global_search(&app.global_search, size, frame.buffer_mut());
    }

    if app.feedback_survey.visible {
        render_feedback_survey(&app.feedback_survey, size, frame.buffer_mut());
    }

    if app.memory_file_selector.visible {
        render_memory_file_selector(&app.memory_file_selector, size, frame.buffer_mut());
    }

    if app.hooks_config_menu.visible {
        render_hooks_config_menu(&app.hooks_config_menu, size, frame.buffer_mut());
    }

    // Overage credit upsell banner
    if app.overage_upsell.visible {
        let banner_h = app.overage_upsell.height();
        if size.height > banner_h + 4 {
            let banner_area = Rect {
                x: size.x,
                y: size.y,
                width: size.width,
                height: banner_h,
            };
            render_overage_upsell(&app.overage_upsell, banner_area, frame.buffer_mut());
        }
    }

    // Voice mode availability notice
    if app.voice_mode_notice.visible {
        let notice_h = app.voice_mode_notice.height();
        if size.height > notice_h + 4 {
            let notice_area = Rect {
                x: size.x,
                y: size.y,
                width: size.width,
                height: notice_h,
            };
            render_voice_mode_notice(&app.voice_mode_notice, notice_area, frame.buffer_mut());
        }
    }

    // Memory update notification banner (bottom of message area)
    if app.memory_update_notification.visible {
        let notif_h = app.memory_update_notification.height();
        if size.height > notif_h + 4 {
            // Place at the bottom of the screen, just above the prompt bar area
            let notif_y = size.y + size.height.saturating_sub(notif_h + 4);
            let notif_area = Rect {
                x: size.x,
                y: notif_y,
                width: size.width,
                height: notif_h,
            };
            render_memory_update_notification(
                &app.memory_update_notification,
                notif_area,
                frame.buffer_mut(),
            );
        }
    }

    // Desktop upsell startup modal
    if app.desktop_upsell.visible {
        render_desktop_upsell_startup(&app.desktop_upsell, size, frame.buffer_mut());
    }

    // Import-config preview dialog
    if app.import_config_dialog.visible {
        render_import_config_dialog(frame, &app.import_config_dialog, size);
    }

    // Invalid config/settings dialog (shown when settings.json or AGENTS.md is malformed)
    if app.invalid_config_dialog.visible {
        render_invalid_config_dialog(frame, &app.invalid_config_dialog, size);
    }

    // Bypass-permissions confirmation dialog (topmost — rendered last so it sits above all)
    if app.bypass_permissions_dialog.visible {
        render_bypass_permissions_dialog(frame, &app.bypass_permissions_dialog, size);
    }

    // File injection warning dialog (shown when oversized/binary files detected)
    if app.file_injection_dialog.visible {
        render_file_injection_dialog(frame, &app.file_injection_dialog, size);
    }

    // AskUserQuestion dialog — renders above bypass-permissions so the model's
    // question is never obscured by the startup confirmation prompt.
    if app.ask_user_dialog.visible {
        render_ask_user_dialog(&app.ask_user_dialog, size, frame.buffer_mut());
    }

    // The plan approval dialog blocks the turn the same way, so it sits at the
    // same height in the stack.
    if app.plan_approval_dialog.visible {
        crate::plan_approval_dialog::render_plan_approval_dialog(
            &app.plan_approval_dialog,
            size,
            frame.buffer_mut(),
        );
    }

    // First-launch onboarding dialog (shown after bypass dialog, below elicitation)
    if app.onboarding_dialog.visible {
        render_onboarding_dialog(frame, &app.onboarding_dialog, size);
    }

    // The `/effort` selector is NOT an overlay — it docks into the prompt input
    // area (see the input dispatch above), replacing the prompt box while open.

    // Import-config source picker
    if app.import_config_picker.visible {
        render_dialog_select(frame, &app.import_config_picker, size);
    }

    // Connect-a-provider dialog (/connect command)
    if app.connect_dialog.visible {
        render_dialog_select(frame, &app.connect_dialog, size);
    }

    // API key input dialog (opened from /connect for key-based providers)
    if app.key_input_dialog.visible {
        render_key_input_dialog(frame, &app.key_input_dialog, size);
    }

    // Custom provider URL + API key dialog.
    if app.custom_provider_dialog.visible {
        render_custom_provider_dialog(frame, &app.custom_provider_dialog, size);
    }

    // "Free" composite-provider setup dialog (Zen + OpenRouter).
    if app.free_mode_dialog.visible {
        crate::free_mode_dialog::render_free_mode_dialog(frame, &app.free_mode_dialog, size);
    }

    // Device code / browser auth dialog (GitHub Copilot, Anthropic OAuth)
    if app.device_auth_dialog.visible {
        render_device_auth_dialog(frame, &app.device_auth_dialog, size);
    }

    // Ctrl+K command palette
    if app.command_palette.visible {
        render_dialog_select(frame, &app.command_palette, size);
    }

    // MCP elicitation dialog (highest priority modal — rendered last to sit on top)
    if app.elicitation.visible {
        render_elicitation_dialog(&app.elicitation, size, frame.buffer_mut());
    }

    // Model picker overlay
    if app.model_picker.visible {
        render_model_picker(&app.model_picker, size, frame.buffer_mut());
    }

    // Session browser overlay
    if app.session_browser.visible {
        render_session_browser(&app.session_browser, size, frame.buffer_mut());
    }

    // Session branching overlay
    if app.session_branching.visible {
        render_session_branching(&app.session_branching, size, frame.buffer_mut());
    }

    // Export format picker dialog
    if app.export_dialog.visible {
        render_export_dialog(frame, &app.export_dialog, size);
    }

    // Context visualization overlay
    if app.context_viz.visible {
        render_context_viz(
            frame,
            &app.context_viz,
            size,
            app.context_used_tokens,
            app.context_window_size,
            app.rate_limit_5h_pct,
            app.rate_limit_7day_pct,
            app.cost_usd,
        );
    }

    // MCP approval dialog
    if app.mcp_approval.visible {
        render_mcp_approval_dialog(&app.mcp_approval, size, frame.buffer_mut());
    }

    // Project settings trust dialog
    if app.project_trust.visible {
        render_project_trust_dialog(&app.project_trust, size, frame.buffer_mut());
    }

    // Always show error modals on top of everything (highest priority)
    if let Some(notif) = app.notifications.current() {
        if notif.kind == NotificationKind::Error {
            let is_welcome_screen = app.messages.is_empty()
                && app.streaming_text.is_empty()
                && app.streaming_thinking.is_empty()
                && app.tool_use_blocks.is_empty();
            render_error_modal(
                frame,
                size,
                notif,
                app.error_modal_scroll_offset,
                app.footer_right_column_area.get(),
                is_welcome_screen,
            );
            return; // Don't render other overlays/notifications when error modal is showing
        }
    }

    let modal_active = is_modal_open(app);

    // Render non-error notifications as toast banners (unless another modal is open)
    if !modal_active && app.notifications.current().is_some() {
        render_notification_banner(frame, &app.notifications, size, &app.palette);
    }

    // ---- Text selection highlight (topmost post-pass) ---------------------
    apply_selection_highlight(frame, app);
    cache_selectable_row_text(frame, app);
    render_context_menu(frame, app);
}

/// Snapshot the rendered text of every row inside the selectable area into
/// `app.last_row_text` so that subsequent double/triple-clicks can locate
/// word and paragraph boundaries (issue #149 follow-up).
fn cache_selectable_row_text(frame: &mut Frame, app: &App) {
    let selectable_area = app.last_selectable_area.get();
    if selectable_area.width == 0 || selectable_area.height == 0 {
        app.last_row_text.borrow_mut().clear();
        return;
    }
    let buf = frame.buffer_mut();
    let max_row = selectable_area
        .y
        .saturating_add(selectable_area.height)
        .saturating_sub(1);
    let max_col = selectable_area
        .x
        .saturating_add(selectable_area.width)
        .saturating_sub(1);
    let mut cache = app.last_row_text.borrow_mut();
    cache.clear();
    for row in selectable_area.y..=max_row {
        let mut s = String::new();
        for col in selectable_area.x..=max_col {
            if let Some(cell) = buf.cell_mut((col, row)) {
                let sym = cell.symbol();
                if sym.is_empty() || sym == "\0" {
                    s.push(' ');
                } else {
                    s.push_str(sym);
                }
            }
        }
        cache.insert(row, s);
    }
}

/// Post-render pass: invert colours on selected cells and extract the
/// selection text into `app.selection_text`.
fn apply_selection_highlight(frame: &mut Frame, app: &App) {
    let (anchor, focus) = match (app.selection_anchor, app.selection_focus) {
        (Some(a), Some(f)) => (a, f),
        _ => return,
    };
    if anchor == focus {
        return;
    }

    let selectable_area = app.last_selectable_area.get();
    if selectable_area.width == 0 || selectable_area.height == 0 {
        return;
    }

    // Validate selection is within selectable bounds
    if anchor.0 < selectable_area.x
        || anchor.0 >= selectable_area.x.saturating_add(selectable_area.width)
        || anchor.1 < selectable_area.y
        || anchor.1 >= selectable_area.y.saturating_add(selectable_area.height)
    {
        return;
    }

    let max_row = selectable_area
        .y
        .saturating_add(selectable_area.height)
        .saturating_sub(1);
    let max_col = selectable_area
        .x
        .saturating_add(selectable_area.width)
        .saturating_sub(1);

    // Clamp anchor and focus to selectable bounds
    let anchor = (
        anchor.0.clamp(selectable_area.x, max_col),
        anchor.1.clamp(selectable_area.y, max_row),
    );
    let focus = (
        focus.0.clamp(selectable_area.x, max_col),
        focus.1.clamp(selectable_area.y, max_row),
    );

    // Normalise so start ≤ end (row-major order).
    let (start, end) = if (anchor.1, anchor.0) <= (focus.1, focus.0) {
        (anchor, focus)
    } else {
        (focus, anchor)
    };

    let buf = frame.buffer_mut();
    let mut text = String::new();
    let last_row = end.1.min(max_row);
    for row in start.1..=last_row {
        let col_from = if row == start.1 {
            start.0
        } else {
            selectable_area.x
        };
        let col_to = if row == end.1 { end.0 } else { max_col };
        for col in col_from..=col_to {
            if let Some(cell) = buf.cell_mut((col, row)) {
                let sym = cell.symbol().to_owned();
                text.push_str(if sym.is_empty() || sym == "\0" {
                    " "
                } else {
                    &sym
                });
                // Highlight: white background, black foreground
                let new_style = Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(200, 200, 200));
                cell.set_style(new_style);
            }
        }
        if row < last_row {
            // Trim trailing spaces from line before newline
            while text.ends_with(' ') {
                text.pop();
            }
            text.push('\n');
        }
    }
    while text.ends_with(|c: char| c.is_whitespace()) {
        text.pop();
    }
    *app.selection_text.borrow_mut() = text;
}

/// Render a right-click context menu at the specified position.
fn render_context_menu(frame: &mut Frame, app: &App) {
    if let Some(menu) = app.context_menu_state {
        let selection_present = !app.selection_text.borrow().trim().is_empty();
        let items: Vec<(&str, bool)> = match menu.kind {
            ContextMenuKind::Message { message_index } => vec![
                ("Copy", app.messages.get(message_index).is_some()),
                ("Fork new chat", app.messages.get(message_index).is_some()),
            ],
            ContextMenuKind::Selection => vec![("Copy", selection_present)],
        };

        let menu_height = (items.len() as u16).saturating_add(2);
        let menu_width = items
            .iter()
            .map(|(label, _)| label.len())
            .max()
            .unwrap_or(4)
            .saturating_add(4) as u16;

        // Clamp menu position to screen bounds
        let screen = frame.area();
        let menu_x = menu.x.min(screen.width.saturating_sub(menu_width + 1));
        let menu_y = menu.y.min(screen.height.saturating_sub(menu_height + 1));

        let menu_area = Rect {
            x: menu_x,
            y: menu_y,
            width: menu_width,
            height: menu_height,
        };

        // Draw menu background with border
        let menu_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().fg(Color::White).bg(Color::Rgb(24, 24, 30)))
            .border_style(Style::default().fg(MIKMIK_ACCENT));
        menu_block.render(menu_area, frame.buffer_mut());

        // Render menu items
        let inner = Rect {
            x: menu_area.x + 1,
            y: menu_area.y + 1,
            width: menu_area.width.saturating_sub(2),
            height: menu_area.height.saturating_sub(2),
        };

        for (idx, (label, enabled)) in items.iter().enumerate() {
            if idx >= inner.height as usize {
                break;
            }

            let y = inner.y + idx as u16;
            let is_selected = idx == menu.selected_index;

            let fg_color = if *enabled {
                if is_selected {
                    Color::Black
                } else {
                    Color::White
                }
            } else {
                Color::DarkGray
            };

            let bg_color = if is_selected {
                if *enabled {
                    MIKMIK_ACCENT
                } else {
                    Color::Rgb(24, 24, 30)
                }
            } else {
                Color::Rgb(24, 24, 30)
            };

            let style = Style::default().fg(fg_color).bg(bg_color);
            let padded_label = format!(
                " {:<width$} ",
                label,
                width = menu_width.saturating_sub(2) as usize
            );

            if let Some(cell) = frame.buffer_mut().cell_mut((inner.x, y)) {
                cell.set_symbol(&padded_label[0..1.min(padded_label.len())]);
                cell.set_style(style);
            }

            for (col_offset, ch) in padded_label.chars().enumerate() {
                if col_offset >= inner.width as usize {
                    break;
                }
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut((inner.x + col_offset as u16, y))
                {
                    cell.set_symbol(&ch.to_string());
                    cell.set_style(style);
                }
            }
        }
    }
}

// -----------------------------------------------------------------------
// Messages pane
// -----------------------------------------------------------------------

fn render_messages(frame: &mut Frame, app: &App, area: Rect) {
    // Reserve space at the top for plugin hint banners
    let hint_height = if app.plugin_hints.iter().any(|h| h.is_visible()) {
        3u16
    } else {
        0
    };

    let (hint_area, content_area) = if hint_height > 0 && area.height > hint_height + 2 {
        let splits = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(hint_height), Constraint::Min(1)])
            .split(area);
        (Some(splits[0]), splits[1])
    } else {
        (None, area)
    };

    // Render plugin hint banner if there is one
    if let Some(ha) = hint_area {
        render_plugin_hints(frame, &app.plugin_hints, ha);
    }

    // The rich two-column welcome box is a full welcome SCREEN, shown only while
    // the transcript is empty. Once a conversation starts it is NOT kept as a
    // fixed header (which permanently ate ~9 rows, issue #310); instead a compact
    // banner is prepended to the transcript (see `welcome_banner_lines`) so it
    // scrolls away with the content and the conversation reclaims the space.
    // A system annotation counts: a shell command run before anything was said
    // has no message behind it, and hiding it behind the welcome box would draw
    // nothing at all for a command that ran.
    let transcript_empty = app.messages.is_empty()
        && app.system_annotations.is_empty()
        && app.streaming_text.is_empty()
        && app.streaming_thinking.is_empty()
        && app.tool_use_blocks.is_empty();

    if transcript_empty {
        app.last_msg_area.set(Rect::default());
        app.message_row_map.borrow_mut().clear();
        app.thinking_row_map.borrow_mut().clear();
        render_welcome_box(frame, app, content_area);
        // Startup notices (remote session, +dir, away summary) sit just below
        // the welcome box on the empty screen.
        let notice_lines = startup_notice_lines(app, content_area.width);
        if !notice_lines.is_empty() && content_area.height > WELCOME_BOX_HEIGHT {
            let notices_area = Rect {
                x: content_area.x,
                y: content_area.y + WELCOME_BOX_HEIGHT,
                width: content_area.width,
                height: content_area.height - WELCOME_BOX_HEIGHT,
            };
            render_startup_notices(frame, app, notices_area);
        }
        return;
    }

    // Active conversation: the whole content area is the (scrollable) transcript.
    // The welcome box is no longer on screen, so clear the anchor rect used to
    // position error modals against its right column.
    app.footer_right_column_area.set(Rect::default());
    let msg_area = content_area;

    // Store the actual message pane bounds for mouse event handling (text selection, scrolling).
    app.last_msg_area.set(msg_area);

    let lines = render_message_items(app, msg_area.width);

    // Which rows the find bar's query matches. Worked out here because this is
    // the only place the transcript exists wrapped to the current width; the
    // key handler reads the result back off `App` the way it reads
    // `last_max_scroll`.
    let find_query = app
        .transcript_find
        .is_searching()
        .then(|| app.transcript_find.query.to_lowercase());
    if let Some(ref q) = find_query {
        *app.find_match_rows.borrow_mut() = lines
            .iter()
            .enumerate()
            .filter(|(_, item)| item.search_text.to_lowercase().contains(q.as_str()))
            .map(|(idx, _)| idx)
            .collect();
    } else {
        app.find_match_rows.borrow_mut().clear();
    }

    // The first row of each message, so `goToLine` can reach a message that is
    // nowhere near the viewport (`message_row_map` only covers visible rows).
    {
        let mut first_rows = app.message_first_row.borrow_mut();
        first_rows.clear();
        for (idx, item) in lines.iter().enumerate() {
            if let Some(message_index) = item.message_index {
                first_rows.entry(message_index).or_insert(idx);
            }
        }
    }

    // Highlight matches in the transcript for whichever search is live. The
    // find bar and the global-search overlay are never open at once, so one
    // query wins and the two cannot fight over the same spans.
    let highlight_query = find_query.or_else(|| {
        (app.global_search.visible && !app.global_search.query.is_empty())
            .then(|| app.global_search.query.to_lowercase())
    });
    // The row the user last stepped to, painted brighter than the rest so it
    // is findable among its neighbours.
    let current_row = app
        .transcript_find
        .current
        .and_then(|i| app.find_match_rows.borrow().get(i).copied());
    let lines = if let Some(query_lc) = highlight_query {
        lines
            .into_iter()
            .enumerate()
            .map(|(idx, mut item)| {
                if item.search_text.to_lowercase().contains(query_lc.as_str()) {
                    let is_current = current_row == Some(idx);
                    let (bg, fg) = if is_current {
                        (Color::Rgb(120, 100, 0), Color::Rgb(255, 255, 220))
                    } else {
                        (Color::Rgb(60, 50, 0), Color::Yellow)
                    };
                    // Re-render the line with the highlight on matching spans
                    let highlighted_spans: Vec<Span<'static>> = item
                        .line
                        .spans
                        .into_iter()
                        .map(|span| {
                            if span.content.to_lowercase().contains(query_lc.as_str()) {
                                Span::styled(span.content, span.style.bg(bg).fg(fg))
                            } else {
                                span
                            }
                        })
                        .collect();
                    item.line = ratatui::text::Line::from(highlighted_spans);
                }
                item
            })
            .collect()
    } else {
        lines
    };

    // Compute total virtual height and apply scroll clamping.
    // When auto_scroll is on we always show the tail; otherwise we respect
    // the user's scroll_offset.
    let content_height = lines.len() as u16;
    let visible_height = msg_area.height; // no borders, full height available
    let max_scroll = content_height.saturating_sub(visible_height) as usize;
    // Publish the max meaningful scroll offset so the next scroll event can
    // clamp `scroll_offset` against it (the content height is only known here,
    // at render time). Prevents unbounded inflation when scrolling past the top
    // (#223).
    app.last_max_scroll.set(max_scroll);
    // scroll_offset counts lines above the bottom (0 = at bottom).
    // ratatui scroll() takes an absolute top-row index, so convert:
    //   top_row = max_scroll - scroll_offset  (clamped to [0, max_scroll])
    let scroll = if app.auto_scroll {
        max_scroll
    } else {
        max_scroll.saturating_sub(app.scroll_offset)
    };

    let mut visible_rows: std::collections::HashMap<u16, usize> = std::collections::HashMap::new();
    let mut thinking_rows: std::collections::HashMap<u16, u64> = std::collections::HashMap::new();
    for (idx, item) in lines
        .iter()
        .enumerate()
        .skip(scroll)
        .take(msg_area.height as usize)
    {
        let screen_row = msg_area
            .y
            .saturating_add((idx.saturating_sub(scroll)) as u16);
        if let Some(message_index) = item.message_index {
            visible_rows.insert(screen_row, message_index);
        }
        if let Some(hash) = item.thinking_hash {
            thinking_rows.insert(screen_row, hash);
        }
    }
    *app.message_row_map.borrow_mut() = visible_rows;
    *app.thinking_row_map.borrow_mut() = thinking_rows;

    // No border — messages render directly into the area.
    let mut list = VirtualList::new();
    list.viewport_height = msg_area.height;
    list.sticky_bottom = app.auto_scroll;
    list.set_items(lines);
    list.scroll_offset = scroll as u16;

    // Track scroll offset for selection validation
    app.last_render_scroll_offset.set(scroll as u16);

    list.render(msg_area, frame.buffer_mut());

    // Scrollbar: thin vertical strip flush with the right edge — no arrow
    // caps, no visible track, muted thumb color. Mirrors Windows Terminal /
    // most modern terminal scrollbars rather than ratatui's chunky default.
    if content_height > visible_height {
        use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

        // ratatui 0.29's Scrollbar maps `position` over `content_length - 1`,
        // not over a 0..=max_scroll range. Passing `content_height` directly
        // makes the thumb top out at `content / (content + viewport)` of the
        // track when fully scrolled — i.e. it never reaches the bottom.
        // Fix: tell ratatui the content length is the number of distinct
        // scroll positions (`max_scroll + 1`), keeping `viewport_content_length`
        // for the proportional thumb size.
        let content_len = max_scroll + 1;
        let mut scrollbar_state = ScrollbarState::new(content_len)
            .position(scroll.min(max_scroll))
            .viewport_content_length(visible_height as usize);

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("\u{2590}") // ▐ right half block — thin vertical strip
            .thumb_style(Style::default().fg(Color::Rgb(110, 110, 130)));

        frame.render_stateful_widget(scrollbar, msg_area, &mut scrollbar_state);
    }

    // “↔ N new messages” indicator when scrolled up and new messages arrived.
    if app.new_messages_while_scrolled > 0 && msg_area.height > 4 && msg_area.width > 20 {
        let indicator = format!(
            " \u{2193} {} new message{} ",
            app.new_messages_while_scrolled,
            if app.new_messages_while_scrolled == 1 {
                ""
            } else {
                "s"
            }
        );
        let ind_len = indicator.len() as u16;
        let ind_x = msg_area
            .x
            .saturating_add(msg_area.width.saturating_sub(ind_len + 2));
        let ind_y = msg_area.y + msg_area.height.saturating_sub(1);
        let ind_area = Rect {
            x: ind_x,
            y: ind_y,
            width: ind_len.min(msg_area.width.saturating_sub(2)),
            height: 1,
        };
        let ind_line = Line::from(vec![Span::styled(
            indicator,
            Style::default()
                .fg(Color::Black)
                .bg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(Paragraph::new(vec![ind_line]), ind_area);
    }
}

// ---------------------------------------------------------------------------
// Live execution timeline panel
// ---------------------------------------------------------------------------

/// Below this the panel would leave nothing readable, so it is not drawn.
const TIMELINE_MIN_WIDTH: u16 = 32;
const TIMELINE_MIN_HEIGHT: u16 = 8;
/// From this width on the panel sits beside the transcript instead of below it.
const TIMELINE_SIDE_THRESHOLD: u16 = 120;
const TIMELINE_SIDE_MIN_WIDTH: u16 = 34;
const TIMELINE_SIDE_MAX_WIDTH: u16 = 52;
const TIMELINE_BOTTOM_MIN_HEIGHT: u16 = 8;
const TIMELINE_BOTTOM_MAX_HEIGHT: u16 = 12;
/// Rows the transcript keeps for itself when the panel docks at the bottom.
const TIMELINE_TRANSCRIPT_MIN_HEIGHT: u16 = 6;
/// A bottom panel thinner than this shows a border and nothing else.
const TIMELINE_BOTTOM_FLOOR: u16 = 5;

/// Divide the transcript area between the transcript and the timeline panel.
///
/// Returns the transcript rect and, when there is room for it, the panel rect.
pub(crate) fn split_area_for_timeline(area: Rect, visible: bool) -> (Rect, Option<Rect>) {
    if !visible || area.width < TIMELINE_MIN_WIDTH || area.height < TIMELINE_MIN_HEIGHT {
        return (area, None);
    }

    if area.width >= TIMELINE_SIDE_THRESHOLD {
        // The panel caps at 52 columns and this branch needs 120, so the
        // transcript always keeps at least 68: no floor check is needed.
        let panel_width = (area.width / 3).clamp(TIMELINE_SIDE_MIN_WIDTH, TIMELINE_SIDE_MAX_WIDTH);
        let transcript = Rect {
            width: area.width - panel_width,
            ..area
        };
        let panel = Rect {
            x: area.x + transcript.width,
            width: panel_width,
            ..area
        };
        return (transcript, Some(panel));
    }

    let panel_height = (area.height / 3)
        .clamp(TIMELINE_BOTTOM_MIN_HEIGHT, TIMELINE_BOTTOM_MAX_HEIGHT)
        .min(area.height.saturating_sub(TIMELINE_TRANSCRIPT_MIN_HEIGHT));
    if panel_height < TIMELINE_BOTTOM_FLOOR {
        return (area, None);
    }
    let transcript = Rect {
        height: area.height - panel_height,
        ..area
    };
    let panel = Rect {
        y: area.y + transcript.height,
        height: panel_height,
        ..area
    };
    (transcript, Some(panel))
}

/// The glyph and colour that stand for a row's status.
fn timeline_status_marker(status: TimelineStatus, frame_count: u64) -> (String, Color) {
    match status {
        TimelineStatus::Running => (spinner_char(frame_count).to_string(), Color::Cyan),
        TimelineStatus::Done => (figures::DIAMOND_FILLED.to_string(), Color::Green),
        TimelineStatus::Error => ("×".to_string(), Color::Red),
        TimelineStatus::Cancelled => (figures::DIAMOND_OPEN.to_string(), Color::DarkGray),
    }
}

/// Render a duration the way the status row does: sub-second in milliseconds,
/// then seconds, then minutes.
fn timeline_duration_label(duration_ms: u64) -> String {
    if duration_ms < 1000 {
        return format!("{duration_ms}ms");
    }
    let seconds = duration_ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    format!("{}m{:02}s", (seconds as u64) / 60, (seconds as u64) % 60)
}

/// Compact token counts so a 34-column panel can still show them.
fn timeline_token_label(tokens: u64) -> String {
    if tokens < 10_000 {
        return tokens.to_string();
    }
    format!("{:.1}k", tokens as f64 / 1000.0)
}

/// The trailing metric for a row: its duration, plus token deltas when the row
/// is a turn summary that carries them.
fn timeline_row_metrics(row: &TimelineRow) -> String {
    let mut parts = Vec::new();
    if let Some(duration) = row.duration_ms() {
        parts.push(timeline_duration_label(duration));
    }
    if let Some(input) = row.token_delta_input {
        parts.push(format!(
            "{}{}",
            figures::UP_ARROW,
            timeline_token_label(input)
        ));
    }
    if let Some(output) = row.token_delta_output {
        parts.push(format!(
            "{}{}",
            figures::DOWN_ARROW,
            timeline_token_label(output)
        ));
    }
    parts.join(" ")
}

/// Average output tokens per second across the finished timeline rows.
///
/// Only rows that both finished and reported an output delta are counted, so a
/// running row does not drag the average down. `None` when nothing has finished
/// yet or the measured time is zero, which keeps a division by zero out of a
/// per-frame code path.
fn timeline_output_rate(rows: &[TimelineRow]) -> Option<f64> {
    let mut tokens = 0u64;
    let mut elapsed_ms = 0u64;
    for row in rows {
        let (Some(output), Some(duration)) = (row.token_delta_output, row.duration_ms()) else {
            continue;
        };
        tokens = tokens.saturating_add(output);
        elapsed_ms = elapsed_ms.saturating_add(duration);
    }
    if tokens == 0 || elapsed_ms == 0 {
        return None;
    }
    Some(tokens as f64 * 1000.0 / elapsed_ms as f64)
}

/// The timeline panel's bottom summary: throughput, plus how much the session
/// has to work with. These do not fit the footer, whose right-hand side is
/// already nine sections wide on a narrow terminal.
///
/// Returns `None` when there is nothing to say, so the panel spends the row on
/// timeline entries instead.
fn timeline_summary_line(app: &App, width: u16) -> Option<Line<'static>> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(rate) = timeline_output_rate(&app.timeline.rows) {
        parts.push(format!("{:.1} tok/s", rate));
    }
    if app.skill_count > 0 {
        parts.push(format!("{} skills", app.skill_count));
    }
    let mcp_count = app.config.mcp_servers.len();
    if mcp_count > 0 {
        parts.push(format!("{} mcp", mcp_count));
    }
    if parts.is_empty() {
        return None;
    }
    // Drop sections from the right until the line fits, rather than letting
    // ratatui clip mid-word.
    while parts.len() > 1 && parts.join(" · ").chars().count() > width as usize {
        parts.pop();
    }
    let text = parts.join(" · ");
    if text.chars().count() > width as usize {
        return None;
    }
    Some(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )))
}

/// Build the visible rows, newest last, ending on the selected row.
///
/// The panel has no scrollbar: it always shows the window that contains the
/// cursor, so a selection made with the arrow keys can never fall off-screen.
fn timeline_window(
    row_count: usize,
    selected_idx: usize,
    capacity: usize,
) -> std::ops::Range<usize> {
    if capacity == 0 || row_count == 0 {
        return 0..0;
    }
    let capacity = capacity.min(row_count);
    let end = (selected_idx + 1).max(capacity).min(row_count);
    (end - capacity)..end
}

/// Draw the timeline panel into `area`.
fn render_timeline_panel(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.timeline_focused;
    let border_color = if focused {
        MIKMIK_ACCENT
    } else {
        Color::DarkGray
    };
    let title = if app.timeline.is_empty() {
        " timeline ".to_string()
    } else {
        format!(" timeline ({}) ", app.timeline.len())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.timeline.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No steps recorded yet.",
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    }

    // The detail goes directly under the row it belongs to, so it spends rows
    // from the same budget and the window has to be shortened by its height.
    let detail_lines = if app.timeline_expanded {
        timeline_detail_lines(app, inner.width)
    } else {
        Vec::new()
    };
    // The summary sits on the panel's last row, so it spends from the same
    // budget as the detail lines. Without this the bottom timeline entry would
    // be pushed off the panel.
    let summary = timeline_summary_line(app, inner.width);
    let capacity = (inner.height as usize)
        .saturating_sub(detail_lines.len())
        .saturating_sub(usize::from(summary.is_some()))
        .max(1);

    let window = timeline_window(app.timeline.len(), app.timeline.selected_idx, capacity);
    let mut lines = Vec::with_capacity(window.len() + detail_lines.len());
    for idx in window {
        let Some(row) = app.timeline.rows.get(idx) else {
            continue;
        };
        let selected = idx == app.timeline.selected_idx;
        lines.push(timeline_row_line(
            row,
            selected,
            focused,
            inner.width,
            app.frame_count,
        ));
        if selected {
            lines.extend(detail_lines.iter().cloned());
        }
    }
    if let Some(summary) = summary {
        lines.truncate((inner.height as usize).saturating_sub(1));
        lines.push(summary);
    }
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One row: status marker, title, and the trailing metrics, fitted to `width`.
fn timeline_row_line(
    row: &TimelineRow,
    selected: bool,
    focused: bool,
    width: u16,
    frame_count: u64,
) -> Line<'static> {
    let (marker, marker_color) = timeline_status_marker(row.status, frame_count);
    let metrics = timeline_row_metrics(row);
    // marker + space, then a space before the metrics when there are any.
    let reserved = 2 + if metrics.is_empty() {
        0
    } else {
        metrics.width() + 1
    };
    let title_width = (width as usize).saturating_sub(reserved);
    let title = truncate_end(&expand_tabs(&row.title), title_width);
    let padding = title_width.saturating_sub(title.width());

    let title_style = if selected && focused {
        Style::default()
            .fg(Color::Black)
            .bg(MIKMIK_ACCENT)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let mut spans = vec![
        Span::styled(marker, Style::default().fg(marker_color)),
        Span::raw(" "),
        Span::styled(title, title_style),
    ];
    if !metrics.is_empty() {
        spans.push(Span::raw(" ".repeat(padding + 1)));
        spans.push(Span::styled(metrics, Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

/// The detail block shown under the list while the selected row is expanded.
fn timeline_detail_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let Some(row) = app.timeline.selected_row() else {
        return Vec::new();
    };
    let body = if row.expandable_details.trim().is_empty() {
        row.detail_preview.as_str()
    } else {
        row.expandable_details.as_str()
    };
    if body.trim().is_empty() {
        return Vec::new();
    }
    // A narrow panel gets one folded line instead of a wrapped block, so the
    // detail never crowds the list it belongs to.
    let max_lines = if width < TIMELINE_SIDE_MIN_WIDTH {
        1
    } else {
        4
    };
    let mut lines = Vec::with_capacity(max_lines);
    for raw in body.lines().take(max_lines) {
        lines.push(Line::from(Span::styled(
            truncate_end(&expand_tabs(raw), width as usize),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

fn push_rendered_items(
    items: &mut Vec<RenderedLineItem>,
    lines: Vec<Line<'static>>,
    message_index: Option<usize>,
    mark_first_header: bool,
) {
    for (index, line) in lines.into_iter().enumerate() {
        items.push(RenderedLineItem {
            search_text: flatten_line_text(&line),
            is_header: mark_first_header && index == 0,
            message_index,
            thinking_hash: None,
            line,
        });
    }
}

/// Push tagged lines from `render_transcript_assistant_message_tagged`.
/// Lines with `Some(hash)` become clickable thinking headers.
fn push_rendered_items_tagged(
    items: &mut Vec<RenderedLineItem>,
    tagged: Vec<(Line<'static>, Option<u64>)>,
    message_index: Option<usize>,
) {
    for (line, thinking_hash) in tagged {
        items.push(RenderedLineItem {
            search_text: flatten_line_text(&line),
            is_header: false,
            message_index,
            thinking_hash,
            line,
        });
    }
}

fn push_blank_item(items: &mut Vec<RenderedLineItem>) {
    push_rendered_items(items, vec![Line::from("")], None, false);
}

fn render_live_thinking_lines(
    turn: &TranscriptTurn<'_>,
    frame_count: u64,
    width: u16,
) -> Vec<Line<'static>> {
    let mut header_spans = vec![Span::raw("  ▼ ")];
    header_spans.extend(shimmer_spans("Thinking", frame_count));
    if let Some(heading) = turn.reasoning_heading() {
        header_spans.push(Span::styled(
            format!(": {}", heading),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    let mut lines = vec![Line::from(header_spans)];
    if let Some(text) = turn.live_thinking {
        lines.extend(render_thinking_live_content(text, width));
    }
    lines
}

fn append_turn_items(
    items: &mut Vec<RenderedLineItem>,
    turn: &TranscriptTurn<'_>,
    ctx: &RenderContext,
    frame_count: u64,
    accent: Color,
) {
    let width = ctx.width;
    push_rendered_items(
        items,
        render_transcript_user_message(turn.user_message, ctx),
        Some(turn.user_index),
        true,
    );

    enum SectionContent {
        Plain(Vec<Line<'static>>),
        Tagged(Vec<(Line<'static>, Option<u64>)>),
    }

    let mut sections: Vec<(SectionContent, Option<usize>)> = Vec::new();
    for (message_index, message) in &turn.assistant_messages {
        let tagged = render_transcript_assistant_message_tagged(message, ctx);
        if !tagged.is_empty() {
            sections.push((SectionContent::Tagged(tagged), Some(*message_index)));
        }
    }

    for block in &turn.tool_blocks {
        let mut lines = Vec::new();
        render_tool_block_lines(
            &mut lines,
            block,
            frame_count,
            ctx.advisor_model,
            &ctx.palette,
            ctx.width,
            ctx.show_tool_duration,
        );
        if !lines.is_empty() {
            sections.push((
                SectionContent::Plain(lines),
                Some(turn.primary_message_index()),
            ));
        }
    }

    if turn.active && turn.live_thinking.is_some() {
        sections.push((
            SectionContent::Plain(render_live_thinking_lines(turn, frame_count, width)),
            Some(turn.primary_message_index()),
        ));
    }

    // Show a "Thinking" shimmer when the turn is active but no text or
    // thinking content has arrived yet — gives visual feedback that the
    // model is working (especially for providers without thinking support).
    if turn.active
        && turn.live_text.is_none()
        && turn.live_thinking.is_none()
        && turn
            .tool_blocks
            .iter()
            .all(|b| b.status != ToolStatus::Running)
    {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(shimmer_spans("Thinking", frame_count));
        sections.push((
            SectionContent::Plain(vec![Line::from(spans)]),
            Some(turn.primary_message_index()),
        ));
    }

    if let Some(text) = turn.live_text {
        let lines = render_transcript_live_text(text, width);
        if !lines.is_empty() {
            sections.push((
                SectionContent::Plain(lines),
                Some(turn.primary_message_index()),
            ));
        }
    }

    if !turn.active {
        // The turn's footer time is the last assistant message's instant, i.e.
        // when the reply finished, not when the user submitted.
        let replied_at = turn
            .assistant_messages
            .last()
            .and_then(|(_, message)| message.timestamp.as_deref());
        if let Some(meta_line) =
            render_transcript_assistant_meta(turn.metadata, accent, replied_at, ctx.show_timestamps)
        {
            if turn.has_visible_assistant_content() {
                sections.push((
                    SectionContent::Plain(vec![meta_line]),
                    Some(turn.primary_message_index()),
                ));
            }
        }
    }

    if !sections.is_empty() {
        push_blank_item(items);
        let total_sections = sections.len();
        for (index, (content, message_index)) in sections.into_iter().enumerate() {
            match content {
                SectionContent::Plain(lines) => {
                    push_rendered_items(items, lines, message_index, false)
                }
                SectionContent::Tagged(tagged) => {
                    push_rendered_items_tagged(items, tagged, message_index)
                }
            }
            if index + 1 < total_sections {
                push_blank_item(items);
            }
        }
    }

    push_blank_item(items);
}

/// Append rendered items for the transcript messages in `[start, end)` to
/// `items`, mirroring the single linear pass used by the full transcript build.
///
/// System annotations are emitted at the top of each landed index exactly as
/// the full pass does; `emit_end_annotations` additionally flushes the
/// annotations anchored at `end` (used when `end` is the true message count so
/// trailing annotations are not lost).
///
/// Splitting the pass at a turn boundary is byte-identical to building the whole
/// range in one shot: `range(0, k, false)` followed by `range(k, total, true)`
/// produces exactly the same items as `range(0, total, true)` whenever `k` is an
/// index the linear pass lands on (i.e. a turn's user index). This is what lets
/// the streaming path serve the committed prefix from cache and rebuild only the
/// live tail without any risk of ghosting.
#[allow(clippy::too_many_arguments)]
fn build_message_items_range(
    app: &App,
    width: u16,
    ctx: &RenderContext,
    turn_map: &std::collections::HashMap<usize, &TranscriptTurn<'_>>,
    start: usize,
    end: usize,
    emit_end_annotations: bool,
    items: &mut Vec<RenderedLineItem>,
) {
    let mut index = start;
    while index < end {
        for ann in app
            .system_annotations
            .iter()
            .filter(|ann| ann.after_index == index)
        {
            let mut lines = Vec::new();
            render_system_annotation_lines(&mut lines, ann, width as usize);
            push_rendered_items(items, lines, None, false);
        }

        let message = &app.messages[index];
        if message.role == Role::User {
            if let Some(&turn) = turn_map.get(&index) {
                append_turn_items(items, turn, ctx, app.frame_count, app.accent_color);
                index = turn.end_message_index + 1;
                continue;
            }
        }

        let tagged = render_transcript_assistant_message_tagged(message, ctx);
        push_rendered_items_tagged(items, tagged, Some(index));
        push_blank_item(items);
        index += 1;
    }

    if emit_end_annotations {
        for ann in app
            .system_annotations
            .iter()
            .filter(|ann| ann.after_index == end)
        {
            let mut lines = Vec::new();
            render_system_annotation_lines(&mut lines, ann, width as usize);
            push_rendered_items(items, lines, None, false);
        }
    }
}

/// Build the full transcript item list from scratch (no caching). Used for the
/// non-streaming path, the streaming fallback, and as the correctness reference
/// in tests.
fn build_all_items(app: &App, width: u16) -> Vec<RenderedLineItem> {
    // Build `tool_names` and the render context ONCE per rebuild and lend them
    // to every message renderer (issue #222).
    let tool_names = build_tool_names(&app.messages);
    let tool_durations = build_tool_durations(&app.messages);
    let ctx = RenderContext {
        width,
        highlight: true,
        show_thinking: false,
        tool_names: &tool_names,
        expanded_thinking: &app.thinking_expanded,
        show_timestamps: app.settings_screen.show_message_timestamps,
        show_tool_duration: app.settings_screen.show_tool_duration,
        tool_durations: &tool_durations,
        advisor_model: app.config.advisor_model.as_deref(),
        goal_completed: app.goal_completed,
        palette: app.palette,
    };
    let turns = build_transcript_turns(app);
    let mut turn_map = std::collections::HashMap::new();
    for turn in &turns {
        turn_map.insert(turn.user_index, turn);
    }

    let total = app.messages.len();
    let mut items = Vec::new();
    // Prepend the compact welcome banner as ordinary scrollable content so it
    // scrolls away with the conversation instead of sitting in a fixed header
    // (issue #310).
    push_rendered_items(&mut items, welcome_banner_lines(app, width), None, false);
    build_message_items_range(app, width, &ctx, &turn_map, 0, total, true, &mut items);

    if total == 0 && !app.tool_use_blocks.is_empty() {
        for block in &app.tool_use_blocks {
            let mut lines = Vec::new();
            render_tool_block_lines(
                &mut lines,
                block,
                app.frame_count,
                app.config.advisor_model.as_deref(),
                &app.palette,
                width,
                app.settings_screen.show_tool_duration,
            );
            push_rendered_items(&mut items, lines, None, false);
            push_blank_item(&mut items);
        }
    }

    items
}

fn render_message_items(app: &App, width: u16) -> Vec<RenderedLineItem> {
    let streaming =
        app.is_streaming || !app.streaming_text.is_empty() || !app.streaming_thinking.is_empty();
    let has_running_tool_blocks = app
        .tool_use_blocks
        .iter()
        .any(|block| block.status == ToolStatus::Running);
    let cacheable = !streaming && !has_running_tool_blocks;

    if !cacheable {
        // Live content is on screen. Instead of re-rendering the whole backlog
        // every frame (the O(messages^2) hot path from issue #222), serve the
        // committed prefix from cache and rebuild only the live tail.
        return render_streaming_items(app, width);
    }

    // Fast path: nothing live — use the full-result cache (ptr-stable check).
    let full_key = MessageLinesCacheKey {
        width,
        transcript_version: app.transcript_version.get(),
        messages_ptr: app.messages.as_ptr() as usize,
        messages_len: app.messages.len(),
        annotations_ptr: app.system_annotations.as_ptr() as usize,
        annotations_len: app.system_annotations.len(),
        thinking_expanded_len: app.thinking_expanded.len(),
        show_timestamps: app.settings_screen.show_message_timestamps,
        show_tool_duration: app.settings_screen.show_tool_duration,
        advisor_model_hash: advisor_model_hash(app),
        palette: app.palette,
    };
    if let Some(lines) = MESSAGE_LINES_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|c| c.key == full_key)
            .map(|c| c.lines.clone())
    }) {
        return lines;
    }

    let items = build_all_items(app, width);
    MESSAGE_LINES_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(MessageLinesCache {
            key: full_key,
            lines: items.clone(),
        });
    });
    items
}

/// Render the transcript while there is live content on screen.
///
/// The only part of the transcript that changes between streaming frames is the
/// last turn (its live text/thinking and any running tool blocks). Every earlier
/// turn is already committed and byte-identical to a full rebuild, so we serve
/// that committed prefix from `COMPLETED_MSG_CACHE` and rebuild only the live
/// tail. Because `build_message_items_range` splits the exact same linear pass
/// at a turn boundary, `prefix ++ tail` is identical to `build_all_items` — no
/// ghosting, no missing content.
fn render_streaming_items(app: &App, width: u16) -> Vec<RenderedLineItem> {
    let tool_names = build_tool_names(&app.messages);
    let tool_durations = build_tool_durations(&app.messages);
    let ctx = RenderContext {
        width,
        highlight: true,
        show_thinking: false,
        tool_names: &tool_names,
        expanded_thinking: &app.thinking_expanded,
        show_timestamps: app.settings_screen.show_message_timestamps,
        show_tool_duration: app.settings_screen.show_tool_duration,
        tool_durations: &tool_durations,
        advisor_model: app.config.advisor_model.as_deref(),
        goal_completed: app.goal_completed,
        palette: app.palette,
    };
    let turns = build_transcript_turns(app);

    // The live tail is the last turn; its user index is the prefix boundary.
    // Without a turn (e.g. tool-blocks-only welcome state) there is no stable
    // prefix to reuse, so fall back to a full rebuild.
    let split_idx = match turns.last() {
        Some(last) => last.user_index,
        None => return build_all_items(app, width),
    };

    let mut turn_map = std::collections::HashMap::new();
    for turn in &turns {
        turn_map.insert(turn.user_index, turn);
    }

    let total = app.messages.len();
    let prefix_key = CompletedMsgCacheKey {
        width,
        prefix_len: split_idx,
        messages_ptr: app.messages.as_ptr() as usize,
        messages_len: total,
        annotations_ptr: app.system_annotations.as_ptr() as usize,
        annotations_len: app.system_annotations.len(),
        thinking_expanded_len: app.thinking_expanded.len(),
        show_timestamps: app.settings_screen.show_message_timestamps,
        show_tool_duration: app.settings_screen.show_tool_duration,
        advisor_model_hash: advisor_model_hash(app),
        palette: app.palette,
    };

    // Committed prefix: messages before the live turn. Stable across streaming
    // deltas, so keyed by identity (not `transcript_version`) and served from
    // cache every frame after the first. The cached prefix does NOT include the
    // welcome banner, so the entry stays byte-identical to the non-streaming
    // build's committed range.
    let prefix = if let Some(lines) = COMPLETED_MSG_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|c| c.key == prefix_key)
            .map(|c| c.lines.clone())
    }) {
        record_prefix_cache_hit();
        lines
    } else {
        record_prefix_cache_miss();
        let mut prefix = Vec::new();
        build_message_items_range(
            app,
            width,
            &ctx,
            &turn_map,
            0,
            split_idx,
            false,
            &mut prefix,
        );
        COMPLETED_MSG_CACHE.with(|cache| {
            *cache.borrow_mut() = Some(CompletedMsgCache {
                key: prefix_key,
                lines: prefix.clone(),
            });
        });
        prefix
    };

    // The welcome banner leads the transcript and scrolls away with content
    // (issue #310). Prepended here, outside the committed-prefix cache, so
    // banner ++ prefix ++ tail stays byte-identical to `build_all_items`.
    let mut items = Vec::new();
    push_rendered_items(&mut items, welcome_banner_lines(app, width), None, false);
    items.extend(prefix);

    // Live tail: the actively-streaming turn, rebuilt fresh every frame.
    build_message_items_range(
        app, width, &ctx, &turn_map, split_idx, total, true, &mut items,
    );
    items
}

// ── Welcome / startup screen ─────────────────────────────────────────────────

/// Render the two-column orange round-bordered welcome box (matches TS LogoV2).
fn render_welcome_box(frame: &mut Frame, app: &App, area: Rect) {
    // --- Box dimensions ---
    // The box should be at most the full area width, and a fixed height.
    let box_width = area.width;
    let box_height: u16 = WELCOME_BOX_HEIGHT;
    if area.height < box_height || box_width < 30 {
        // Too small: fall back to a single line
        let line = Line::from(vec![
            Span::styled(
                "MikMik ",
                Style::default()
                    .fg(CLAUDE_ORANGE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{}", APP_VERSION),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(vec![line]), area);
        return;
    }
    let box_area = Rect {
        x: area.x,
        y: area.y,
        width: box_width,
        height: box_height,
    };

    // Outer border with title "MikMik vX.Y"
    let accent = app.accent_color;
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Line::from(vec![
            Span::styled(
                " MikMik ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{} ", APP_VERSION),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    frame.render_widget(outer_block, box_area);

    // Inner area (inside the border)
    let inner = Rect {
        x: box_area.x + 1,
        y: box_area.y + 1,
        width: box_area.width.saturating_sub(2),
        height: box_area.height.saturating_sub(2),
    };

    // Split inner into left | divider(1) | right
    // Left width: ~28 chars or half the inner width, whichever is smaller
    let left_w = (inner.width / 2)
        .clamp(22, 32)
        .min(inner.width.saturating_sub(3));
    let right_w = inner.width.saturating_sub(left_w + 1);
    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_w),
            Constraint::Length(1),
            Constraint::Length(right_w),
        ])
        .split(inner);

    // Store the right column area for error modal positioning
    app.footer_right_column_area.set(h_chunks[2]);

    // Draw vertical divider in accent color
    let divider_lines: Vec<Line> = (0..inner.height)
        .map(|_| Line::from(Span::styled("\u{2502}", Style::default().fg(accent))))
        .collect();
    frame.render_widget(Paragraph::new(divider_lines), h_chunks[1]);

    // --- Left column ---
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty());
    let welcome_msg = if let Some(ref name) = username {
        format!("Welcome back {}!", name)
    } else {
        "Welcome back!".to_string()
    };
    let mikmik = mikmik_lines(&app.mikmik_current_pose);
    let mut left_lines: Vec<Line> = Vec::new();
    left_lines.push(Line::from(Span::styled(
        welcome_msg,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    left_lines.push(Line::from(""));
    // Center mascot in left column. Every row is MIKMIK_WIDTH wide, so one
    // indent centres them all.
    let mascot_indent = left_w.saturating_sub(MIKMIK_WIDTH) / 2;
    let pad = " ".repeat(mascot_indent as usize);
    for cl in &mikmik {
        let mut spans = vec![Span::raw(pad.clone())];
        spans.extend(cl.spans.iter().cloned());
        left_lines.push(Line::from(spans));
    }
    // The name, centred under the cat. The mascot is three rows where the old
    // one was four, so this row costs nothing.
    let name_indent = left_w.saturating_sub(MIKMIK_NAME.len() as u16) / 2;
    left_lines.insert(
        left_lines.len() - 1,
        Line::from(Span::styled(
            format!("{}{MIKMIK_NAME}", " ".repeat(name_indent as usize)),
            Style::default().fg(Color::Rgb(150, 150, 164)),
        )),
    );
    frame.render_widget(
        Paragraph::new(left_lines).wrap(Wrap { trim: false }),
        h_chunks[0],
    );

    // --- Right column ---
    let tip_text = mikmik_core::tips::select_tip(0)
        .map(|t| t.content.to_string())
        .unwrap_or_else(|| "Edit AGENTS.md to add instructions for MikMik".to_string());

    let mut right_lines: Vec<Line> = Vec::new();
    right_lines.push(Line::from(Span::styled(
        "Tips for getting started",
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )));
    // Word-wrap the tip text into the right column width
    let right_w_usize = right_w.saturating_sub(1) as usize;
    for chunk in tip_text
        .chars()
        .collect::<Vec<_>>()
        .chunks(right_w_usize.max(1))
    {
        right_lines.push(Line::from(chunk.iter().collect::<String>()));
    }
    right_lines.push(Line::from(""));
    right_lines.push(Line::from(Span::styled(
        "Recent activity",
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )));
    right_lines.extend(recent_activity_lines(&app.recent_sessions, right_w_usize));

    frame.render_widget(
        Paragraph::new(right_lines).wrap(Wrap { trim: false }),
        h_chunks[2],
    );
}

/// Build the compact welcome banner shown at the very top of the transcript.
///
/// Unlike the full two-column welcome box (which is a whole welcome *screen*
/// rendered only while the transcript is empty), this banner is prepended to the
/// message list as ordinary scrollable content, so it scrolls away with the
/// conversation instead of occupying a permanent fixed header (issue #310). It
/// carries the greeting the box led with plus a getting-started hint and any
/// startup notices, in just a few rows.
///
/// Deliberately free of disk/IO or per-frame state (no `select_tip`, which reads
/// the tip history from disk) so it is cheap to rebuild every streaming frame and
/// byte-identical between the full and cached-prefix render paths.
fn welcome_banner_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let accent = app.accent_color;

    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty());
    let greeting = match username {
        Some(ref name) => format!("Welcome back, {}!", name),
        None => "Welcome to MikMik".to_string(),
    };

    // Too narrow for a bordered box: fall back to a single title line + notices.
    if width < 24 {
        let mut lines = vec![Line::from(vec![
            Span::styled(
                "MikMik ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{}", APP_VERSION),
                Style::default().fg(Color::DarkGray),
            ),
        ])];
        lines.extend(startup_notice_lines(app, width));
        lines.push(Line::from(""));
        return lines;
    }

    let box_w = width as usize;
    let inner_w = box_w.saturating_sub(4); // "│ " + content + " │"

    // Top border with an embedded title: ╭─ MikMik vX.Y ─…─╮
    // Span widths: "╭─"=2, TITLE, "v{ver} "=ver+2, dashes=fill, "╮"=1.
    const TITLE: &str = " MikMik ";
    let used = 2 + TITLE.len() + (APP_VERSION.len() + 2) + 1;
    let fill = box_w.saturating_sub(used);
    let top = Line::from(vec![
        Span::styled("\u{256d}\u{2500}", Style::default().fg(accent)),
        Span::styled(
            TITLE,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("v{} ", APP_VERSION),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{}\u{256e}", "\u{2500}".repeat(fill)),
            Style::default().fg(accent),
        ),
    ]);

    let content_row = |text: String, style: Style| -> Line<'static> {
        let text = truncate_end(&text, inner_w);
        let pad = inner_w.saturating_sub(UnicodeWidthStr::width(text.as_str()));
        Line::from(vec![
            Span::styled("\u{2502} ", Style::default().fg(accent)),
            Span::styled(text, style),
            Span::raw(" ".repeat(pad)),
            Span::styled(" \u{2502}", Style::default().fg(accent)),
        ])
    };

    let bottom = Line::from(Span::styled(
        format!(
            "\u{2570}{}\u{256f}",
            "\u{2500}".repeat(box_w.saturating_sub(2))
        ),
        Style::default().fg(accent),
    ));

    let mut lines = vec![
        top,
        content_row(
            greeting,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        content_row(
            "/help for commands  \u{00b7}  ? for shortcuts".to_string(),
            Style::default().fg(Color::Gray),
        ),
        bottom,
    ];
    lines.extend(startup_notice_lines(app, width));
    lines.push(Line::from(""));
    lines
}

// ── Per-message rendering ─────────────────────────────────────────────────────

/// Build a tool_use_id → tool_name lookup from all messages in the transcript.
/// This allows ToolResult blocks to dispatch to tool-specific renderers.
fn build_tool_names(
    messages: &[mikmik_core::types::Message],
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for msg in messages {
        for block in msg.content_blocks() {
            if let mikmik_core::types::ContentBlock::ToolUse { id, name, .. } = block {
                map.insert(id.clone(), name.clone());
            }
        }
    }
    map
}

/// Gather how long each tool call took, from the messages that recorded it.
///
/// The durations sit on the message carrying the tool results, and a
/// `ToolResult` block is rendered from a different message than the one it was
/// recorded on, so the lookup is built once per rebuild rather than searched
/// for per block.
fn build_tool_durations(
    messages: &[mikmik_core::types::Message],
) -> std::collections::HashMap<String, u64> {
    let mut map = std::collections::HashMap::new();
    for msg in messages {
        for (id, took) in msg.tool_durations.iter().flatten() {
            map.insert(id.clone(), *took);
        }
    }
    map
}

/// The line that reports how long a tool call took, right-aligned in `width`.
///
/// `None` when there is nothing to report, so the caller adds no line at all
/// rather than an empty one.
pub(crate) fn tool_duration_line(
    duration_ms: Option<u64>,
    width: u16,
    show: bool,
) -> Option<Line<'static>> {
    if !show {
        return None;
    }
    let label = timeline_duration_label(duration_ms?);
    // Right-aligned by padding rather than by `Alignment::Right`, which needs a
    // `Rect`; a transcript line is built before any area is known.
    let padding = (width as usize).saturating_sub(label.width());
    Some(Line::from(vec![
        Span::raw(" ".repeat(padding)),
        Span::styled(
            label,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
    ]))
}

// ── System annotation (compact boundary, info notices) ───────────────────────

fn render_system_annotation_lines(
    lines: &mut Vec<Line<'static>>,
    ann: &SystemAnnotation,
    width: usize,
) {
    // Compact boundary: show ✻ prefix with dimmed text
    if ann.style == SystemMessageStyle::Compact {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", figures::TEARDROP_ASTERISK),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                ann.text.clone(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
        lines.push(Line::from(""));
        return;
    }

    let (text_color, border_color) = match ann.style {
        SystemMessageStyle::Info => (Color::DarkGray, Color::DarkGray),
        SystemMessageStyle::Warning => (Color::Yellow, Color::Yellow),
        SystemMessageStyle::Compact => unreachable!(),
    };

    // A block of text (a shell command and its output) has no business in a
    // one-line rule: it would run off the edge and take the whole thing with
    // it. Draw it as indented lines under a rule instead.
    if ann.text.contains('\n') {
        lines.push(Line::from(Span::styled(
            format!("  {}", "\u{2500}".repeat(width.saturating_sub(4))),
            Style::default().fg(border_color),
        )));
        for line in ann.text.lines() {
            for wrapped in crate::prompt_input::wrap_line(line, width.saturating_sub(4)) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", wrapped),
                    Style::default().fg(text_color).add_modifier(Modifier::DIM),
                )));
            }
        }
        lines.push(Line::from(""));
        return;
    }

    // Centred, padded rule: "─── text ───"
    let text = ann.text.as_str();
    let inner_width = width.saturating_sub(4);
    let text_len = text.len();
    let dashes = inner_width.saturating_sub(text_len + 2);
    let left = dashes / 2;
    let right = dashes - left;

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}", "\u{2500}".repeat(left)),
            Style::default().fg(border_color),
        ),
        Span::styled(
            format!("\u{2500} {} \u{2500}", text),
            Style::default().fg(text_color).add_modifier(Modifier::DIM),
        ),
        Span::styled("\u{2500}".repeat(right), Style::default().fg(border_color)),
    ]));
    lines.push(Line::from(""));
}

// ── Tool use block ────────────────────────────────────────────────────────────

/// Per-tool marker shown at the head of a tool block (the marker conveys the
/// tool, the line then shows the primary argument). Falls back to the generic
/// `~` for unmapped tools.
///
/// These are deliberately ASCII: many terminals render "pretty" Unicode glyphs
/// (arrows, ✱, ☰, …) two cells wide while ratatui's layout counts them as one,
/// which both breaks header alignment and desyncs the scroll redraw. ASCII is
/// guaranteed one cell everywhere, and the shell-flavoured choices read well in
/// context (`<` read, `>` write, `*` glob, `/` grep).
fn tool_icon(normalized: &str) -> &'static str {
    match normalized {
        "bash" | "powershell" => "$",
        "read" => "<",
        "write" | "apply_patch" | "edit" => ">",
        "glob" | "list" => "*",
        "grep" | "codesearch" => "/",
        "webfetch" => "@",
        "websearch" => "?",
        "todowrite" | "todo_write" | "todo" => ":",
        "task" | "agent" => "+",
        "lsp" => "^",
        _ => "~",
    }
}

/// Replace a leading home-directory prefix with `~` for compact display
/// (mirrors pi's `shortenPath`). Works on Windows too via `dirs::home_dir`.
fn shorten_home_path(s: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        let home = home.trim_end_matches(['/', '\\']);
        if !home.is_empty() && s.starts_with(home) {
            let rest = &s[home.len()..];
            return format!("~{}", rest);
        }
    }
    s.to_string()
}

/// Replace tabs with spaces before text reaches a buffer cell.
///
/// ratatui puts a tab in one cell and counts it as one column, but a terminal
/// advances the cursor to the next tab stop instead. Everything after it on the
/// row then sits one or more columns right of where ratatui believes it is, the
/// skipped cells keep whatever the terminal had there, and a redraw cannot
/// repair it because ratatui sees those cells as already correct. Tool output is
/// full of tabs, so every line built from external text goes through here.
///
/// Four spaces, matching `paste_viewer`.
fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    text.replace('\t', "    ")
}

/// Running-state verb shown (with shimmer) while a tool is in flight.
pub(crate) fn tool_running_label(normalized: &str, fallback: &str) -> String {
    match normalized {
        "bash" | "powershell" => "Running command",
        "read" => "Reading file",
        "write" | "apply_patch" => "Writing file",
        "edit" => "Editing file",
        "glob" | "list" => "Listing files",
        "grep" | "codesearch" => "Searching code",
        "webfetch" => "Fetching page",
        "websearch" => "Searching web",
        "todowrite" | "todo_write" | "todo" => "Updating todos",
        "lsp" => "Asking the language server",
        _ => fallback,
    }
    .to_string()
}

/// The whole result of a finished block, when it is short enough to sit on the
/// header line instead of under a rule.
///
/// Only the language-server block, where "nothing found" is an ordinary answer
/// and the three lines it takes are all noise. An error keeps its own line: it
/// is the one result worth the room.
fn fold_onto_header(normalized: &str, block: &crate::app::ToolUseBlock) -> Option<String> {
    if normalized != "lsp" || block.status != ToolStatus::Done {
        return None;
    }
    let preview = block.output_preview.as_ref()?;
    let text = preview.trim();
    if text.is_empty() || text.contains('\n') || text.chars().count() > FOLDED_RESULT_LIMIT {
        return None;
    }
    Some(text.to_string())
}

fn render_tool_block_lines(
    lines: &mut Vec<Line<'static>>,
    block: &crate::app::ToolUseBlock,
    frame_count: u64,
    advisor_model: Option<&str>,
    palette: &crate::theme_colors::ColorPalette,
    // The pane's width, for the right-aligned duration. Every other line here
    // is laid out by a fixed indent and needs none.
    width: u16,
    show_duration: bool,
) {
    let input_val: serde_json::Value =
        serde_json::from_str(&block.input_json).unwrap_or(serde_json::Value::Null);
    let normalized = block.name.to_ascii_lowercase();
    let running = block.status == ToolStatus::Running;
    let accent = if block.status == ToolStatus::Error {
        palette.error
    } else {
        CLAUDE_ORANGE
    };
    let icon = tool_icon(&normalized);

    // TodoWrite renders as a real checklist rather than a generic tool block.
    if matches!(normalized.as_str(), "todowrite" | "todo_write" | "todo")
        && render_todo_block(lines, &input_val, icon, accent, running, frame_count)
    {
        return;
    }

    // The advisor collapses to a one-line status: the advice itself lands in
    // the tool result, so echoing the question here would duplicate it.
    if normalized == "advisor" {
        lines.extend(crate::messages::render_advisor_message(
            running,
            advisor_model,
        ));
        return;
    }

    // Primary argument shown on the header line (icon + arg), opencode-style.
    let mut summary = crate::messages::extract_tool_summary(&block.name, &input_val);
    let running_label = if normalized == "task" || normalized == "agent" {
        if let Some(description) = input_val
            .get("description")
            .and_then(|value| value.as_str())
        {
            summary = description.to_string();
        }
        crate::messages::subagent_title(&input_val)
    } else {
        tool_running_label(&normalized, &block.name)
    };

    // Shorten home paths in path-bearing summaries.
    if matches!(
        normalized.as_str(),
        "read" | "edit" | "write" | "apply_patch" | "glob" | "list"
    ) {
        summary = shorten_home_path(&summary);
    }

    let mut header_spans = vec![Span::styled(
        format!("   {} ", icon),
        Style::default().fg(accent),
    )];
    if running {
        header_spans.extend(shimmer_spans(&running_label, frame_count));
    } else {
        // Show the primary argument; fall back to the tool name when there is none.
        let primary = if summary.is_empty() {
            block.name.clone()
        } else {
            summary
        };
        header_spans.push(Span::styled(
            primary,
            Style::default()
                .fg(if block.status == ToolStatus::Error {
                    accent
                } else {
                    Color::White
                })
                .add_modifier(Modifier::BOLD),
        ));
    }

    // A one-line answer sits on the header rather than under a rule of its
    // own. "Nothing found" is the ordinary answer to half of what a language
    // server is asked, and three lines to say it buries the rest of the
    // transcript.
    let folded = fold_onto_header(&normalized, block);
    if let Some(ref text) = folded {
        header_spans.push(Span::styled(
            format!("  {text}"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    lines.push(Line::from(header_spans));
    if folded.is_some() {
        return;
    }

    // What the command has printed so far. Only ever filled while it runs and
    // only when the setting is on, so an ordinary session reaches neither this
    // branch nor the allocation behind it.
    if running && !block.live_output.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("     "),
            Span::styled(
                TOOL_OUTPUT_RULE.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
        for line_text in block.live_output_tail(LIVE_OUTPUT_TAIL_LINES) {
            lines.push(Line::from(vec![
                Span::raw("     "),
                Span::styled(
                    expand_tabs(&shorten_home_path(line_text)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }

    // Output preview (done/error state) — home paths shortened, dimmed.
    if let Some(ref preview) = block.output_preview {
        // A rule between the command and what it printed. Without it the
        // header and the first output line read as one wrapped sentence.
        lines.push(Line::from(vec![
            Span::raw("     "),
            Span::styled(
                TOOL_OUTPUT_RULE.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]));

        let preview_style = match block.status {
            ToolStatus::Error => Style::default().fg(palette.error),
            _ => Style::default().fg(Color::DarkGray),
        };
        for line_text in preview.lines() {
            if line_text.starts_with('\u{2026}') {
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(
                        expand_tabs(line_text),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(expand_tabs(&shorten_home_path(line_text)), preview_style),
                ]));
            }
        }
    }

    // Last, so it reads as the block's footer rather than as part of the
    // output. A running block has no duration yet and gets no line.
    if let Some(line) = tool_duration_line(block.duration_ms, width, show_duration) {
        lines.push(line);
    }
}

/// The confidence score to show for one todo item.
///
/// A completed item prefers the score recorded at completion and falls back to
/// the up-front estimate, so an item completed without a fresh score still
/// shows the number the model last stood behind.
fn todo_confidence(todo: &serde_json::Value) -> Option<u8> {
    let completed = todo.get("status").and_then(|status| status.as_str()) == Some("completed");
    let value = completed
        .then(|| todo.get("completion_confidence"))
        .flatten()
        .filter(|value| !value.is_null())
        .or_else(|| todo.get("confidence"))?;
    mikmik_tools::todo_write::parse_confidence(value)
}

/// One score for the whole list, weighted by priority so a shaky high-priority
/// item is not averaged away by several confident trivial ones.
fn aggregate_todo_confidence(todos: &[serde_json::Value]) -> Option<u8> {
    let mut weighted_sum = 0u32;
    let mut total_weight = 0u32;
    for todo in todos {
        let Some(score) = todo_confidence(todo) else {
            continue;
        };
        let weight = match todo.get("priority").and_then(|priority| priority.as_str()) {
            Some("high") => 3,
            Some("medium") => 2,
            _ => 1,
        };
        weighted_sum += u32::from(score) * weight;
        total_weight += weight;
    }
    (total_weight > 0).then(|| ((weighted_sum + total_weight / 2) / total_weight) as u8)
}

/// Named colours rather than RGB, so a terminal theme still applies.
fn confidence_color(score: u8) -> Color {
    match score {
        80..=100 => Color::Green,
        50..=79 => Color::Yellow,
        _ => Color::Red,
    }
}

/// Render a TodoWrite call as a checklist. Returns `false` (so the caller can
/// fall back to the generic block) when the input carries no `todos` array.
fn render_todo_block(
    lines: &mut Vec<Line<'static>>,
    input_val: &serde_json::Value,
    icon: &str,
    accent: Color,
    running: bool,
    frame_count: u64,
) -> bool {
    let Some(todos) = input_val.get("todos").and_then(|v| v.as_array()) else {
        return false;
    };
    if todos.is_empty() {
        return false;
    }

    fn status_of(t: &serde_json::Value) -> &str {
        t.get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending")
    }
    let done = todos.iter().filter(|t| status_of(t) == "completed").count();
    let total = todos.len();

    // Header: ☰ Todos   <done>/<total>
    let mut header = vec![Span::styled(
        format!("   {} ", icon),
        Style::default().fg(accent),
    )];
    if running {
        header.extend(shimmer_spans("Updating todos", frame_count));
    } else {
        header.push(Span::styled(
            "Todos".to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        header.push(Span::styled(
            format!("  {}/{} done", done, total),
            Style::default().fg(Color::DarkGray),
        ));
        if let Some(confidence) = aggregate_todo_confidence(todos) {
            header.push(Span::styled(
                format!(" · confidence {}%", confidence),
                Style::default().fg(confidence_color(confidence)),
            ));
        }
    }
    lines.push(Line::from(header));

    // Checklist items: ✓ done (green/dim) · • in-progress (orange) · ○ pending.
    const MAX_ITEMS: usize = 12;
    for t in todos.iter().take(MAX_ITEMS) {
        let content = t
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            continue;
        }
        // ASCII checkboxes (markdown-style) so alignment holds on every
        // terminal: [x] done, [>] in-progress, [ ] pending.
        let (glyph, glyph_color, text_style) = match status_of(t) {
            "completed" => (
                "[x]",
                Color::Rgb(120, 200, 120),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
            "in_progress" => (
                "[>]",
                accent,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            _ => (
                "[ ]",
                Color::Rgb(150, 150, 150),
                Style::default().fg(Color::Rgb(170, 170, 170)),
            ),
        };
        let mut item = vec![
            Span::styled(format!("     {} ", glyph), Style::default().fg(glyph_color)),
            Span::styled(content.to_string(), text_style),
        ];
        if let Some(score) = todo_confidence(t) {
            item.push(Span::styled(
                format!(" [{}%]", score),
                Style::default().fg(confidence_color(score)),
            ));
        }
        lines.push(Line::from(item));
    }
    if total > MAX_ITEMS {
        lines.push(Line::from(vec![
            Span::raw("     "),
            Span::styled(
                format!("... {} more", total - MAX_ITEMS),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
    }
    true
}

// -----------------------------------------------------------------------
// Input pane
// -----------------------------------------------------------------------

/// Width of a companion sprite, plus one column of breathing room.
///
/// Every sprite in `mikmik-buddy` pads its rows to exactly 12 columns.
const COMPANION_COLUMN: u16 = 13;

/// Narrowest the prompt may become before the companion is dropped.
///
/// The companion is decoration; the input box is the product.
const MIN_INPUT_WIDTH: u16 = 40;

fn render_input(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    // The companion's line goes above everything else, including the status
    // line, so it reads as the companion talking rather than as chrome.
    let area = match (&app.companion_bubble, &app.companion) {
        (Some(line), Some(companion)) if area.height > 3 => {
            let splits = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(area);
            render_companion_bubble(frame, companion, line, splits[0]);
            splits[1]
        }
        _ => area,
    };

    // Split: 1-row model/mode status line + remaining rows for the prompt input.
    let (status_area, input_area) = if area.height > 2 {
        let splits = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(area);
        (Some(splits[0]), splits[1])
    } else {
        // Not enough room for the extra line — skip the status row.
        (None, area)
    };

    // Give the companion a column beside the prompt box, but only when the
    // prompt can spare the width. The status line above keeps its full width
    // either way: it already truncates the model name at 80 columns, and
    // taking 13 more would cut it to nothing.
    let input_area = match &app.companion {
        Some(companion) if input_area.width >= COMPANION_COLUMN + MIN_INPUT_WIDTH => {
            let splits = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(COMPANION_COLUMN), Constraint::Min(1)])
                .split(input_area);
            render_companion(frame, companion, app, splits[0]);
            splits[1]
        }
        _ => input_area,
    };

    // Render model + agent mode status line above the prompt.
    if let Some(status_area) = status_area {
        let agent_mode = match app.agent_mode.as_deref() {
            Some(m) => m,
            None if app.plan_mode => "plan",
            _ => "build",
        };

        let pink = app.accent_color;
        let dim = Color::Rgb(110, 110, 124);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(status_area.width.min(50)),
            ])
            .split(status_area);

        let left_line = if app.has_credentials {
            // The same split the turn loop makes, so the status line names the
            // account the next request will actually reach. Splitting on the
            // first `/` here would read `meta-llama/Llama-3.3` as an account.
            let route = app.config.resolve_route(&app.model_name);
            let (provider, model_short) = (route.account, route.model.to_string());
            let mut spans = vec![
                Span::styled(
                    format!(" {} ", agent_mode.to_uppercase()),
                    Style::default()
                        .fg(Color::Black)
                        .bg(pink)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    model_short,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            spans.push(Span::styled(
                format!(" · {}", provider),
                Style::default().fg(dim),
            ));
            if let Some(ref badge) = app.agent_type_badge {
                spans.push(Span::styled(
                    format!(" · {}", badge),
                    Style::default().fg(dim),
                ));
            }
            Line::from(spans)
        } else {
            Line::from(vec![
                Span::styled(
                    " /connect ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(pink)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" connect a provider", Style::default().fg(dim)),
            ])
        };

        // `?` opens the shortcuts overlay which already lists Ctrl+A / Ctrl+K
        // and friends — surfacing them again here is redundant clutter. It is
        // also suppressed once the prompt has text, so the hint doesn't compete
        // with what the user is typing (matches the footer contract).
        let right_hint = if app.has_credentials && app.prompt_input.text.is_empty() {
            Line::from(vec![Span::styled("? shortcuts", Style::default().fg(dim))])
        } else if app.prompt_input.has_expandable_paste_ref() {
            // A [Pasted text #N ...] placeholder is in the buffer — tell the
            // user how to view the full pasted body before submitting.
            Line::from(vec![Span::styled(
                "click to view paste · alt+e expands",
                Style::default().fg(dim),
            )])
        } else {
            Line::from(Vec::<Span>::new())
        };

        let left_padded = Rect {
            x: chunks[0].x + 1,
            y: chunks[0].y,
            width: chunks[0].width.saturating_sub(1),
            height: chunks[0].height,
        };
        let right_padded = Rect {
            x: chunks[1].x,
            y: chunks[1].y,
            width: chunks[1].width.saturating_sub(1),
            height: chunks[1].height,
        };
        frame.render_widget(Paragraph::new(vec![left_line]), left_padded);
        frame.render_widget(
            Paragraph::new(vec![right_hint]).alignment(Alignment::Right),
            right_padded,
        );
    }

    render_prompt_input(
        &app.prompt_input,
        input_area,
        frame.buffer_mut(),
        focused,
        if app.is_streaming {
            InputMode::Readonly
        } else if app.plan_mode {
            InputMode::Plan
        } else {
            InputMode::Default
        },
        app.accent_color,
        app.settings_screen.cursor_blink_enabled,
    );
}

/// Draw the companion sprite in its own column, bottom-aligned so it stands on
/// the same line as the bottom of the prompt box rather than floating.
///
/// Rows that do not fit are dropped from the top, which is where the sprites
/// keep their hat and their per-frame flourishes.
fn render_companion(frame: &mut Frame, companion: &mikmik_buddy::Companion, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // The sprite animates on a 500 ms cycle. Derived from elapsed time rather
    // than the frame counter so the pace does not follow the redraw rate.
    let tick = app.session_start.elapsed().as_millis() as u64 / 500;
    let sprite = mikmik_buddy::render(companion, tick);

    let rows: Vec<&str> = sprite.lines().collect();
    let shown = rows.len().min(area.height as usize);
    let lines: Vec<Line> = rows[rows.len() - shown..]
        .iter()
        .map(|row| {
            Line::from(Span::styled(
                row.to_string(),
                Style::default().fg(Color::Rgb(150, 150, 164)),
            ))
        })
        .collect();

    let sprite_area = Rect {
        x: area.x,
        y: area.y + area.height - shown as u16,
        width: area.width,
        height: shown as u16,
    };
    frame.render_widget(Paragraph::new(lines), sprite_area);
}

/// Draw the companion's line: its face, then what it said.
///
/// One row, truncated rather than wrapped. The companion is asked for a single
/// short line, and a bubble that grows to three rows would push the prompt box
/// around while the user is typing.
fn render_companion_bubble(
    frame: &mut Frame,
    companion: &mikmik_buddy::Companion,
    line: &str,
    area: Rect,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let dim = Color::Rgb(150, 150, 164);
    let spans = vec![
        Span::styled(
            format!("  {} ", mikmik_buddy::render_face(&companion.bones)),
            Style::default().fg(dim),
        ),
        Span::styled(
            line.replace('\n', " "),
            Style::default().fg(dim).add_modifier(Modifier::ITALIC),
        ),
    ];
    frame.render_widget(Paragraph::new(vec![Line::from(spans)]), area);
}

fn should_render_status_row(app: &App) -> bool {
    let interesting_stream_status = app
        .status_message
        .as_deref()
        .map(|status| {
            let trimmed = status.trim();
            !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case(STATUS_THINKING)
                && !trimmed.eq_ignore_ascii_case(STATUS_THINKING_ELLIPSIS)
        })
        .unwrap_or(false);

    // Note: a completed turn's "Worked for Xs" summary (`last_turn_elapsed`) is
    // intentionally NOT a reason to keep the status row on — it stays set until
    // the next submit, so gating on it pinned the idle spinner glyph on screen
    // permanently after the first turn. The row now shows only while actually
    // active (voice, streaming, or an idle status message).
    app.voice_recording
        || (!app.is_streaming && app.status_message.is_some())
        || (app.is_streaming && interesting_stream_status)
}

fn render_status_row(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    let spans = if app.voice_recording {
        vec![Span::styled(
            format!(
                "{} Recording... press Alt+V to transcribe",
                figures::black_circle()
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]
    } else if app.is_streaming {
        // Pick a label: use the status message if it has real content,
        // otherwise show a default "Thinking" shimmer so the user always
        // sees that the model is working.
        let raw_label = app
            .status_message
            .as_deref()
            .filter(|s| {
                let t = s.trim();
                !t.is_empty()
                    && !t.eq_ignore_ascii_case(STATUS_THINKING)
                    && !t.eq_ignore_ascii_case(STATUS_THINKING_ELLIPSIS)
            })
            .or(app.spinner_verb.as_deref())
            .unwrap_or("Thinking");

        let mut s = vec![Span::styled(
            spinner_char(app.frame_count).to_string(),
            Style::default()
                .fg(spinner_color(app))
                .add_modifier(Modifier::BOLD),
        )];
        let label = format!("{}…", raw_label.trim_end_matches('…'));

        s.push(Span::raw(" "));
        s.extend(shimmer_spans(&label, app.frame_count));
        s
    } else if let (Some(verb), Some(elapsed)) =
        (app.last_turn_verb, app.last_turn_elapsed.as_deref())
    {
        // "✽ Worked for 2m 5s" — mirrors TS TeammateSpinnerLine idle state
        vec![Span::styled(
            format!("{} {} for {}", figures::TEARDROP_ASTERISK, verb, elapsed),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )]
    } else if let Some(status) = app.status_message.as_deref() {
        vec![Span::styled(
            status.to_string(),
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        Vec::new()
    };

    if spans.is_empty() {
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

/// Build spans for a text string with a right-to-left glimmer sweep, matching
/// the TS `GlimmerMessage` behaviour (glimmerSpeed=200ms, 3-char shimmer window).
///
/// At ~50ms per frame a 4-frame step ≈ 200ms, giving the same cadence as TS.
fn shimmer_spans(text: &str, frame_count: u64) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len == 0 {
        return Vec::new();
    }

    // Cycle length = text_len + 20 (10 off-screen on each side)
    let cycle_len = len + 20;
    // One step every 4 frames (~200ms at 50ms/frame)
    let cycle_pos = (frame_count as usize / 4) % cycle_len;
    // Glimmer sweeps right→left: starts at len+10 (off right), ends at -10 (off left)
    let glimmer_center = (len + 10).saturating_sub(cycle_pos) as isize;

    let base = Style::default().fg(Color::DarkGray);
    let bright = Style::default().fg(Color::White);

    // Accumulate runs of same style to minimise span count
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_bright = false;

    for (i, &ch) in chars.iter().enumerate() {
        let is_bright = (i as isize - glimmer_center).abs() <= 1
            && glimmer_center >= 0
            && glimmer_center < len as isize;

        if is_bright != run_bright && !run.is_empty() {
            spans.push(Span::styled(
                run.clone(),
                if run_bright { bright } else { base },
            ));
            run.clear();
        }
        run_bright = is_bright;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_bright { bright } else { base }));
    }
    spans
}
// Keybinding hints footer
// -----------------------------------------------------------------------

/// `(done, total)` for the most recent TodoWrite call, or `None` when this
/// session has no todo list.
///
/// Read from the tool blocks already in memory rather than from the persisted
/// list: the footer redraws on every frame, so a disk read here would be one
/// file read per frame, and the in-memory copy is the same one the transcript
/// checklist draws from, so the two can never disagree.
fn footer_todo_progress(app: &App) -> Option<(usize, usize)> {
    let block = app.tool_use_blocks.iter().rev().find(|block| {
        matches!(
            block.name.to_ascii_lowercase().as_str(),
            "todowrite" | "todo_write" | "todo"
        )
    })?;
    let input: serde_json::Value = serde_json::from_str(&block.input_json).ok()?;
    let todos = input.get("todos")?.as_array()?;
    if todos.is_empty() {
        return None;
    }
    let done = todos
        .iter()
        .filter(|todo| todo.get("status").and_then(|s| s.as_str()) == Some("completed"))
        .count();
    Some((done, todos.len()))
}

/// Single footer line matching the TS contract more closely:
/// - `? for shortcuts` is suppressed once the prompt becomes non-empty
/// - the right side shows comprehensive status info and notifications
fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    // Use only the first line of the footer area, leaving bottom padding
    let footer_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };

    // Left side: ordered pills — voice > PR badge > background task > vim > hint
    let left_spans: Vec<Span> = if app.voice_recording {
        vec![Span::styled(
            format!(" {} REC — speak now", figures::black_circle()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]
    } else {
        let mut spans: Vec<Span> = Vec::new();

        // Agent type badge (shown when running as subagent / coordinator)
        if let Some(ref badge) = app.agent_type_badge {
            spans.push(Span::styled(
                format!("\u{2699} {}", badge),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // PR badge — shows "PR #<n>" in cyan, with optional state in brackets.
        // State color: approved=green, changes_requested=red,
        //              review_required=yellow, else=gray.
        if let Some(pr_num) = app.pr_number {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let pr_label = match &app.pr_state {
                Some(state) => format!("PR #{} [{}]", pr_num, state),
                None => format!("PR #{}", pr_num),
            };
            // Colors mirror TS PrBadge getPrStatusColor + TS ink color names:
            //   approved → Green, changes_requested → Red (error),
            //   pending / review_required → Yellow (warning), merged → Magenta.
            let pr_color = match app.pr_state.as_deref() {
                Some("approved") => Color::Green,
                Some("changes_requested") => Color::Red,
                Some("merged") => Color::Magenta,
                Some("pending") | Some("review_required") => Color::Yellow,
                Some(_) => Color::Gray,
                None => Color::Cyan,
            };
            spans.push(Span::styled(
                pr_label,
                Style::default().fg(pr_color).add_modifier(Modifier::BOLD),
            ));
        }

        // Background task status pill — shows "⟳ N tasks" when count > 0.
        // Falls back to background_task_status pre-formatted string if set.
        if app.background_task_count > 0 {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let label = if app.background_task_count == 1 {
                "\u{27f3} 1 task".to_string()
            } else {
                format!("\u{27f3} {} tasks", app.background_task_count)
            };
            spans.push(Span::styled(label, Style::default().fg(Color::Yellow)));
        } else if let Some(ref task_status) = app.background_task_status {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("\u{27f3} {}", task_status),
                Style::default().fg(Color::Yellow),
            ));
        }

        // Vim mode indicator — shown for all modes using neovim "-- MODE --" convention.
        // INSERT is dim (common, low-noise); other modes use bright colour.
        // A status line that renders `vim.mode` itself can suppress this so the
        // mode is not shown twice.
        let vim_indicator_hidden = app
            .config
            .status_line
            .as_ref()
            .is_some_and(|status_line| status_line.hide_vim_mode_indicator);
        if app.prompt_input.vim_enabled && !vim_indicator_hidden {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let (label, style) = match app.prompt_input.vim_mode {
                VimMode::Insert => ("-- INSERT --", Style::default().fg(Color::DarkGray)),
                VimMode::Normal => (
                    "-- NORMAL --",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                VimMode::Visual => (
                    "-- VISUAL --",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                VimMode::VisualLine => (
                    "-- VISUAL LINE --",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                VimMode::VisualBlock => (
                    "-- VISUAL BLOCK --",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                VimMode::Command => (
                    "-- COMMAND --",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                VimMode::Search => (
                    "-- SEARCH --",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            };
            spans.push(Span::styled(label, style));
        }

        // Bash prefix indicator — shown when the line will run as a shell
        // command. `!!` escapes the bang, so that line goes to the model and
        // must not be badged as one that does not.
        if crate::input::is_bang_command(&app.prompt_input.text) {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                "[BASH]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // Permission mode badge (left side, mirrors TS bottom-left indicator).
        // Default mode is silent; non-default modes show a badge.
        {
            use mikmik_core::config::PermissionMode;
            match &app.config.permission_mode {
                PermissionMode::BypassPermissions => {
                    if !spans.is_empty() {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(
                        "\u{23f5}\u{23f5} bypass",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                }
                PermissionMode::AcceptEdits => {
                    if !spans.is_empty() {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(
                        "accept-edits",
                        Style::default().fg(Color::Yellow),
                    ));
                }
                PermissionMode::Plan => {
                    if !spans.is_empty() {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled("plan", Style::default().fg(Color::Blue)));
                }
                PermissionMode::Default => {}
            }
        }

        // During streaming show "esc to interrupt". The "? shortcuts" hint is
        // rendered in the top-right status bar (see render_prompt area), so do
        // not duplicate it here (issue #149 follow-up).
        if spans.is_empty() && app.is_streaming {
            spans.push(Span::styled(
                "esc interrupt",
                Style::default().fg(Color::DarkGray),
            ));
        }

        spans
    };

    // Right side: status metrics and lightweight badges.
    let right_spans: Vec<Span> = {
        let mut parts: Vec<Span> = Vec::new();

        // 1. Context window usage — show "N% until auto-compact" mirroring TS TokenWarning.
        //    When an update is available and context is below 85%, show the update notification
        //    instead to keep the status bar uncluttered.
        if app.context_window_size > 0 {
            let used_pct =
                (app.context_used_tokens as f64 / app.context_window_size as f64 * 100.0) as u64;
            let left_pct = 100u64.saturating_sub(used_pct);

            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }

            if used_pct >= 85 {
                // High usage — always show context window info regardless of update status.
                if used_pct >= 95 {
                    parts.push(Span::styled(
                        format!("{}% context used — /compact now", used_pct),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                } else {
                    parts.push(Span::styled(
                        format!("{}% until auto-compact", left_pct),
                        Style::default().fg(Color::Yellow),
                    ));
                }
            } else if let Some(ref version) = app.update_available {
                // Update available and context is fine — show update nudge in bottom-right.
                parts.push(Span::styled(
                    format!("⬆ v{} available  Run: /update", version),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            } else if used_pct >= 70 {
                // 70–84%: mild warning.
                parts.push(Span::styled(
                    format!("{}% until auto-compact", left_pct),
                    Style::default().fg(Color::Yellow),
                ));
            } else {
                // Normal: dim display.
                let used_k = app.context_used_tokens / 1000;
                let total_k = app.context_window_size / 1000;
                parts.push(Span::styled(
                    format!("{}k/{}k", used_k, total_k),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }

        // 3. Cost — mirrors TS formatCost: 4 decimal places for costs < $0.50, else 2.
        // Display cost if it's >= 0.0, so free models show $0.00
        if app.cost_usd >= 0.0 {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            let cost_str = if app.cost_usd < 0.5 {
                format!("${:.4}", app.cost_usd)
            } else {
                format!("${:.2}", app.cost_usd)
            };
            parts.push(Span::styled(cost_str, Style::default().fg(Color::DarkGray)));
        }

        // 3b. Token budget (feature-gated)
        #[cfg(feature = "token_budget")]
        if let Some(max_tokens) = app.token_budget {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            let used = app.token_count as u64;
            let max = max_tokens as u64;
            let pct = if max > 0 {
                (used as f64 / max as f64 * 100.0) as u32
            } else {
                0
            };
            let color = if pct >= 90 {
                Color::Red
            } else if pct >= 75 {
                Color::Yellow
            } else {
                Color::DarkGray
            };
            parts.push(Span::styled(
                format!("Tokens: {}/{} ({}%)", used, max, pct),
                Style::default().fg(color),
            ));
        }

        // 3c. Todo progress — mirrors the checklist block in the transcript.
        if let Some((done, total)) = footer_todo_progress(app) {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            let color = if done == total {
                Color::Green
            } else {
                Color::Yellow
            };
            parts.push(Span::styled(
                format!("\u{2713}{}/{}", done, total),
                Style::default().fg(color),
            ));
        }

        // 4. Rate limits
        if let Some(pct) = app.rate_limit_5h_pct {
            if pct > 0.0 {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                let color = if pct >= 90.0 {
                    Color::Red
                } else {
                    Color::Yellow
                };
                parts.push(Span::styled(
                    format!("5h:{:.0}%", pct),
                    Style::default().fg(color),
                ));
            }
        }
        if let Some(pct) = app.rate_limit_7day_pct {
            if pct > 0.0 {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                let color = if pct >= 90.0 {
                    Color::Red
                } else {
                    Color::Yellow
                };
                parts.push(Span::styled(
                    format!("7d:{:.0}%", pct),
                    Style::default().fg(color),
                ));
            }
        }

        // 5. Vim mode — displayed on the left side as "-- MODE --"; nothing extra on right.

        // 5b. Goal badge — shown when a goal is active for this session.
        if let Some(ref badge) = app.active_goal_badge {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[goal: {}]", badge),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // 6. Agent type badge
        if let Some(ref badge) = app.agent_type_badge {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", badge),
                Style::default().fg(MIKMIK_ACCENT),
            ));
        }

        // 7. Worktree branch
        if let Some(ref branch) = app.worktree_branch {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", branch),
                Style::default().fg(Color::Green),
            ));
        }

        // Git branch (if settings enabled)
        if app.settings_screen.show_git_branch {
            if let Some(ref branch) = app.git_branch {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                parts.push(Span::styled(
                    format!("⎇ {}", branch),
                    Style::default().fg(Color::Cyan),
                ));
            }
        }

        // Current directory (if settings enabled)
        if app.settings_screen.show_cwd {
            if let Some(ref dir) = app.current_dir {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                // Use dirs::home_dir() so this works on Windows (where $HOME
                // is unset and the home is $USERPROFILE). Guard against an
                // empty home string: `str::replace("", "~")` inserts "~"
                // between every character, producing the infamous
                // `~X~:~\~B~i~g~g~e~r~…` output.
                let home = dirs::home_dir()
                    .and_then(|p| p.to_str().map(|s| s.to_string()))
                    .filter(|s| !s.is_empty());
                let display_dir = match home {
                    Some(h) if dir.starts_with(&h) => dir.replacen(&h, "~", 1),
                    _ => dir.clone(),
                };
                parts.push(Span::styled(
                    display_dir,
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }

        // Output style indicator (only when non-default)
        if app.output_style != "auto" {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", app.output_style),
                Style::default().fg(Color::DarkGray),
            ));
        }

        // 8. Bridge badge
        if let Some(badge) = app.bridge_state.status_badge(app.frame_count) {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(badge);
        } else if app.pending_mcp_reconnect {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                "MCP reconnecting",
                Style::default().fg(Color::Yellow),
            ));
        }

        parts
    };

    // Gap fill
    let left_len: usize = left_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_len: usize = right_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let gap = (footer_area.width.saturating_sub(2) as usize).saturating_sub(left_len + right_len);

    let mut spans = left_spans;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right_spans);

    // Add padding: 1 char on each side
    let padded_area = Rect {
        x: footer_area.x + 1,
        y: footer_area.y,
        width: footer_area.width.saturating_sub(2),
        height: footer_area.height,
    };
    frame.render_widget(Paragraph::new(vec![Line::from(spans)]), padded_area);
}

/// Lay out the external status line command's output for the rows above the
/// footer: styled from its own ANSI, padded, and clipped to the terminal.
///
/// Returns no rows when nothing has been printed yet, which collapses the row
/// group so the transcript keeps the space.
fn status_line_lines(app: &App, size: Rect) -> Vec<Line<'static>> {
    let Some(text) = app.status_line_override.as_deref() else {
        return Vec::new();
    };
    if text.is_empty() || size.width == 0 {
        return Vec::new();
    }

    let requested = app
        .config
        .status_line
        .as_ref()
        .and_then(|status_line| status_line.padding)
        .unwrap_or(0);
    // Leave at least one column of content, however wide the padding asks to be.
    let padding = requested.min(size.width.saturating_sub(1) / 2) as usize;
    let content_width = size.width as usize - padding * 2;
    // A script that prints a hundred lines must not push the transcript away.
    let max_rows = (size.height / 2).max(1) as usize;

    crate::ansi::ansi_to_lines(text)
        .into_iter()
        .take(max_rows)
        .map(|line| pad_and_clip(line, padding, content_width))
        .collect()
}

/// Indent a line by `padding` columns and cut it off at `max_width`, measuring
/// in display columns so wide glyphs do not overrun the row.
fn pad_and_clip(line: Line<'static>, padding: usize, max_width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }

    let mut width = 0usize;
    for span in line.spans {
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if width + span_width <= max_width {
            width += span_width;
            spans.push(span);
            continue;
        }
        // Clip inside this span, keeping a column for the ellipsis.
        let style = span.style;
        let mut kept = String::new();
        for ch in span.content.chars() {
            let ch_width = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]));
            if width + ch_width + 1 > max_width {
                break;
            }
            kept.push(ch);
            width += ch_width;
        }
        if !kept.is_empty() {
            spans.push(Span::styled(kept, style));
        }
        spans.push(Span::styled("\u{2026}", style));
        break;
    }
    Line::from(spans)
}

/// One-row find / go-to-message bar, drawn directly above the prompt.
///
/// The match count comes from `find_match_rows`, which the transcript render
/// filled earlier in the same frame, so the counter can never describe a
/// different wrap than the highlight the user is looking at.
fn render_find_bar(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let match_count = app.find_match_rows.borrow().len();
    let label = app.transcript_find.label(match_count);
    let hint = match app.transcript_find.mode {
        crate::transcript_find::FindMode::Search => "  Enter/F3 next · Shift+F3 prev · Esc close",
        crate::transcript_find::FindMode::GoToMessage => "  Enter go · Esc close",
    };

    let line = Line::from(vec![
        Span::styled("  ".to_string(), Style::default()),
        Span::styled(
            label,
            Style::default()
                .fg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(hint.to_string(), Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(vec![line]), area);
}

fn render_prompt_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    let suggestions = &app.prompt_input.suggestions;
    if suggestions.is_empty() || area.height == 0 {
        return;
    }

    let selected = app.prompt_input.suggestion_index.unwrap_or(0);
    let max_visible = area.height as usize;
    let start = selected
        .saturating_sub(max_visible / 2)
        .min(suggestions.len().saturating_sub(max_visible));
    let end = (start + max_visible).min(suggestions.len());
    let label_width = area.width.saturating_div(3).max(12) as usize;

    for (row, suggestion) in suggestions[start..end].iter().enumerate() {
        let is_selected = start + row == selected;
        let accent_style = if is_selected {
            Style::default()
                .fg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let label_style = if is_selected {
            Style::default()
                .fg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let detail_style = if is_selected {
            Style::default().fg(CLAUDE_ORANGE)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![Span::styled(
            if is_selected { "\u{203a} " } else { "  " },
            accent_style,
        )];
        match suggestion.source {
            TypeaheadSource::SlashCommand => {
                let display_name = truncate_text(&suggestion.text, label_width);
                spans.push(Span::styled(
                    format!("{display_name:<width$}", width = label_width),
                    label_style,
                ));
                spans.push(Span::styled(
                    " [cmd] ",
                    Style::default().fg(Color::DarkGray),
                ));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(
                        truncate_text(
                            &suggestion.description,
                            area.width.saturating_sub(label_width as u16 + 10) as usize,
                        ),
                        detail_style,
                    ));
                }
            }
            TypeaheadSource::FileRef => {
                spans.push(Span::styled("+ ", accent_style));
                spans.push(Span::styled(
                    truncate_middle(&suggestion.text, label_width),
                    label_style,
                ));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(
                        " \u{2014} ",
                        Style::default().fg(Color::DarkGray),
                    ));
                    spans.push(Span::styled(
                        truncate_text(&suggestion.description, area.width as usize / 2),
                        detail_style,
                    ));
                }
            }
            TypeaheadSource::History => {
                let display_name = truncate_text(&suggestion.text, label_width);
                spans.push(Span::styled(
                    format!("{display_name:<width$}", width = label_width),
                    label_style,
                ));
                spans.push(Span::styled(
                    " [history] ",
                    Style::default().fg(Color::DarkGray),
                ));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(
                        truncate_text(&suggestion.description, area.width as usize / 2),
                        detail_style,
                    ));
                }
            }
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: area.x,
                y: area.y + row as u16,
                width: area.width,
                height: 1,
            },
        );
    }
}

// -----------------------------------------------------------------------
// Legacy simple help overlay (fallback when help_overlay is not open)
// -----------------------------------------------------------------------

fn render_simple_help_overlay(frame: &mut Frame, area: Rect) {
    let help_width = 50u16.min(area.width.saturating_sub(4));
    let help_height = 20u16.min(area.height.saturating_sub(4));
    let help_area = crate::overlays::centered_rect(help_width, help_height, area);

    frame.render_widget(Clear, help_area);

    let lines = vec![
        Line::from(vec![Span::styled(
            " Key Bindings",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )]),
        Line::from(""),
        kb_line("Enter", "Submit message"),
        kb_line("Ctrl+C", "Cancel streaming / Quit"),
        kb_line("Ctrl+D", "Quit (empty input)"),
        kb_line("Up / Down", "Navigate input history"),
        kb_line("Ctrl+R", "Search input history"),
        kb_line("PageUp / PageDown", "Scroll messages"),
        kb_line("F1 / ?", "Toggle this help"),
        Line::from(""),
        Line::from(vec![Span::styled(
            " Permission Dialog",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )]),
        Line::from(""),
        kb_line("1 / 2 / 3", "Select option"),
        kb_line("y / a / n", "Allow / Always / Deny"),
        kb_line("Enter", "Confirm selection"),
        kb_line("Esc", "Deny (close dialog)"),
        Line::from(""),
        Line::from(vec![Span::styled(
            " press F1 or ? to close ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Left);
    frame.render_widget(para, help_area);
}

fn kb_line<'a>(key: &str, desc: &str) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<20}", key),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

// -----------------------------------------------------------------------
// Legacy history search overlay (used when history_search_overlay is not open)
// -----------------------------------------------------------------------

fn render_legacy_history_search(
    frame: &mut Frame,
    hs: &crate::app::HistorySearch,
    app: &App,
    area: Rect,
) {
    let dialog_width = 60u16.min(area.width.saturating_sub(4));
    let visible_matches = 8usize;
    let dialog_height = (4 + visible_matches.min(hs.matches.len().max(1)) as u16)
        .min(area.height.saturating_sub(4));
    let dialog_area = crate::overlays::centered_rect(dialog_width, dialog_height, area);

    frame.render_widget(Clear, dialog_area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::raw("  Search: "),
        Span::styled(
            hs.query.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2588}", Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(""));

    if hs.matches.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        let start = hs.selected.saturating_sub(visible_matches / 2);
        let end = (start + visible_matches).min(hs.matches.len());
        let start = end.saturating_sub(visible_matches).min(start);

        for (display_idx, &hist_idx) in hs.matches[start..end].iter().enumerate() {
            let real_idx = start + display_idx;
            let is_selected = real_idx == hs.selected;
            let entry = app
                .prompt_input
                .history
                .get(hist_idx)
                .map(String::as_str)
                .unwrap_or("");

            // truncate_end is width-aware, cuts on char boundaries, and appends
            // its own ellipsis. The old code did `String::truncate` on a raw
            // byte index (panics mid-codepoint) after a `usize` subtraction that
            // could underflow-panic on a narrow terminal (#221).
            let truncated = truncate_end(entry, (dialog_width as usize).saturating_sub(6));

            let (prefix, style) = if is_selected {
                (
                    "  \u{25BA} ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("    ", Style::default().fg(Color::White))
            };
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(truncated, style),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" History Search (Esc to cancel) ")
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, dialog_area);
}

// -----------------------------------------------------------------------
// Complete status line (T2-8)
// -----------------------------------------------------------------------

/// How full a context window looks, as one colour.
///
/// Every surface that colours the window goes through here: the footer, the
/// compact warning beside it, and the `/context` overlay. They each carried
/// their own copy of the two thresholds, and the overlay compared with `>`
/// while the others used `>=`, so a window at exactly 95 % was red in the
/// footer and yellow in the overlay.
pub(crate) fn context_fill_color(fraction: f64) -> Color {
    if fraction >= CONTEXT_CRITICAL_FRACTION {
        Color::Red
    } else if fraction >= CONTEXT_WARNING_FRACTION {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Complete status line data for rendering.
#[derive(Debug, Clone, Default)]
pub struct StatusLineData {
    pub model: String,
    pub tokens_used: u64,
    pub tokens_total: u64,
    pub cost_cents: f64,
    pub compact_warning_pct: Option<f64>, // None = no warning; Some(pct) = show warning
    pub vim_mode: Option<String>,         // None = no vim mode; Some("NORMAL") etc.
    pub bridge_connected: bool,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub agent_badge: Option<String>,
    pub rate_limit_pct_5h: Option<f64>,
    pub rate_limit_pct_7d: Option<f64>,
    /// Goal badge: Some("active · 5m · 3 turns") when a goal is running.
    pub goal_badge: Option<String>,
}

pub fn render_full_status_line(
    data: &StatusLineData,
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
) {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Paragraph, Widget},
    };

    let mut spans = Vec::new();

    // Model name
    if !data.model.is_empty() {
        spans.push(Span::styled(
            format!(" {} ", data.model),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    }

    // Context window
    if data.tokens_total > 0 {
        let pct = data.tokens_used as f64 / data.tokens_total as f64;
        let ctx_color = context_fill_color(pct);
        let used_k = data.tokens_used / 1000;
        let total_k = data.tokens_total / 1000;
        spans.push(Span::styled(
            format!("{}k/{}k ({:.0}%)", used_k, total_k, pct * 100.0),
            Style::default().fg(ctx_color),
        ));
        spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    }

    // Cost
    if data.cost_cents > 0.0 {
        spans.push(Span::styled(
            format!("${:.2}", data.cost_cents / 100.0),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    }

    // Compact warning
    if let Some(pct) = data.compact_warning_pct {
        if pct >= CONTEXT_WARNING_FRACTION {
            let color = context_fill_color(pct);
            spans.push(Span::styled(
                format!("⚠ ctx {:.0}% ", pct * 100.0),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
    }

    // Vim mode
    if let Some(mode) = &data.vim_mode {
        let color = match mode.as_str() {
            "NORMAL" => Color::Green,
            "INSERT" => Color::Blue,
            "VISUAL" => Color::Magenta,
            _ => Color::White,
        };
        spans.push(Span::styled(
            format!("[{}]", mode),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }

    // Agent badge
    if let Some(badge) = &data.agent_badge {
        spans.push(Span::styled(
            format!("[{}]", badge),
            Style::default().fg(Color::Magenta),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }

    // Goal badge
    if let Some(goal) = &data.goal_badge {
        spans.push(Span::styled(
            format!("[goal: {}]", goal),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }

    // Bridge connected
    if data.bridge_connected {
        spans.push(Span::styled(
            "\u{1F517} ",
            Style::default().fg(Color::Green),
        ));
    }

    // Session ID
    if let Some(sid) = &data.session_id {
        let short = &sid[..sid.len().min(8)];
        spans.push(Span::styled(
            format!("[session:{}]", short),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Worktree
    if let Some(wt) = &data.worktree {
        spans.push(Span::styled(
            format!("[worktree:{}]", wt),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let line = Line::from(spans);
    Paragraph::new(line)
        .style(Style::default().bg(Color::Black))
        .render(area, buf);
}

// ---------------------------------------------------------------------------
// Multi-agent UI components
// ---------------------------------------------------------------------------

/// Render a single header line for a teammate's message block.
///
/// Format: `┤ teammate: <id> ├` in magenta, optional `· <session_info>` in dim
///
/// # Arguments
/// * `teammate_id`  — teammate identifier string
/// * `session_info` — optional session info snippet to append
pub fn render_teammate_header(teammate_id: &str, session_info: Option<&str>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            "┤ teammate: ".to_string(),
            Style::default().fg(Color::Magenta),
        ),
        Span::styled(
            teammate_id.to_string(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ├".to_string(), Style::default().fg(Color::Magenta)),
    ];

    if let Some(info) = session_info {
        spans.push(Span::styled(
            "  · ".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            info.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }

    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Tests — tool-block rendering (icon headers, path shortening, todo checklist)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tool_block_tests {
    use super::*;
    use crate::app::{ToolStatus, ToolUseBlock};

    fn block(name: &str, status: ToolStatus, input: &str, preview: Option<&str>) -> ToolUseBlock {
        ToolUseBlock {
            id: "t".into(),
            name: name.into(),
            turn_index: None,
            status,
            output_preview: preview.map(|s| s.to_string()),
            input_json: input.into(),
            live_output: String::new(),
            duration_ms: None,
        }
    }

    /// The three bands, and both boundaries, from the one rule every surface
    /// now reads.
    #[test]
    fn the_context_colour_changes_at_the_shared_thresholds() {
        assert_eq!(context_fill_color(0.0), Color::Green);
        assert_eq!(context_fill_color(0.79), Color::Green);
        // The boundary itself, which is where the overlay used to disagree with
        // the footer.
        assert_eq!(context_fill_color(CONTEXT_WARNING_FRACTION), Color::Yellow);
        assert_eq!(context_fill_color(0.94), Color::Yellow);
        assert_eq!(context_fill_color(CONTEXT_CRITICAL_FRACTION), Color::Red);
        assert_eq!(context_fill_color(1.0), Color::Red);
    }

    /// The auto-compact machinery reports the same window, so the colour and
    /// the warning the model is sent cannot describe different states.
    #[test]
    fn the_warning_state_turns_at_the_same_points_as_the_colour() {
        use mikmik_query::compact::{calculate_token_warning_state_for_window, TokenWarningState};

        // Large enough that `WARNING_THRESHOLD_BUFFER_TOKENS` cannot reach the
        // boundary being measured and answer for it.
        let window = 1_000_000u64;
        let at = |fraction: f64| (window as f64 * fraction) as u64;
        let state = |tokens: u64| calculate_token_warning_state_for_window(tokens, window);

        // Both sides of each boundary. Checking the boundary alone only catches
        // a threshold that drifted upwards.
        assert_eq!(
            state(at(CONTEXT_WARNING_FRACTION) - 1),
            TokenWarningState::Ok
        );
        assert_eq!(
            state(at(CONTEXT_WARNING_FRACTION)),
            TokenWarningState::Warning
        );
        assert_eq!(
            state(at(CONTEXT_CRITICAL_FRACTION) - 1),
            TokenWarningState::Warning
        );
        assert_eq!(
            state(at(CONTEXT_CRITICAL_FRACTION)),
            TokenWarningState::Critical
        );
    }

    fn palette() -> crate::theme_colors::ColorPalette {
        crate::theme_colors::ColorPalette::for_theme("default")
    }

    fn render(b: &ToolUseBlock) -> Vec<String> {
        let mut lines = Vec::new();
        render_tool_block_lines(&mut lines, b, 0, None, &palette(), 80, false);
        lines.iter().map(flatten_line_text).collect()
    }

    fn render_with_advisor(b: &ToolUseBlock, model: &str) -> Vec<String> {
        let mut lines = Vec::new();
        render_tool_block_lines(&mut lines, b, 0, Some(model), &palette(), 80, false);
        lines.iter().map(flatten_line_text).collect()
    }

    /// Render `b` with `showToolDuration` on, at `width`.
    fn render_timed(b: &ToolUseBlock, width: u16) -> Vec<String> {
        let mut lines = Vec::new();
        render_tool_block_lines(&mut lines, b, 0, None, &palette(), width, true);
        lines.iter().map(flatten_line_text).collect()
    }

    fn finished_block(duration_ms: Option<u64>) -> ToolUseBlock {
        let mut b = block(
            "Bash",
            ToolStatus::Done,
            r#"{"command":"cargo check"}"#,
            Some("Finished in 17s"),
        );
        b.duration_ms = duration_ms;
        b
    }

    #[test]
    fn a_finished_tool_reports_how_long_it_took_at_the_right_edge() {
        let lines = render_timed(&finished_block(Some(17_400)), 40);
        let last = lines.last().expect("the block has lines");
        assert_eq!(last.trim(), "17.4s", "{lines:?}");
        assert_eq!(
            last.width(),
            40,
            "the label must sit at the pane's right edge: {last:?}"
        );
    }

    #[test]
    fn the_duration_is_absent_while_the_setting_is_off() {
        let with = render_timed(&finished_block(Some(17_400)), 40);
        let without = render(&finished_block(Some(17_400)));
        assert_eq!(
            without.len(),
            with.len() - 1,
            "off must draw exactly the same block minus the duration line"
        );
        assert!(
            !without.iter().any(|line| line.contains("17.4s")),
            "{without:?}"
        );
    }

    #[test]
    fn a_running_tool_reports_no_duration() {
        // It has not finished, so there is no number yet and a line saying so
        // would be an empty row that appears and disappears mid-turn.
        let mut running = finished_block(None);
        running.status = ToolStatus::Running;
        running.output_preview = None;
        let lines = render_timed(&running, 40);
        assert!(
            !lines.iter().any(|line| line.trim().ends_with('s')
                && line
                    .trim()
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())),
            "{lines:?}"
        );
    }

    #[test]
    fn the_duration_reaches_the_drawn_screens_right_edge() {
        // The line tests measure a `Line` built at a width the test chose. This
        // one measures the cells a terminal actually shows, which is the only
        // place a wrong width shows up as a label floating mid-pane.
        use crate::app::App;
        use mikmik_core::config::Config;
        use mikmik_core::cost::CostTracker;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(Config::default(), CostTracker::new());
        app.settings_screen.show_tool_duration = true;
        app.handle_query_event(mikmik_query::QueryEvent::ToolStart {
            tool_name: "Bash".to_string(),
            tool_id: "t1".to_string(),
            input_json: r#"{"command":"cargo check"}"#.to_string(),
        });
        app.handle_query_event(mikmik_query::QueryEvent::ToolEnd {
            tool_name: "Bash".to_string(),
            tool_id: "t1".to_string(),
            result: "Finished".to_string(),
            is_error: false,
            duration_ms: Some(17_400),
        });

        let width = 60u16;
        let mut terminal = match Terminal::new(TestBackend::new(width, 24)) {
            Ok(terminal) => terminal,
            Err(err) => panic!("test terminal: {err}"),
        };
        if let Err(err) = terminal.draw(|frame| render_app(frame, &app)) {
            panic!("draw: {err}");
        }
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();

        let row = rows
            .iter()
            .find(|row| row.contains("17.4s"))
            .unwrap_or_else(|| panic!("no row carries the duration: {rows:#?}"));
        assert!(
            row.ends_with("17.4s"),
            "the duration must reach the pane's last column rather than sit \
             anywhere left of it: {row:?}"
        );
    }

    #[test]
    fn a_duration_wider_than_the_pane_does_not_overflow_it() {
        // `saturating_sub` on the padding: a pane narrower than the label must
        // print the label rather than panicking or wrapping the line.
        let lines = render_timed(&finished_block(Some(3_725_000)), 4);
        let last = lines.last().expect("the block has lines");
        assert_eq!(last.trim(), "62m05s", "{lines:?}");
    }

    #[test]
    fn a_one_line_language_server_answer_stays_on_the_header() {
        // "Nothing found" is the ordinary answer to half of what a language
        // server is asked, and three lines to say it buries the transcript.
        let lines = render(&block(
            "LSP",
            ToolStatus::Done,
            r#"{"action":"definition","file":"src/a.rs","symbol":"foo","line":3}"#,
            Some("No definition found for foo"),
        ));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("definition foo (a.rs:3)"), "{lines:?}");
        assert!(
            lines[0].contains("No definition found for foo"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_longer_language_server_answer_keeps_its_own_lines() {
        let lines = render(&block(
            "LSP",
            ToolStatus::Done,
            r#"{"action":"references","file":"src/a.rs","symbol":"foo","line":3}"#,
            Some("src/a.rs:3:5\nsrc/b.rs:9:1"),
        ));
        assert!(lines.len() > 2, "{lines:?}");
        assert!(
            lines.iter().any(|line| line.contains("src/b.rs:9:1")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_failed_language_server_call_keeps_its_own_line() {
        // An error is the one result worth the room.
        let lines = render(&block(
            "LSP",
            ToolStatus::Error,
            r#"{"action":"hover","file":"src/a.rs","line":3}"#,
            Some("no server"),
        ));
        assert!(lines.len() > 1, "{lines:?}");
    }

    #[test]
    fn another_tool_keeps_its_output_below_the_rule() {
        let lines = render(&block(
            "Bash",
            ToolStatus::Done,
            r#"{"command":"ls"}"#,
            Some("a.txt"),
        ));
        assert!(lines.len() > 1, "{lines:?}");
    }

    #[test]
    fn a_rule_separates_the_command_from_what_it_printed() {
        let with_output = render(&block(
            "Bash",
            ToolStatus::Done,
            r#"{"command":"ls"}"#,
            Some("alpha\nbeta"),
        ));
        let rule_at = with_output
            .iter()
            .position(|line| line.contains(TOOL_OUTPUT_RULE));
        let first_output_at = with_output.iter().position(|line| line.contains("alpha"));
        assert!(rule_at.is_some(), "no rule in {with_output:?}");
        assert!(
            rule_at < first_output_at,
            "the rule must come before the output: {with_output:?}"
        );
        assert!(rule_at > Some(0), "the header comes first: {with_output:?}");
    }

    #[test]
    fn a_block_with_no_output_gets_no_rule() {
        // A rule under a header with nothing beneath it would draw a line to
        // separate the header from the next block.
        let running = render(&block(
            "Bash",
            ToolStatus::Running,
            r#"{"command":"ls"}"#,
            None,
        ));
        assert!(
            !running.iter().any(|line| line.contains(TOOL_OUTPUT_RULE)),
            "{running:?}"
        );
    }

    #[test]
    fn the_error_colour_comes_from_the_theme() {
        // The theme setting used to change nothing on screen; this is the
        // first place it does.
        fn error_accent(theme: &str) -> ratatui::style::Color {
            let mut lines = Vec::new();
            render_tool_block_lines(
                &mut lines,
                &block("Bash", ToolStatus::Error, r#"{"command":"ls"}"#, Some("no")),
                0,
                None,
                &crate::theme_colors::ColorPalette::for_theme(theme),
                80,
                false,
            );
            lines[0].spans[0].style.fg.expect("the header is coloured")
        }

        assert_ne!(
            error_accent("deuteranopia"),
            error_accent("default"),
            "a deuteranopic reader must not get the default red"
        );
    }

    #[test]
    fn a_running_block_draws_what_the_command_has_printed_so_far() {
        let mut b = block("Bash", ToolStatus::Running, r#"{"command":"make"}"#, None);
        b.push_live_output("compiling one\ncompiling two\n");

        let rendered = render(&b);
        assert!(
            rendered.iter().any(|line| line.contains("compiling two")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains(TOOL_OUTPUT_RULE)),
            "live output needs the same rule as a finished result: {rendered:?}"
        );
    }

    #[test]
    fn only_the_tail_of_a_long_run_stays_on_screen() {
        // A build prints thousands of lines; drawing them all would push the
        // conversation off the screen for as long as it ran.
        let mut b = block("Bash", ToolStatus::Running, r#"{"command":"make"}"#, None);
        for i in 0..(LIVE_OUTPUT_TAIL_LINES * 3) {
            b.push_live_output(&format!("line {i}\n"));
        }

        let rendered = render(&b);
        assert!(
            rendered.iter().any(|line| line.contains("line 29")),
            "the newest line must be shown: {rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("line 0")),
            "the oldest line must have scrolled off: {rendered:?}"
        );
    }

    #[test]
    fn a_finished_block_shows_the_result_rather_than_the_play_by_play() {
        let mut b = block(
            "Bash",
            ToolStatus::Done,
            r#"{"command":"make"}"#,
            Some("done"),
        );
        b.push_live_output("compiling one\n");

        let rendered = render(&b).join("\n");
        assert!(rendered.contains("done"), "{rendered:?}");
        assert!(
            !rendered.contains("compiling one"),
            "a finished block keeps the result, not the steps: {rendered:?}"
        );
    }

    #[test]
    fn advisor_block_collapses_to_a_status_line() {
        let input = r#"{"question":"is this refactor safe?"}"#;

        let running = render_with_advisor(
            &block("Advisor", ToolStatus::Running, input, None),
            "claude-opus-4-6",
        )
        .join("\n");
        assert!(running.contains("Advising"), "got {running:?}");
        assert!(running.contains("claude-opus-4-6"), "got {running:?}");

        let done = render_with_advisor(
            &block("Advisor", ToolStatus::Done, input, Some("looks fine")),
            "claude-opus-4-6",
        )
        .join("\n");
        assert!(done.contains("Advisor reviewed"), "got {done:?}");

        // The question is echoed by neither state: the advice lands in the
        // tool result, so repeating the prompt here would duplicate it.
        assert!(
            !running.contains("is this refactor safe?"),
            "got {running:?}"
        );
        assert!(!done.contains("is this refactor safe?"), "got {done:?}");
    }

    #[test]
    fn advisor_block_omits_the_model_when_unknown() {
        let rendered = render(&block(
            "Advisor",
            ToolStatus::Done,
            r#"{"question":"q"}"#,
            None,
        ))
        .join("\n");
        assert!(rendered.contains("Advisor reviewed"), "got {rendered:?}");
        assert!(!rendered.contains('('), "no empty parens, got {rendered:?}");
    }

    #[test]
    fn icons_are_per_tool_and_ascii() {
        assert_eq!(tool_icon("bash"), "$");
        assert_eq!(tool_icon("read"), "<");
        assert_eq!(tool_icon("write"), ">");
        assert_eq!(tool_icon("glob"), "*");
        assert_eq!(tool_icon("grep"), "/");
        assert_eq!(tool_icon("todowrite"), ":");
        assert_eq!(tool_icon("something-unknown"), "~");
        // All markers must be single-byte ASCII (guaranteed one terminal cell).
        for t in [
            "bash",
            "read",
            "write",
            "glob",
            "grep",
            "webfetch",
            "websearch",
            "todo",
            "task",
            "lsp",
            "x",
        ] {
            let icon = tool_icon(t);
            assert_eq!(icon.len(), 1, "{t} icon {icon:?} must be 1 ASCII byte");
            assert!(icon.is_ascii(), "{t} icon {icon:?} must be ASCII");
        }
    }

    #[test]
    fn shorten_home_replaces_prefix() {
        if let Some(home) = dirs::home_dir() {
            let p = home.join("projects").join("x.yaml");
            let shortened = shorten_home_path(&p.to_string_lossy());
            assert!(shortened.starts_with("~"), "got {shortened:?}");
            assert!(shortened.ends_with("x.yaml"));
            assert!(!shortened.contains(home.to_string_lossy().as_ref()));
        }
        // A non-home path is left untouched.
        assert_eq!(shorten_home_path("/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn bash_header_is_icon_led_and_not_duplicated() {
        let b = block(
            "bash",
            ToolStatus::Done,
            r#"{"command":"python3 - <<'PY'\nfrom pathlib import Path"}"#,
            Some("218183\nMarketing Outbound OS"),
        );
        let lines = render(&b);
        // Header: "$ python3 - <<'PY'"
        assert!(
            lines[0].contains('$'),
            "header should be icon-led: {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("python3 - <<'PY'"),
            "header shows command: {:?}",
            lines[0]
        );
        // The command must appear exactly once (no summary + $-line duplication).
        let joined = lines.join("\n");
        assert_eq!(
            joined.matches("python3 - <<'PY'").count(),
            1,
            "no dup: {joined:?}"
        );
        // Output preview still shown.
        assert!(joined.contains("218183"));
    }

    #[test]
    fn read_header_shortens_home_path() {
        if let Some(home) = dirs::home_dir() {
            let path = home.join("FOLLOWUPS.md");
            let input = serde_json::json!({
                "file_path": path.to_string_lossy().to_string(),
            })
            .to_string();
            let b = block("read", ToolStatus::Done, &input, None);
            let lines = render(&b);
            assert!(lines[0].contains('<'), "read icon: {:?}", lines[0]);
            assert!(lines[0].contains('~'), "home shortened: {:?}", lines[0]);
            assert!(!lines[0].contains(home.to_string_lossy().as_ref()));
        }
    }

    #[test]
    fn todo_renders_checklist_with_glyphs_and_counts() {
        let b = block(
            "TodoWrite",
            ToolStatus::Done,
            r#"{"todos":[
                {"content":"Locate files","status":"completed"},
                {"content":"Build importer","status":"in_progress"},
                {"content":"Wire adapter","status":"pending"}
            ]}"#,
            Some("Todo list updated (3 total)"),
        );
        let lines = render(&b);
        let joined = lines.join("\n");
        // Header shows count, not the raw "Todo list updated (...)".
        assert!(joined.contains("Todos"), "{joined:?}");
        assert!(joined.contains("1/3 done"), "{joined:?}");
        // Each status has its ASCII checkbox + content.
        assert!(
            joined.contains("[x] Locate files"),
            "done marker: {joined:?}"
        );
        assert!(
            joined.contains("[>] Build importer"),
            "in-progress marker: {joined:?}"
        );
        assert!(
            joined.contains("[ ] Wire adapter"),
            "pending marker: {joined:?}"
        );
        // The raw result-preview string must NOT leak into the checklist view.
        assert!(
            !joined.contains("Todo list updated"),
            "preview suppressed: {joined:?}"
        );
        // A list with no scores must look exactly as it did before.
        assert!(
            !joined.contains('%'),
            "no score, no percentages: {joined:?}"
        );
    }

    #[test]
    fn todo_checklist_shows_confidence_when_present() {
        let b = block(
            "TodoWrite",
            ToolStatus::Done,
            r#"{"todos":[
                {"content":"Locate files","status":"completed","completion_confidence":90},
                {"content":"Build importer","status":"in_progress","confidence":70},
                {"content":"Wire adapter","status":"pending","confidence":50}
            ]}"#,
            Some("Todo list updated (3 total)"),
        );
        let joined = render(&b).join("\n");
        assert!(joined.contains("[x] Locate files [90%]"), "{joined:?}");
        assert!(joined.contains("[>] Build importer [70%]"), "{joined:?}");
        assert!(joined.contains("[ ] Wire adapter [50%]"), "{joined:?}");
        assert!(joined.contains("confidence 70%"), "aggregate: {joined:?}");
    }

    #[test]
    fn a_completed_item_falls_back_to_its_up_front_score() {
        let b = block(
            "TodoWrite",
            ToolStatus::Done,
            r#"{"todos":[{"content":"Locate files","status":"completed","confidence":60}]}"#,
            None,
        );
        assert!(render(&b).join("\n").contains("[60%]"));
    }

    #[test]
    fn priority_weights_the_aggregate_score() {
        // Without weighting this would read 60%; the high-priority item counts
        // three times, so the shaky task dominates.
        let b = block(
            "TodoWrite",
            ToolStatus::Done,
            r#"{"todos":[
                {"content":"Risky","status":"pending","confidence":20,"priority":"high"},
                {"content":"Easy","status":"pending","confidence":100,"priority":"low"}
            ]}"#,
            None,
        );
        assert!(render(&b).join("\n").contains("confidence 40%"));
    }

    #[test]
    fn an_unscored_item_is_skipped_rather_than_counted_as_zero() {
        let b = block(
            "TodoWrite",
            ToolStatus::Done,
            r#"{"todos":[
                {"content":"Scored","status":"pending","confidence":80},
                {"content":"Unscored","status":"pending"}
            ]}"#,
            None,
        );
        let joined = render(&b).join("\n");
        assert!(joined.contains("confidence 80%"), "{joined:?}");
        assert!(
            joined.contains("[ ] Unscored\n") || joined.ends_with("[ ] Unscored"),
            "{joined:?}"
        );
    }

    #[test]
    fn legacy_history_search_narrow_multibyte_no_panic() {
        use crate::app::{App, HistorySearch};
        use mikmik_core::config::Config;
        use mikmik_core::cost::CostTracker;
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = App::new(Config::default(), CostTracker::new());
        app.prompt_input.history = vec!["\u{4f60}\u{597d}\u{4e16}\u{754c}".repeat(6)]; // wide CJK
        let mut hs = HistorySearch::new();
        hs.matches = vec![0];

        // width 10 -> dialog_width 6 -> `dialog_width - 9` underflow-panicked
        // pre-fix, and `String::truncate` on a byte index sliced the CJK entry
        // mid-codepoint (#221). No panic == pass.
        let mut terminal = Terminal::new(TestBackend::new(10, 12)).unwrap();
        terminal
            .draw(|frame| render_legacy_history_search(frame, &hs, &app, frame.area()))
            .unwrap();
    }
}

/// Tests for the streaming transcript cache (issue #222): the committed prefix
/// must be reused across streaming deltas, and streaming output must be
/// byte-identical to a full (non-cached) rebuild.
#[cfg(test)]
mod stream_cache_tests {
    use super::*;
    use crate::app::App;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use mikmik_core::types::Message;

    const WIDTH: u16 = 80;

    fn test_app() -> App {
        App::new(Config::default(), CostTracker::new())
    }

    /// A per-item signature that captures the rendered spans+styles (via Debug)
    /// plus all metadata, so equality means byte-identical rendering.
    fn item_sig(item: &RenderedLineItem) -> (String, bool, Option<usize>, Option<u64>) {
        (
            format!("{:?}", item.line),
            item.is_header,
            item.message_index,
            item.thinking_hash,
        )
    }

    fn sigs(items: &[RenderedLineItem]) -> Vec<(String, bool, Option<usize>, Option<u64>)> {
        items.iter().map(item_sig).collect()
    }

    fn joined_text(items: &[RenderedLineItem]) -> String {
        items
            .iter()
            .map(|i| i.search_text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The completed-message items are reused (served from cache) across a
    /// streaming delta, while the live tail updates.
    #[test]
    fn completed_prefix_reused_across_streaming_delta() {
        let mut app = test_app();
        // Turn 0 is fully committed; turn 1 is the live/streaming turn.
        app.messages.push(Message::user("user one prompt"));
        app.messages
            .push(Message::assistant("assistant one committed reply"));
        app.messages.push(Message::user("user two prompt"));
        app.is_streaming = true;
        app.streaming_text = "streaming tail alpha".to_string();

        reset_render_caches();

        // First render: prefix is built fresh (a miss).
        let render1 = render_message_items(&app, WIDTH);
        assert_eq!(
            prefix_cache_counts(),
            (0, 1),
            "first render builds the prefix"
        );

        // A streaming delta arrives: only the live text grows. Real code bumps
        // transcript_version on every delta — assert that does NOT evict the
        // committed-prefix entry.
        app.streaming_text.push_str(" beta");
        app.invalidate_transcript();

        let render2 = render_message_items(&app, WIDTH);
        let (hits, misses) = prefix_cache_counts();
        assert_eq!(
            (hits, misses),
            (1, 1),
            "committed prefix served from cache after the delta (no rebuild)"
        );

        // The committed content is identical in both renders and appears before
        // the live tail diverges.
        let sig1 = sigs(&render1);
        let sig2 = sigs(&render2);
        let common = sig1
            .iter()
            .zip(sig2.iter())
            .take_while(|(a, b)| a == b)
            .count();
        assert!(common > 0, "some leading items must be identical");
        let leading_text = joined_text(&render1[..common]);
        assert!(
            leading_text.contains("user one prompt")
                && leading_text.contains("assistant one committed reply"),
            "the reused prefix contains the whole committed turn: {leading_text:?}"
        );
        // The reused prefix must not contain any live tail content.
        assert!(
            !leading_text.contains("streaming tail alpha"),
            "prefix must not include the live tail: {leading_text:?}"
        );

        // The live tail updated between renders.
        let text1 = joined_text(&render1);
        let text2 = joined_text(&render2);
        assert!(text1.contains("streaming tail alpha"));
        assert!(!text1.contains("streaming tail alpha beta"));
        assert!(
            text2.contains("streaming tail alpha beta"),
            "tail rebuilt with the delta"
        );
    }

    /// Streaming render (cached prefix + rebuilt tail) is byte-identical to a
    /// full rebuild for a multi-message transcript — no ghosting, no missing or
    /// stale content — both on the first (cold) frame and after a delta (warm).
    #[test]
    fn streaming_render_matches_full_rebuild() {
        let mut app = test_app();
        app.messages.push(Message::user("first user question"));
        app.messages.push(Message::assistant(
            "first assistant answer with **markdown**",
        ));
        app.messages.push(Message::user("second user question"));
        app.messages
            .push(Message::assistant("second assistant answer"));
        app.messages.push(Message::user("third user question"));
        app.is_streaming = true;
        app.streaming_thinking = "pondering the third answer".to_string();
        app.streaming_text = "third answer so far".to_string();

        reset_render_caches();

        // Cold frame: streaming path vs a direct full rebuild.
        let streamed_cold = render_message_items(&app, WIDTH);
        let full_cold = build_all_items(&app, WIDTH);
        assert_eq!(
            sigs(&streamed_cold),
            sigs(&full_cold),
            "cold streaming render must match a full rebuild"
        );

        // Warm frame: after a delta, the prefix is served from cache but the
        // concatenation must still equal a full rebuild.
        app.streaming_text.push_str(" plus more tokens");
        app.invalidate_transcript();
        let streamed_warm = render_message_items(&app, WIDTH);
        let (hits, _) = prefix_cache_counts();
        assert!(hits >= 1, "warm frame served the prefix from cache");
        let full_warm = build_all_items(&app, WIDTH);
        assert_eq!(
            sigs(&streamed_warm),
            sigs(&full_warm),
            "warm streaming render must match a full rebuild"
        );
    }

    /// Swapping the transcript (session switch / fork / revert / compaction)
    /// must NOT serve a stale committed prefix, even mid-stream.
    #[test]
    fn transcript_swap_does_not_ghost_stale_prefix() {
        let mut app = test_app();
        app.messages.push(Message::user("session A user"));
        app.messages
            .push(Message::assistant("session A assistant reply"));
        app.messages.push(Message::user("session A live turn"));
        app.is_streaming = true;
        app.streaming_text = "A tail".to_string();

        reset_render_caches();
        let render_a = render_message_items(&app, WIDTH);
        assert!(joined_text(&render_a).contains("session A assistant reply"));

        // Swap in a different transcript (new Vec) while still streaming. The
        // prefix cache must be re-keyed by identity, so no session-A content
        // leaks through.
        app.messages = vec![
            Message::user("session B user"),
            Message::assistant("session B assistant reply"),
            Message::user("session B live turn"),
        ];
        app.streaming_text = "B tail".to_string();
        app.invalidate_transcript();

        let render_b = render_message_items(&app, WIDTH);
        let text_b = joined_text(&render_b);
        assert!(
            text_b.contains("session B assistant reply"),
            "shows swapped content"
        );
        assert!(
            !text_b.contains("session A"),
            "no stale session-A content ghosts through: {text_b:?}"
        );
        // And the swapped render equals a full rebuild.
        assert_eq!(sigs(&render_b), sigs(&build_all_items(&app, WIDTH)));
    }

    /// The last message toggling streaming -> completed moves cleanly into the
    /// cached (non-streaming) set with identical content.
    #[test]
    fn streaming_to_completed_transition_is_clean() {
        let mut app = test_app();
        app.messages.push(Message::user("q1"));
        app.messages.push(Message::assistant("a1 committed"));
        app.messages.push(Message::user("q2"));
        app.is_streaming = true;
        app.streaming_text = "live answer body".to_string();

        reset_render_caches();
        let _streaming = render_message_items(&app, WIDTH);

        // Commit the streamed message (as flush_streamed_assistant_message would)
        // and end streaming.
        app.messages.push(Message::assistant("live answer body"));
        app.is_streaming = false;
        app.streaming_text.clear();
        app.invalidate_transcript();

        let completed = render_message_items(&app, WIDTH);
        // Non-streaming render equals a full rebuild (correct committed set).
        assert_eq!(sigs(&completed), sigs(&build_all_items(&app, WIDTH)));
        let text = joined_text(&completed);
        assert!(text.contains("a1 committed"));
        assert!(text.contains("live answer body"));
    }
}

/// The `/effort` selector docks into the prompt area and replaces the prompt box
/// while open (issue #275).
#[cfg(test)]
mod effort_dock_tests {
    use super::*;
    use crate::app::App;
    use crate::model_picker::EffortLevel;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};

    /// The prompt pointer glyph drawn by `render_prompt_input`.
    const PROMPT_POINTER: char = '\u{276f}';

    fn render_screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render_app(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
        }
        out
    }

    /// The compaction boundary reaches the screen, not just `App` state.
    ///
    /// A user whose history is replaced mid-session has to be able to see
    /// where that happened; the transcript is otherwise indistinguishable
    /// from one where the earlier turns were simply never sent.
    #[test]
    fn a_compaction_boundary_is_drawn_in_the_transcript() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.push_message(mikmik_core::types::Message::user("do the thing"));
        app.push_message(mikmik_core::types::Message::assistant("done"));

        let before = render_screen(&app);
        assert!(!before.contains("Compacted"));

        app.handle_query_event(mikmik_query::QueryEvent::Compacted {
            messages_before: 40,
            messages_after: 6,
            tokens_after: 18_000,
        });

        let after = render_screen(&app);
        assert!(
            after.contains("Compacted 34 message"),
            "the boundary is on screen, not only in App state"
        );
    }

    #[test]
    fn effort_picker_replaces_prompt_box_when_open() {
        let mut app = App::new(Config::default(), CostTracker::new());

        // Closed: the prompt box (its pointer) is drawn; no selector chrome.
        let closed = render_screen(&app);
        assert!(
            closed.contains(PROMPT_POINTER),
            "prompt pointer should be visible when the picker is closed"
        );
        assert!(
            !closed.contains("ultracode"),
            "selector labels must not show while the picker is closed"
        );

        // Open: the selector takes over the prompt area; the prompt box is gone.
        app.effort_picker.open(
            EffortLevel::High,
            vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
                EffortLevel::XHigh,
                EffortLevel::Max,
                EffortLevel::Ultracode,
            ],
        );
        let open = render_screen(&app);
        assert!(
            open.contains("Effort") && open.contains("ultracode"),
            "the docked Effort selector should render in the prompt area"
        );
        assert!(
            !open.contains(PROMPT_POINTER),
            "prompt input must NOT be drawn while the picker is open"
        );
    }
}

// ---------------------------------------------------------------------------
// Find-in-transcript
// ---------------------------------------------------------------------------

#[cfg(test)]
mod find_bar_tests {
    use super::*;
    use crate::app::App;
    use crate::transcript_find::FindMode;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use mikmik_core::types::Message;
    use ratatui::{backend::TestBackend, Terminal};

    fn render_screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render_app(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn app_with_transcript() -> App {
        let mut app = App::new(Config::default(), CostTracker::new());
        for i in 0..12 {
            app.messages.push(Message::user(format!("question {i}")));
            app.messages
                .push(Message::assistant(format!("answer about apples {i}")));
        }
        app
    }

    #[test]
    fn the_bar_draws_only_while_it_is_open() {
        let mut app = app_with_transcript();
        assert!(!render_screen(&app).contains("Find:"));

        app.transcript_find.open(FindMode::Search);
        assert!(render_screen(&app).contains("Find:"));

        app.transcript_find.open(FindMode::GoToMessage);
        let screen = render_screen(&app);
        assert!(screen.contains("Go to message #"));
        assert!(!screen.contains("Find:"));
    }

    /// The renderer is what knows how the transcript wraps, so it is what
    /// works out which rows match. Nothing is collected until a query is live.
    #[test]
    fn the_render_pass_reports_the_matching_rows() {
        let mut app = app_with_transcript();
        render_screen(&app);
        assert!(app.find_match_rows.borrow().is_empty());

        app.transcript_find.open(FindMode::Search);
        app.transcript_find.push_char('a');
        app.transcript_find.push_char('p');
        app.transcript_find.push_char('p');
        render_screen(&app);

        let rows = app.find_match_rows.borrow().clone();
        assert_eq!(rows.len(), 12, "one row per assistant answer");
        assert!(rows.windows(2).all(|w| w[0] < w[1]), "rows must ascend");

        // A query nothing matches leaves an empty list rather than a stale one.
        app.transcript_find.push_char('z');
        render_screen(&app);
        assert!(app.find_match_rows.borrow().is_empty());
    }

    /// `goToLine` needs a row for a message the viewport is nowhere near, which
    /// `message_row_map` (visible rows only) cannot give.
    #[test]
    fn the_render_pass_reports_the_first_row_of_every_message() {
        let app = app_with_transcript();
        render_screen(&app);

        let first_rows = app.message_first_row.borrow();
        assert_eq!(first_rows.len(), app.messages.len());

        // A later message starts further down, and no two share a first row.
        let rows: Vec<usize> = (0..app.messages.len())
            .map(|i| *first_rows.get(&i).expect("every message has a row"))
            .collect();
        assert!(
            rows.windows(2).all(|w| w[0] < w[1]),
            "message rows must ascend with message index: {rows:?}"
        );
    }

    /// While the bar is docked it takes typing, so a query cannot leak into
    /// the prompt the user is composing.
    #[test]
    fn the_bar_takes_typing_instead_of_the_prompt() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let press = |c: char| KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        let mut app = app_with_transcript();
        app.transcript_find.open(FindMode::Search);

        app.handle_key_event(press('a'));
        app.handle_key_event(press('p'));

        assert_eq!(app.transcript_find.query, "ap");
        assert!(
            app.prompt_input.text.is_empty(),
            "the query landed in the prompt"
        );

        // A paste burst belongs to the query too, not to the prompt behind it.
        app.handle_paste_data("ple pie\nsecond line".to_string());
        assert_eq!(app.transcript_find.query, "apple pie");
        assert!(app.prompt_input.text.is_empty());

        app.handle_key_event(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(!app.transcript_find.visible);

        // Closed again, the same keys go back to the prompt.
        app.handle_key_event(press('a'));
        assert_eq!(app.prompt_input.text, "a");
    }

    /// Message numbers are 1-based on screen. A number past the end names no
    /// message, so the bar stays open with a note instead of jumping somewhere.
    #[test]
    fn go_to_message_scrolls_and_refuses_a_number_with_no_message() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let press = |code: KeyCode| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        let mut app = app_with_transcript();
        render_screen(&app);
        let max_scroll = app.last_max_scroll.get();
        let third_row = *app.message_first_row.borrow().get(&2).unwrap();

        app.transcript_find.open(FindMode::GoToMessage);
        app.handle_key_event(press(KeyCode::Char('3')));
        app.handle_key_event(press(KeyCode::Enter));

        assert!(
            !app.transcript_find.visible,
            "the bar should close on a hit"
        );
        assert_eq!(app.scroll_offset, max_scroll.saturating_sub(third_row));

        app.transcript_find.open(FindMode::GoToMessage);
        app.handle_key_event(press(KeyCode::Char('9')));
        app.handle_key_event(press(KeyCode::Char('9')));
        app.handle_key_event(press(KeyCode::Enter));

        assert!(
            app.transcript_find.visible,
            "the bar should stay open for a correction"
        );
        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("No message #99")));
    }

    #[test]
    fn stepping_scrolls_to_the_match_and_wraps() {
        let mut app = app_with_transcript();
        app.transcript_find.open(FindMode::Search);
        for c in "apples".chars() {
            app.transcript_find.push_char(c);
        }
        render_screen(&app);
        let rows = app.find_match_rows.borrow().clone();
        let max_scroll = app.last_max_scroll.get();

        app.step_find_match(true);
        assert_eq!(app.transcript_find.current, Some(0));
        assert_eq!(app.scroll_offset, max_scroll.saturating_sub(rows[0]));
        assert!(!app.auto_scroll, "stepping must stop following the tail");

        app.step_find_match(false);
        assert_eq!(
            app.transcript_find.current,
            Some(rows.len() - 1),
            "stepping back from the first match must wrap to the last"
        );
    }
}

// ---------------------------------------------------------------------------
// MikMik on the welcome screen
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mikmik_welcome_tests {
    use super::*;
    use crate::app::App;
    use crate::mikmik::MikMikPose;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};

    fn welcome_rows(pose: MikMikPose) -> Vec<String> {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.mikmik_current_pose = pose;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render_app(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_welcome_screen_shows_the_cat_above_its_name() {
        let rows = welcome_rows(MikMikPose::Default);
        let row_of = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("not drawn: {needle}"))
        };

        assert!(row_of("/\\_/\\") < row_of("( o.o )"));
        assert!(row_of("( o.o )") < row_of("> ^ <"));

        // The product carries the mascot's name now, so several rows match
        // "MikMik": the banner title, and the greeting when there is no
        // resolvable user name. Search by position instead, because the label
        // the test is about is the row directly under the cat.
        let under_the_cat = &rows[row_of("> ^ <") + 1];
        assert!(
            under_the_cat.contains("MikMik"),
            "the mascot's name should sit directly under it: {under_the_cat:?}"
        );
    }

    #[test]
    fn the_banner_reaches_its_right_edge() {
        // The dash fill is arithmetic over the title's width. It was written
        // against a nine-character title and the rename made it eight, which
        // left the closing corner short of the edge.
        let app = App::new(Config::default(), CostTracker::new());
        for width in [40u16, 80, 120] {
            let lines = welcome_banner_lines(&app, width);
            let top: String = lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(
                UnicodeWidthStr::width(top.as_str()),
                width as usize,
                "top border at width {width}: {top:?}"
            );
        }
    }

    #[test]
    fn the_welcome_box_still_closes_under_the_name() {
        // The cat is three rows where the crab was four, so the name fits
        // without growing WELCOME_BOX_HEIGHT. Guard that it stayed inside.
        let rows = welcome_rows(MikMikPose::Default);
        let name_row = rows
            .iter()
            .position(|r| r.contains("MikMik"))
            .expect("name");
        let border_row = rows
            .iter()
            .position(|r| r.starts_with('\u{256d}') || r.contains('\u{2570}'))
            .expect("the box has a border");
        assert!(name_row < rows.len());
        assert!(
            rows.iter().skip(name_row).any(|r| r.contains('\u{2570}')),
            "the box never closes after the name row"
        );
        let _ = border_row;
    }

    #[test]
    fn every_pose_keeps_the_cat_centred_on_the_same_column() {
        // Each row is MIKMIK_WIDTH wide, so changing pose must not shift it.
        let column_of_ears = |pose: MikMikPose| {
            welcome_rows(pose)
                .iter()
                .find(|row| row.contains("/\\_/\\"))
                .and_then(|row| row.find("/\\_/\\"))
                .expect("the ears are drawn")
        };
        let baseline = column_of_ears(MikMikPose::Default);
        for pose in [
            MikMikPose::Blink,
            MikMikPose::LookLeft,
            MikMikPose::LookRight,
            MikMikPose::LookDown,
            MikMikPose::Loading { frame: 5 },
        ] {
            assert_eq!(column_of_ears(pose.clone()), baseline, "{pose:?} shifted");
        }
    }
}

// ---------------------------------------------------------------------------
// Companion column beside the input box
// ---------------------------------------------------------------------------

#[cfg(test)]
mod companion_tests {
    use super::*;
    use crate::app::App;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};

    const PROMPT_POINTER: char = '\u{276f}';

    fn app_with_companion() -> App {
        let mut app = App::new(Config::default(), CostTracker::new());
        let mut companion = mikmik_buddy::Companion::new("render-test", None);
        companion.soul = Some(mikmik_buddy::CompanionSoul {
            name: "Quackers".to_string(),
            personality: "chaotic, helpful, slightly damp".to_string(),
            hatched_at: chrono::Utc::now(),
        });
        app.companion = Some(companion);
        app
    }

    /// Render the whole screen at a given size and return the row containing
    /// the prompt pointer, plus the full screen text.
    fn render_at(app: &App, width: u16, height: u16) -> (String, Vec<String>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render_app(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut rows = Vec::new();
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    row.push_str(cell.symbol());
                }
            }
            rows.push(row);
        }
        (rows.join("\n"), rows)
    }

    #[test]
    fn a_wide_terminal_puts_the_companion_left_of_the_prompt() {
        let app = app_with_companion();
        let (_, rows) = render_at(&app, 100, 24);

        let prompt_row = rows
            .iter()
            .find(|row| row.contains(PROMPT_POINTER))
            .expect("the prompt box is drawn");
        let pointer_at = prompt_row
            .chars()
            .position(|c| c == PROMPT_POINTER)
            .expect("found above");
        assert!(
            pointer_at >= COMPANION_COLUMN as usize,
            "the prompt should start after the companion column, not at {pointer_at}"
        );
    }

    #[test]
    fn a_narrow_terminal_drops_the_companion_and_returns_the_width() {
        let app = app_with_companion();
        // Below COMPANION_COLUMN + MIN_INPUT_WIDTH.
        let (_, narrow) = render_at(&app, 50, 24);
        let (_, wide) = render_at(&app, 100, 24);

        let pointer_column = |rows: &[String]| {
            rows.iter()
                .find(|row| row.contains(PROMPT_POINTER))
                .and_then(|row| row.chars().position(|c| c == PROMPT_POINTER))
                .expect("the prompt box is drawn")
        };

        let narrow_at = pointer_column(&narrow);
        assert!(
            narrow_at < COMPANION_COLUMN as usize,
            "a narrow terminal must give the width back to the prompt, got {narrow_at}"
        );
        assert!(pointer_column(&wide) > narrow_at);
    }

    #[test]
    fn no_companion_means_no_reserved_column() {
        let with = app_with_companion();
        let without = App::new(Config::default(), CostTracker::new());

        let pointer_column = |app: &App| {
            let (_, rows) = render_at(app, 100, 24);
            rows.iter()
                .find(|row| row.contains(PROMPT_POINTER))
                .and_then(|row| row.chars().position(|c| c == PROMPT_POINTER))
                .expect("the prompt box is drawn")
        };

        assert!(pointer_column(&with) > pointer_column(&without));
    }

    #[test]
    fn a_bubble_takes_a_row_above_the_prompt_without_shrinking_it() {
        let quiet = app_with_companion();
        let mut talking = app_with_companion();
        talking.companion_bubble = Some("you broke it again".to_string());

        let (quiet_screen, quiet_rows) = render_at(&quiet, 100, 24);
        let (talking_screen, talking_rows) = render_at(&talking, 100, 24);

        assert!(!quiet_screen.contains("you broke it again"));
        assert!(talking_screen.contains("you broke it again"));

        // The line sits above the status line, and the prompt box keeps its
        // own row rather than giving one up.
        let row_of = |rows: &[String], needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("not drawn: {needle}"))
        };
        assert!(row_of(&talking_rows, "you broke it again") < row_of(&talking_rows, "BUILD"));
        assert_eq!(
            quiet_rows[row_of(&quiet_rows, "BUILD")],
            talking_rows[row_of(&talking_rows, "BUILD")],
        );
    }

    #[test]
    fn a_multi_line_reply_is_flattened_into_the_single_bubble_row() {
        // The companion is asked for one line; a model that sends three must
        // not push the prompt box around.
        let mut app = app_with_companion();
        app.companion_bubble = Some("first\nsecond\nthird".to_string());
        let (_, rows) = render_at(&app, 100, 24);

        let bubble = rows
            .iter()
            .find(|row| row.contains("first"))
            .expect("the bubble is drawn");
        assert!(bubble.contains("second"), "lines should be joined, not cut");
        assert_eq!(rows.iter().filter(|row| row.contains("first")).count(), 1);
    }

    #[test]
    fn the_companion_does_not_eat_the_status_line() {
        // The status line already truncates the model name at 80 columns.
        // Taking the companion's column out of it too cut "claude-opus-4-6"
        // down to "claude-o", so the companion sits beside the prompt box
        // only, not beside the line above it.
        let with = app_with_companion();
        let without = App::new(Config::default(), CostTracker::new());

        let status_line = |app: &App| {
            let (_, rows) = render_at(app, 80, 24);
            rows.iter()
                .find(|row| row.contains("BUILD"))
                .cloned()
                .expect("the status line is drawn")
        };

        assert_eq!(status_line(&with), status_line(&without));
    }
}

// ---------------------------------------------------------------------------
// Welcome screen: recent activity (issue #277)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod recent_activity_tests {
    use super::*;
    use crate::app::{App, RecentSession};
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::{Duration, SystemTime};

    fn recent(label: &str, secs_ago: u64) -> RecentSession {
        RecentSession {
            label: label.to_string(),
            mtime: SystemTime::now() - Duration::from_secs(secs_ago),
        }
    }

    fn lines_text(recent: &[RecentSession], width: usize) -> Vec<String> {
        recent_activity_lines(recent, width)
            .iter()
            .map(flatten_line_text)
            .collect()
    }

    // -- relative-time formatter ------------------------------------------

    #[test]
    fn relative_mtime_buckets() {
        let ago = |secs| SystemTime::now() - Duration::from_secs(secs);
        assert_eq!(relative_mtime(ago(0)), "just now");
        assert_eq!(relative_mtime(ago(59)), "just now");
        assert_eq!(relative_mtime(ago(60)), "1 minute ago");
        assert_eq!(relative_mtime(ago(5 * 60)), "5 minutes ago");
        assert_eq!(relative_mtime(ago(2 * 3_600)), "2 hours ago");
        assert_eq!(relative_mtime(ago(30 * 3_600)), "yesterday");
    }

    #[test]
    fn relative_mtime_older_than_two_days_is_a_calendar_date() {
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        let rendered = relative_mtime(old);
        assert!(
            !rendered.contains("ago"),
            "expected a calendar date, got {rendered:?}"
        );
    }

    #[test]
    fn relative_mtime_handles_future_mtime() {
        // Clock skew (mtime slightly in the future) must not panic.
        let future = SystemTime::now() + Duration::from_secs(120);
        assert_eq!(relative_mtime(future), "just now");
    }

    // -- render-from-state path -------------------------------------------

    #[test]
    fn empty_state_shows_placeholder() {
        let out = lines_text(&[], 40);
        assert_eq!(out, vec!["No recent activity".to_string()]);
    }

    #[test]
    fn populated_state_shows_titles_and_relative_times() {
        let sessions = vec![
            recent("Fix the parser bug", 2 * 3_600),
            recent("Wire up onboarding", 3 * 86_400),
        ];
        let out = lines_text(&sessions, 40).join("\n");
        assert!(out.contains("Fix the parser bug"), "first title: {out:?}");
        assert!(out.contains("2 hours ago"), "first time: {out:?}");
        assert!(out.contains("Wire up onboarding"), "second title: {out:?}");
        // Past yesterday the entry carries a calendar date, which moves with
        // the clock, so assert that the relative wording is gone instead.
        assert!(!out.contains("days ago"), "second time: {out:?}");
        // The placeholder must NOT appear when there is real activity.
        assert!(
            !out.contains("No recent activity"),
            "no placeholder: {out:?}"
        );
    }

    #[test]
    fn no_entry_outgrows_its_column() {
        // The relative time is wordier than it used to be, and the label
        // truncates against its width. A line wider than the column wraps and
        // pushes the rest of the welcome box down.
        let sessions = vec![
            recent("short", 2 * 3_600),
            recent("a much longer session label", 5 * 60),
            recent("older work", 30 * 86_400),
        ];
        for width in [20, 30, 40, 60] {
            for line in lines_text(&sessions, width) {
                assert!(
                    line.chars().count() <= width,
                    "line {line:?} exceeds width {width}"
                );
            }
        }
    }

    #[test]
    fn caps_at_five_entries() {
        let sessions: Vec<RecentSession> = (0..8)
            .map(|i| recent(&format!("session {i}"), 60))
            .collect();
        assert_eq!(recent_activity_lines(&sessions, 40).len(), 5);
    }

    #[test]
    fn long_label_is_truncated_and_leaves_room_for_time() {
        let sessions = vec![recent(
            "an extremely long session title that should be truncated to fit",
            60,
        )];
        let out = lines_text(&sessions, 20);
        assert_eq!(out.len(), 1);
        let line = &out[0];
        assert!(line.contains('\u{2026}'), "should be ellipsised: {line:?}");
        assert!(
            line.ends_with("1 minute ago"),
            "time preserved at end: {line:?}"
        );
    }

    #[test]
    fn welcome_box_renders_recent_activity_from_state() {
        // Full-widget smoke test: the section header renders and, when state is
        // populated, a session label reaches the screen buffer without panic.
        let mut app = App::new(Config::default(), CostTracker::new());
        app.recent_sessions = vec![recent("Sortable label ABCDEF", 2 * 3_600)];

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render_welcome_box(frame, &app, frame.area()))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            screen.contains("Recent activity"),
            "header rendered: present"
        );
        assert!(screen.contains("Sortable label"), "session label rendered");
        assert!(screen.contains("2 hours ago"), "relative time rendered");
    }

    #[test]
    fn welcome_box_shows_a_calendar_date_for_older_sessions() {
        let mut app = App::new(Config::default(), CostTracker::new());
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        app.recent_sessions = vec![recent("Older session", 30 * 86_400)];

        let mut terminal = match Terminal::new(TestBackend::new(80, 24)) {
            Ok(terminal) => terminal,
            Err(err) => panic!("test terminal: {err}"),
        };
        if let Err(err) = terminal.draw(|frame| render_welcome_box(frame, &app, frame.area())) {
            panic!("draw: {err}");
        }
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();

        assert!(
            screen.contains(&relative_mtime(old)),
            "expected the calendar date on screen, got {screen:?}"
        );
        assert!(!screen.contains("days ago"), "relative wording is gone");
    }
}

// ---------------------------------------------------------------------------
// Message timestamps (showMessageTimestamps)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod message_timestamp_tests {
    use super::*;
    use crate::app::App;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use mikmik_core::types::Message;

    /// Whether the rendered transcript contains an `HH:MM` clock.
    fn has_clock(text: &str) -> bool {
        let bytes = text.as_bytes();
        bytes.windows(5).any(|w| {
            w[0].is_ascii_digit()
                && w[1].is_ascii_digit()
                && w[2] == b':'
                && w[3].is_ascii_digit()
                && w[4].is_ascii_digit()
        })
    }

    fn transcript_text(app: &App) -> String {
        render_message_items(app, 80)
            .iter()
            .flat_map(|item| item.line.spans.iter().map(|span| span.content.to_string()))
            .collect()
    }

    fn app_with_a_turn() -> App {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.replace_messages(vec![Message::user("ping"), Message::assistant("pong")]);
        app
    }

    #[test]
    fn transcript_hides_times_until_the_setting_is_on() {
        let app = app_with_a_turn();
        let rendered = transcript_text(&app);
        assert!(rendered.contains("ping") && rendered.contains("pong"));
        assert!(
            !has_clock(&rendered),
            "timestamps are opt-in, got {rendered:?}"
        );
    }

    #[test]
    fn toggling_the_setting_refreshes_the_cached_transcript() {
        let mut app = app_with_a_turn();

        // Prime the full-result cache with the timestamps-off rendering.
        let before = transcript_text(&app);
        assert!(!has_clock(&before));

        // Flipping the setting does not bump `transcript_version`, so this only
        // repaints if the flag is part of the cache key.
        app.settings_screen.show_message_timestamps = true;
        let after = transcript_text(&app);
        assert!(
            has_clock(&after),
            "toggling the setting must invalidate the cached lines, got {after:?}"
        );
        assert!(after.contains("ping") && after.contains("pong"));
    }
}

#[cfg(test)]
mod status_line_tests {
    use super::*;
    use crate::app::App;
    use mikmik_core::config::{Config, ProviderConfig};
    use mikmik_core::cost::CostTracker;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Draw the input pane and return the status row as plain text.
    fn status_row(app: &App) -> String {
        let mut terminal = match Terminal::new(TestBackend::new(120, 6)) {
            Ok(terminal) => terminal,
            Err(err) => panic!("test terminal: {err}"),
        };
        if let Err(err) = terminal.draw(|frame| render_input(frame, app, frame.area(), true)) {
            panic!("draw: {err}");
        }
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect()
    }

    fn app_with_account(account: &str, model: &str) -> App {
        let mut config = Config::default();
        config.provider_configs.insert(
            account.to_string(),
            ProviderConfig {
                protocol: Some("anthropic".to_string()),
                ..Default::default()
            },
        );
        config.provider = Some(account.to_string());
        let mut app = App::new(config, CostTracker::new());
        app.has_credentials = true;
        app.model_name = model.to_string();
        app
    }

    #[test]
    fn the_status_line_names_the_active_account() {
        // `/switch` only writes `config.provider`; the status line has to read
        // it from there or it keeps showing whatever was set at startup.
        let app = app_with_account("work", "claude-opus-5");
        let row = status_row(&app);
        assert!(
            row.contains("claude-opus-5 · work"),
            "expected the account name in the status line, got {row:?}"
        );
    }

    #[test]
    fn a_slash_in_the_model_id_is_not_read_as_an_account() {
        // OpenRouter model ids carry a slash of their own. Splitting on the
        // first one would print `Llama-3.3 · meta-llama`.
        let app = app_with_account("openrouter", "meta-llama/Llama-3.3");
        let row = status_row(&app);
        assert!(
            row.contains("meta-llama/Llama-3.3 · openrouter"),
            "expected the full model id, got {row:?}"
        );
    }

    #[test]
    fn an_account_prefix_on_the_model_still_wins() {
        let mut app = app_with_account("work", "claude-opus-5");
        app.config.provider_configs.insert(
            "personal".to_string(),
            ProviderConfig {
                protocol: Some("anthropic".to_string()),
                ..Default::default()
            },
        );
        app.model_name = "personal/claude-sonnet-5".to_string();
        let row = status_row(&app);
        assert!(
            row.contains("claude-sonnet-5 · personal"),
            "expected the prefixed account to win, got {row:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Live execution timeline panel
// ---------------------------------------------------------------------------

#[cfg(test)]
mod timeline_panel_tests {
    use super::*;
    use crate::app::App;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use mikmik_query::QueryEvent;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app_with_rows(rows: usize) -> App {
        let config = Config {
            timeline_enabled: true,
            ..Default::default()
        };
        let mut app = App::new(config, CostTracker::new());
        for idx in 0..rows {
            app.handle_query_event(QueryEvent::ToolStart {
                tool_name: "Read".to_string(),
                tool_id: format!("tool-{idx}"),
                input_json: format!(r#"{{"file_path":"file-{idx}.rs"}}"#),
            });
        }
        app.timeline_visible = true;
        app
    }

    /// Draw the whole app and return the screen as one string per row.
    fn screen(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = match Terminal::new(TestBackend::new(width, height)) {
            Ok(terminal) => terminal,
            Err(err) => panic!("test terminal: {err}"),
        };
        if let Err(err) = terminal.draw(|frame| render_app(frame, app)) {
            panic!("draw: {err}");
        }
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_hidden_panel_leaves_the_whole_area_to_the_transcript() {
        let area = Rect::new(0, 0, 140, 40);
        assert_eq!(split_area_for_timeline(area, false), (area, None));
    }

    #[test]
    fn a_wide_terminal_puts_the_panel_beside_the_transcript() {
        let area = Rect::new(0, 0, 140, 40);
        let (transcript, panel) = split_area_for_timeline(area, true);
        let panel = match panel {
            Some(panel) => panel,
            None => panic!("140 columns has room for a side panel"),
        };
        assert_eq!(
            transcript.height, area.height,
            "a side panel is full height"
        );
        assert_eq!(panel.height, area.height);
        assert_eq!(transcript.width + panel.width, area.width);
        assert!(
            (TIMELINE_SIDE_MIN_WIDTH..=TIMELINE_SIDE_MAX_WIDTH).contains(&panel.width),
            "the panel width should stay inside its bounds, got {}",
            panel.width
        );
        assert_eq!(panel.x, transcript.width, "the panel sits on the right");
    }

    #[test]
    fn the_widest_terminal_still_caps_the_panel() {
        let (_, panel) = split_area_for_timeline(Rect::new(0, 0, 400, 40), true);
        let panel = match panel {
            Some(panel) => panel,
            None => panic!("400 columns has room for a side panel"),
        };
        assert_eq!(panel.width, TIMELINE_SIDE_MAX_WIDTH);
    }

    #[test]
    fn a_narrow_terminal_docks_the_panel_at_the_bottom() {
        let area = Rect::new(0, 0, 90, 30);
        let (transcript, panel) = split_area_for_timeline(area, true);
        let panel = match panel {
            Some(panel) => panel,
            None => panic!("90x30 has room for a bottom panel"),
        };
        assert_eq!(transcript.width, area.width, "a bottom panel is full width");
        assert_eq!(panel.width, area.width);
        assert_eq!(transcript.height + panel.height, area.height);
        assert_eq!(panel.y, transcript.height, "the panel sits below");
        assert!(
            transcript.height >= TIMELINE_TRANSCRIPT_MIN_HEIGHT,
            "the transcript keeps its floor, got {}",
            transcript.height
        );
    }

    #[test]
    fn a_tiny_terminal_gets_no_panel_at_all() {
        for area in [Rect::new(0, 0, 30, 24), Rect::new(0, 0, 90, 8)] {
            assert_eq!(
                split_area_for_timeline(area, true),
                (area, None),
                "{area:?} is too small to split"
            );
        }
    }

    #[test]
    fn the_window_keeps_the_selected_row_on_screen() {
        assert_eq!(
            timeline_window(20, 19, 5),
            15..20,
            "the cursor is the last row"
        );
        assert_eq!(
            timeline_window(20, 2, 5),
            0..5,
            "an early cursor pins the top"
        );
        assert_eq!(timeline_window(20, 9, 5), 5..10);
        assert_eq!(
            timeline_window(3, 0, 5),
            0..3,
            "fewer rows than the capacity"
        );
        assert_eq!(timeline_window(0, 0, 5), 0..0, "an empty timeline");
        assert_eq!(timeline_window(20, 5, 0), 0..0, "no room to draw");
    }

    #[test]
    fn the_side_panel_lists_the_rows_it_recorded() {
        let app = app_with_rows(3);
        let screen = screen(&app, 140, 30);
        let joined = screen.join("\n");
        assert!(
            joined.contains("timeline (3)"),
            "the border should count the rows:\n{joined}"
        );
        assert!(
            joined.contains("file-2.rs"),
            "the newest row should be on screen:\n{joined}"
        );
    }

    #[test]
    fn the_bottom_panel_lists_the_rows_it_recorded() {
        let app = app_with_rows(3);
        let joined = screen(&app, 90, 30).join("\n");
        assert!(
            joined.contains("timeline (3)"),
            "the panel should be drawn at 90 columns too:\n{joined}"
        );
    }

    #[test]
    fn a_tiny_terminal_draws_no_panel() {
        let app = app_with_rows(3);
        let joined = screen(&app, 40, 8).join("\n");
        assert!(
            !joined.contains("timeline"),
            "40x8 has no room for the panel:\n{joined}"
        );
    }

    #[test]
    fn an_empty_timeline_says_so() {
        let config = Config {
            timeline_enabled: true,
            ..Default::default()
        };
        let mut app = App::new(config, CostTracker::new());
        app.timeline_visible = true;
        app.messages.push(mikmik_core::types::Message::user("hi"));

        let joined = screen(&app, 140, 30).join("\n");
        assert!(
            joined.contains("No steps recorded yet."),
            "an empty panel should explain itself:\n{joined}"
        );
    }

    #[test]
    fn an_expanded_detail_sits_directly_under_its_row() {
        let mut app = app_with_rows(2);
        app.timeline.add_turn_summary(
            "turn-1",
            "Assistant turn 1 finished",
            0,
            26,
            "1800 in, 830 out",
            "stop_reason=end_turn",
            Some(1800),
            Some(830),
            None,
        );
        app.timeline.set_selected_idx(2);
        app.timeline_focused = true;
        app.timeline_expanded = true;

        let screen = screen(&app, 140, 30);
        let row = screen
            .iter()
            .position(|line| line.contains("Assistant turn 1 finished"))
            .unwrap_or_else(|| {
                panic!(
                    "the selected row should be on screen:\n{}",
                    screen.join("\n")
                )
            });
        let detail = screen
            .iter()
            .position(|line| line.contains("stop_reason=end_turn"))
            .unwrap_or_else(|| panic!("the detail should be on screen:\n{}", screen.join("\n")));

        assert_eq!(
            detail,
            row + 1,
            "a detail parked at the far end of the panel cannot be read against \
             the row it explains:\n{}",
            screen.join("\n")
        );
    }

    #[test]
    fn durations_read_in_the_unit_that_fits() {
        assert_eq!(timeline_duration_label(120), "120ms");
        assert_eq!(timeline_duration_label(1500), "1.5s");
        assert_eq!(timeline_duration_label(125_000), "2m05s");
    }

    #[test]
    fn large_token_counts_are_shortened() {
        assert_eq!(timeline_token_label(940), "940");
        assert_eq!(timeline_token_label(12_400), "12.4k");
    }
}

#[cfg(test)]
mod tab_expansion_tests {
    use super::*;
    use crate::app::{ToolStatus, ToolUseBlock};

    /// Flatten a rendered line to the text a terminal would receive.
    fn flatten(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn a_tab_never_reaches_a_buffer_cell() {
        assert_eq!(expand_tabs("1\talpha"), "1    alpha");
        assert_eq!(expand_tabs("no tabs here"), "no tabs here");
    }

    #[test]
    fn tool_output_with_tabs_renders_without_them() {
        // `Read` numbers its lines with a tab, so this is the common case, not
        // an edge one: the terminal jumps to its own tab stop and every column
        // after it on the row is off by what ratatui counted.
        let block = ToolUseBlock {
            id: "tool-1".to_string(),
            name: "Read".to_string(),
            input_json: r#"{"file_path":"notes.txt"}"#.to_string(),
            turn_index: None,
            status: ToolStatus::Done,
            output_preview: Some("1\talpha\n2\tbeta\n3\tgamma".to_string()),
            live_output: String::new(),
            duration_ms: None,
        };

        let mut lines = Vec::new();
        render_tool_block_lines(
            &mut lines,
            &block,
            0,
            None,
            &crate::theme_colors::ColorPalette::for_theme("default"),
            80,
            false,
        );

        let rendered: Vec<String> = lines.iter().map(flatten).collect();
        for line in &rendered {
            assert!(!line.contains('\t'), "a tab survived in {line:?}");
        }
        assert!(
            rendered.iter().any(|line| line.contains("1    alpha")),
            "the numbered line should keep its columns, got {rendered:?}"
        );
    }

    #[test]
    fn a_timeline_row_renders_without_tabs() {
        let row = TimelineRow {
            id: "tool-1".to_string(),
            title: "Reading file:\tnotes.txt".to_string(),
            kind: mikmik_core::timeline::TimelineKind::ToolCall,
            status: TimelineStatus::Done,
            started_at_ms: 0,
            finished_at_ms: Some(5),
            token_delta_input: None,
            token_delta_output: None,
            cost_delta_usd: None,
            detail_preview: String::new(),
            expandable_details: String::new(),
        };

        let line = flatten(&timeline_row_line(&row, false, false, 60, 0));
        assert!(!line.contains('\t'), "a tab survived in {line:?}");
    }
}

#[cfg(test)]
mod external_status_line_tests {
    use super::*;
    use crate::app::App;
    use mikmik_core::config::{Config, StatusLineConfig};
    use mikmik_core::cost::CostTracker;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 12;

    fn app_with_output(output: &str, padding: Option<u16>) -> App {
        let config = Config {
            status_line: Some(StatusLineConfig {
                kind: "command".to_string(),
                command: "irrelevant".to_string(),
                padding,
                refresh_interval: None,
                hide_vim_mode_indicator: false,
            }),
            ..Config::default()
        };
        let mut app = App::new(config, CostTracker::new());
        app.status_line_override = Some(output.to_string());
        app
    }

    fn draw(app: &App) -> ratatui::buffer::Buffer {
        let mut terminal = match Terminal::new(TestBackend::new(WIDTH, HEIGHT)) {
            Ok(terminal) => terminal,
            Err(err) => panic!("test terminal: {err}"),
        };
        if let Err(err) = terminal.draw(|frame| render_app(frame, app)) {
            panic!("draw: {err}");
        }
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The status line sits directly above the footer, which is the last row.
    fn status_row(buffer: &ratatui::buffer::Buffer) -> String {
        row_text(buffer, HEIGHT - 2)
    }

    #[test]
    fn the_output_lands_above_the_footer() {
        let buffer = draw(&app_with_output("ctx 42%", None));
        assert_eq!(status_row(&buffer), "ctx 42%");
    }

    #[test]
    fn non_ascii_output_is_not_stripped() {
        // The footer used to filter on `is_ascii_graphic`, printing "alyor".
        let buffer = draw(&app_with_output("çalışıyor", None));
        assert_eq!(status_row(&buffer), "çalışıyor");
    }

    #[test]
    fn a_colour_reaches_the_cells() {
        let buffer = draw(&app_with_output("\u{1b}[32mok\u{1b}[0m!", None));
        assert_eq!(status_row(&buffer), "ok!");
        assert_eq!(buffer[(0, HEIGHT - 2)].fg, Color::Green);
        // The reset returns the cell to the frame's own foreground.
        assert_eq!(buffer[(2, HEIGHT - 2)].fg, Color::White);
    }

    #[test]
    fn every_printed_line_gets_its_own_row() {
        let buffer = draw(&app_with_output("first\nsecond", None));
        assert_eq!(row_text(&buffer, HEIGHT - 3), "first");
        assert_eq!(row_text(&buffer, HEIGHT - 2), "second");
    }

    #[test]
    fn padding_indents_the_output() {
        let buffer = draw(&app_with_output("ctx", Some(3)));
        assert_eq!(row_text(&buffer, HEIGHT - 2), "   ctx");
    }

    #[test]
    fn a_line_wider_than_the_terminal_is_clipped() {
        let long = "x".repeat(usize::from(WIDTH) + 20);
        let buffer = draw(&app_with_output(&long, None));
        let row = status_row(&buffer);

        assert_eq!(row.chars().count(), usize::from(WIDTH));
        assert!(
            row.ends_with('\u{2026}'),
            "expected an ellipsis, got {row:?}"
        );
    }

    #[test]
    fn a_flood_of_lines_cannot_take_the_screen() {
        let flood = (0..40)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let buffer = draw(&app_with_output(&flood, None));

        // Half the screen at most, so the transcript keeps the rest. Only the
        // status line prints bare numbers, so counting those counts its rows.
        let rows = (0..HEIGHT)
            .filter(|y| row_text(&buffer, *y).trim().parse::<u32>().is_ok())
            .count();
        assert_eq!(
            rows,
            usize::from(HEIGHT) / 2,
            "status line took {rows} rows"
        );
    }

    #[test]
    fn the_row_yields_to_the_suggestion_popup() {
        let mut app = app_with_output("ctx 42%", None);
        app.prompt_input.suggestions = vec![crate::prompt_input::TypeaheadSuggestion {
            text: "/help".to_string(),
            description: String::new(),
            source: crate::prompt_input::TypeaheadSource::SlashCommand,
        }];
        let buffer = draw(&app);

        for y in 0..HEIGHT {
            assert!(
                !row_text(&buffer, y).contains("ctx 42%"),
                "the status line stayed up while suggestions were open"
            );
        }
    }

    #[test]
    fn nothing_is_reserved_before_the_first_run() {
        let mut app = app_with_output("", None);
        app.status_line_override = None;
        let buffer = draw(&app);

        assert_eq!(status_row(&buffer), "");
    }
}

#[cfg(test)]
mod workspace_root_notice_tests {
    use super::*;
    use crate::app::App;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use std::path::PathBuf;

    fn notices(config: Config) -> String {
        let app = App::new(config, CostTracker::new());
        startup_notice_lines(&app, 80)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_extra_directory_is_listed_under_its_root_name() {
        let config = Config {
            project_dir: Some(PathBuf::from("/repo")),
            additional_dirs: vec![PathBuf::from("/elsewhere/docs")],
            ..Config::default()
        };

        let text = notices(config);
        assert!(text.contains("&docs"), "{text:?}");
        assert!(text.contains("/elsewhere/docs"), "{text:?}");
    }

    #[test]
    fn the_working_directory_is_not_listed_again() {
        let config = Config {
            project_dir: Some(PathBuf::from("/repo")),
            additional_dirs: vec![PathBuf::from("/elsewhere/docs")],
            ..Config::default()
        };

        assert!(!notices(config).contains("&main"));
    }

    #[test]
    fn two_directories_of_the_same_name_are_told_apart() {
        let config = Config {
            project_dir: Some(PathBuf::from("/repo")),
            additional_dirs: vec![PathBuf::from("/a/lib"), PathBuf::from("/b/lib")],
            ..Config::default()
        };

        let text = notices(config);
        assert!(text.contains("&lib "), "{text:?}");
        assert!(text.contains("&lib-2"), "{text:?}");
    }

    #[test]
    fn without_extra_directories_nothing_is_listed() {
        let config = Config {
            project_dir: Some(PathBuf::from("/repo")),
            ..Config::default()
        };

        assert!(!notices(config).contains('&'));
    }
}

#[cfg(test)]
mod footer_todo_progress_tests {
    //! The footer counter reads the tool blocks already in memory, so it has
    //! to survive a missing `todos` array, an empty list and unparseable
    //! input without drawing a misleading count.
    use super::*;
    use crate::app::App;
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use mikmik_query::QueryEvent;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app_with_todos(input_json: &str) -> App {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.handle_query_event(QueryEvent::ToolStart {
            tool_name: "TodoWrite".to_string(),
            tool_id: "todo-1".to_string(),
            input_json: input_json.to_string(),
        });
        app
    }

    fn footer_row(app: &App, width: u16, height: u16) -> String {
        let mut terminal = match Terminal::new(TestBackend::new(width, height)) {
            Ok(terminal) => terminal,
            Err(err) => panic!("test terminal: {err}"),
        };
        if let Err(err) = terminal.draw(|frame| render_app(frame, app)) {
            panic!("draw: {err}");
        }
        let buffer = terminal.backend().buffer();
        let row = buffer.area.height - 1;
        (0..buffer.area.width)
            .map(|x| buffer[(x, row)].symbol())
            .collect()
    }

    #[test]
    fn counts_completed_against_total() {
        let app = app_with_todos(
            r#"{"todos":[
                {"id":"1","content":"a","status":"completed"},
                {"id":"2","content":"b","status":"in_progress"},
                {"id":"3","content":"c","status":"pending"}
            ]}"#,
        );
        assert_eq!(footer_todo_progress(&app), Some((1, 3)));
    }

    #[test]
    fn only_the_most_recent_call_counts() {
        let mut app = app_with_todos(r#"{"todos":[{"id":"1","content":"a","status":"pending"}]}"#);
        app.handle_query_event(QueryEvent::ToolStart {
            tool_name: "TodoWrite".to_string(),
            tool_id: "todo-2".to_string(),
            input_json: r#"{"todos":[
                {"id":"1","content":"a","status":"completed"},
                {"id":"2","content":"b","status":"completed"}
            ]}"#
            .to_string(),
        });
        assert_eq!(footer_todo_progress(&app), Some((2, 2)));
    }

    #[test]
    fn no_todo_call_no_empty_list_and_no_broken_input_draw_a_counter() {
        let bare = App::new(Config::default(), CostTracker::new());
        assert_eq!(footer_todo_progress(&bare), None);
        assert_eq!(
            footer_todo_progress(&app_with_todos(r#"{"todos":[]}"#)),
            None
        );
        assert_eq!(
            footer_todo_progress(&app_with_todos(r#"{"other":1}"#)),
            None
        );
        assert_eq!(footer_todo_progress(&app_with_todos("not json")), None);
    }

    #[test]
    fn the_counter_reaches_the_drawn_footer() {
        let app = app_with_todos(
            r#"{"todos":[
                {"id":"1","content":"a","status":"completed"},
                {"id":"2","content":"b","status":"pending"}
            ]}"#,
        );
        let footer = footer_row(&app, 120, 24);
        assert!(footer.contains("\u{2713}1/2"), "{footer:?}");
    }

    #[test]
    fn an_empty_list_draws_no_counter() {
        // Same app shape as the populated case, so the only difference in the
        // footer is the counter itself.
        let empty = footer_row(&app_with_todos(r#"{"todos":[]}"#), 120, 24);
        let populated = footer_row(
            &app_with_todos(r#"{"todos":[{"id":"1","content":"a","status":"pending"}]}"#),
            120,
            24,
        );
        assert!(!empty.contains('\u{2713}'), "{empty:?}");
        assert!(populated.contains("\u{2713}0/1"), "{populated:?}");
    }

    #[test]
    fn a_narrow_terminal_still_draws_a_single_footer_row() {
        // The counter lengthens an already crowded right-hand side; the
        // rightmost section may be clipped, but the layout must not break.
        let app = app_with_todos(r#"{"todos":[{"id":"1","content":"a","status":"pending"}]}"#);
        let footer = footer_row(&app, 40, 24);
        assert_eq!(footer.chars().count(), 40, "{footer:?}");
    }
}

#[cfg(test)]
mod timeline_summary_tests {
    //! The tok/s average and the skills/MCP counts do not fit the footer, so
    //! they live on the timeline panel's last row. That row spends from the
    //! same height budget as the entries above it.
    use super::*;
    use crate::app::App;
    use mikmik_core::config::{Config, McpServerConfig};
    use mikmik_core::cost::CostTracker;
    use mikmik_core::timeline::{TimelineKind, TimelineRow, TimelineStatus};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn row(started_at_ms: u64, finished_at_ms: Option<u64>, output: Option<u64>) -> TimelineRow {
        TimelineRow {
            id: format!("row-{started_at_ms}"),
            title: "Read".to_string(),
            kind: TimelineKind::ToolCall,
            status: TimelineStatus::Done,
            started_at_ms,
            finished_at_ms,
            token_delta_input: None,
            token_delta_output: output,
            cost_delta_usd: None,
            detail_preview: String::new(),
            expandable_details: String::new(),
        }
    }

    #[test]
    fn rate_averages_over_finished_rows() {
        // 300 tokens over 2s and 100 over 1s: 400 tokens in 3s.
        let rows = vec![
            row(0, Some(2_000), Some(300)),
            row(2_000, Some(3_000), Some(100)),
        ];
        let rate = timeline_output_rate(&rows).expect("two finished rows carry a rate");
        assert!((rate - 400.0 / 3.0).abs() < 0.001, "{rate}");
    }

    #[test]
    fn unfinished_rows_do_not_drag_the_average_down() {
        let rows = vec![row(0, Some(1_000), Some(100)), row(1_000, None, None)];
        let rate = timeline_output_rate(&rows).expect("the finished row carries a rate");
        assert!((rate - 100.0).abs() < 0.001, "{rate}");
    }

    #[test]
    fn no_measurable_work_yields_no_rate() {
        assert_eq!(timeline_output_rate(&[]), None);
        assert_eq!(timeline_output_rate(&[row(0, None, None)]), None);
        // A zero-length row must not divide by zero.
        assert_eq!(timeline_output_rate(&[row(5, Some(5), Some(100))]), None);
    }

    fn app_with_summary_inputs() -> App {
        let config = Config {
            timeline_enabled: true,
            mcp_servers: vec![McpServerConfig {
                name: "docs".to_string(),
                command: Some("docs-server".to_string()),
                args: Vec::new(),
                env: Default::default(),
                url: None,
                headers: Default::default(),
                server_type: "stdio".to_string(),
                origin: Default::default(),
            }],
            ..Default::default()
        };
        let mut app = App::new(config, CostTracker::new());
        app.skill_count = 4;
        app.timeline.rows = vec![row(0, Some(1_000), Some(120))];
        app.timeline_visible = true;
        app
    }

    #[test]
    fn summary_names_every_populated_section() {
        let app = app_with_summary_inputs();
        let line = timeline_summary_line(&app, 60).expect("all three sections are populated");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("120.0 tok/s"), "{text:?}");
        assert!(text.contains("4 skills"), "{text:?}");
        assert!(text.contains("1 mcp"), "{text:?}");
    }

    #[test]
    fn empty_sections_are_omitted_and_an_empty_summary_is_dropped() {
        let mut app = app_with_summary_inputs();
        app.skill_count = 0;
        let line = timeline_summary_line(&app, 60).expect("rate and mcp remain");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains("skills"), "{text:?}");

        let bare = App::new(Config::default(), CostTracker::new());
        assert!(timeline_summary_line(&bare, 60).is_none());
    }

    #[test]
    fn a_narrow_panel_drops_sections_from_the_right() {
        let app = app_with_summary_inputs();
        let line = timeline_summary_line(&app, 14).expect("the first section still fits");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "120.0 tok/s");
    }

    #[test]
    fn the_summary_reaches_the_drawn_panel_and_keeps_its_height() {
        let app = app_with_summary_inputs();
        let mut terminal = match Terminal::new(TestBackend::new(100, 24)) {
            Ok(terminal) => terminal,
            Err(err) => panic!("test terminal: {err}"),
        };
        if let Err(err) = terminal.draw(|frame| render_app(frame, &app)) {
            panic!("draw: {err}");
        }
        let buffer = terminal.backend().buffer();
        let screen: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert!(
            screen.iter().any(|line| line.contains("tok/s")),
            "summary missing: {screen:?}"
        );
        assert_eq!(screen.len(), 24);
    }
}

#[cfg(test)]
mod bridge_indicator_tests {
    //! The bridge indicator shipped as mojibake: the UTF-8 bytes of 🔗 read
    //! back as latin-1. It is written as an escape now so re-reading the file
    //! under the wrong encoding cannot reintroduce the corruption.
    use super::*;
    use ratatui::buffer::Buffer;

    fn status_row(bridge_connected: bool) -> String {
        let data = StatusLineData {
            model: "test-model".to_string(),
            bridge_connected,
            ..Default::default()
        };
        let area = Rect::new(0, 0, 120, 1);
        let mut buffer = Buffer::empty(area);
        render_full_status_line(&data, area, &mut buffer);
        (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect()
    }

    #[test]
    fn a_connected_bridge_draws_a_link_glyph_not_mojibake() {
        let row = status_row(true);
        assert!(row.contains('\u{1F517}'), "{row:?}");
        for corrupted in ['\u{00F0}', '\u{0178}', '\u{201D}'] {
            assert!(!row.contains(corrupted), "{corrupted:?} in {row:?}");
        }
    }

    #[test]
    fn a_disconnected_bridge_draws_nothing() {
        assert!(!status_row(false).contains('\u{1F517}'));
    }
}

// ---------------------------------------------------------------------------
// A system annotation with no message behind it
// ---------------------------------------------------------------------------

#[cfg(test)]
mod annotation_only_transcript_tests {
    use super::*;
    use crate::app::{App, SystemMessageStyle};
    use mikmik_core::config::Config;
    use mikmik_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};

    fn drawn_rows(app: &App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render_app(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect()
            })
            .collect()
    }

    fn drawn(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render_app(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
        }
        out
    }

    #[test]
    fn every_output_line_gets_its_own_row() {
        // The one-line rule shape a short notice uses would run a command's
        // output off the edge and lose everything past the width.
        let mut app = App::new(Config::default(), CostTracker::new());
        app.push_system_message(
            "$ ls\nfirst-file\nsecond-file\nthird-file".to_string(),
            SystemMessageStyle::Info,
        );

        let rows: Vec<String> = drawn_rows(&app);
        for wanted in ["first-file", "second-file", "third-file"] {
            assert!(
                rows.iter().any(|row| row.contains(wanted)),
                "{wanted} is missing from {rows:?}"
            );
        }
        assert!(
            rows.iter().filter(|row| row.contains("-file")).count() == 3,
            "each output line needs its own row: {rows:?}"
        );
    }

    #[test]
    fn a_shell_command_run_before_anything_was_said_is_still_drawn() {
        // The bang path pushes an annotation and no message, so a transcript
        // judged empty by message count alone would hide a command that ran.
        let mut app = App::new(Config::default(), CostTracker::new());
        // The mascot, not the name: the banner keeps drawing "MikMik" above
        // the transcript, so only the cat tells the welcome box apart.
        assert!(
            drawn(&app).contains("> ^ <"),
            "the welcome box is what an empty transcript draws"
        );

        app.push_system_message("$ pwd\n/some/where".to_string(), SystemMessageStyle::Info);

        let after = drawn(&app);
        assert!(after.contains("/some/where"), "{after:?}");
        assert!(
            !after.contains("> ^ <"),
            "the welcome box must give way to the output"
        );
    }
}
