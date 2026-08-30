//! Live execution timeline: a deterministic record of what the agent did.
//!
//! The container holds one row per tool call, per finished turn, and per status
//! note, so a caller can show progress instead of a spinner. It carries no
//! rendering code and reads no clock: every timestamp arrives from the caller,
//! which keeps it testable and lets the TUI and a remote client show the same
//! row with the same timing.
//!
//! It lives in `mikmik-core` rather than `mikmik-tui` because
//! `mikmik-bridge` names [`TimelineRow`] on the wire and cannot depend on the
//! TUI crate.

use serde::{Deserialize, Serialize};

/// How far a timeline row has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineStatus {
    Running,
    Done,
    Error,
    Cancelled,
}

/// What a timeline row describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineKind {
    ToolCall,
    TurnSummary,
    Status,
    /// A change to the session's todo list (a `TodoWrite` call), summarised as
    /// its progress rather than shown as a raw tool call.
    Todo,
    /// A plan-mode transition (entered or left).
    Plan,
}

/// One visible timeline row.
///
/// `token_delta_*` and `cost_delta_usd` stay `None` on a tool row: usage is
/// accounted per turn, not per tool call, so only a turn summary can fill them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineRow {
    pub id: String,
    pub title: String,
    pub kind: TimelineKind,
    pub status: TimelineStatus,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub token_delta_input: Option<u64>,
    pub token_delta_output: Option<u64>,
    pub cost_delta_usd: Option<f64>,
    pub detail_preview: String,
    pub expandable_details: String,
}

impl TimelineRow {
    /// How long the step took, once it has finished.
    pub fn duration_ms(&self) -> Option<u64> {
        self.finished_at_ms
            .map(|finished_at_ms| finished_at_ms.saturating_sub(self.started_at_ms))
    }

    /// Whether the row has more to show than its preview line.
    pub fn has_expandable_details(&self) -> bool {
        !self.expandable_details.is_empty()
    }
}

/// How many rows a timeline keeps before dropping the oldest.
pub const DEFAULT_MAX_ROWS: usize = 200;

/// The rows collected so far, plus the caller's cursor into them.
#[derive(Debug, Clone)]
pub struct Timeline {
    pub rows: Vec<TimelineRow>,
    pub selected_idx: usize,
    pub max_rows: usize,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ROWS)
    }
}

impl Timeline {
    /// Build a timeline that keeps at most `max_rows` rows.
    ///
    /// A cap of zero is raised to one, because a timeline that can hold nothing
    /// would panic the pruning arithmetic rather than simply stay empty.
    pub fn new(max_rows: usize) -> Self {
        Self {
            rows: Vec::new(),
            selected_idx: 0,
            max_rows: max_rows.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn selected_row(&self) -> Option<&TimelineRow> {
        self.rows.get(self.selected_idx)
    }

    /// Drop every row and put the cursor back at the start.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.selected_idx = 0;
    }

    /// Pull the cursor back onto a row that still exists.
    pub fn clamp_selected_idx(&mut self) {
        self.selected_idx = Self::clamp_index(self.selected_idx, self.rows.len());
    }

    /// Move the cursor, clamping to the last row.
    pub fn set_selected_idx(&mut self, idx: usize) {
        self.selected_idx = Self::clamp_index(idx, self.rows.len());
    }

    /// Start a tool row and return its index.
    pub fn add_running_tool(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        started_at_ms: u64,
        detail_preview: impl Into<String>,
        expandable_details: impl Into<String>,
    ) -> usize {
        self.push_running(
            TimelineKind::ToolCall,
            id,
            title,
            started_at_ms,
            detail_preview,
            expandable_details,
        )
    }

    /// Start a todo-list row (a `TodoWrite`), closed later by `finish_tool`
    /// under the same `id`, so it reads as its progress instead of a raw call.
    pub fn add_running_todo(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        started_at_ms: u64,
        detail_preview: impl Into<String>,
        expandable_details: impl Into<String>,
    ) -> usize {
        self.push_running(
            TimelineKind::Todo,
            id,
            title,
            started_at_ms,
            detail_preview,
            expandable_details,
        )
    }

    /// Push a running row of `kind`; `finish_tool` closes it by `id`.
    fn push_running(
        &mut self,
        kind: TimelineKind,
        id: impl Into<String>,
        title: impl Into<String>,
        started_at_ms: u64,
        detail_preview: impl Into<String>,
        expandable_details: impl Into<String>,
    ) -> usize {
        self.push_row(TimelineRow {
            id: id.into(),
            title: title.into(),
            kind,
            status: TimelineStatus::Running,
            started_at_ms,
            finished_at_ms: None,
            token_delta_input: None,
            token_delta_output: None,
            cost_delta_usd: None,
            detail_preview: detail_preview.into(),
            expandable_details: expandable_details.into(),
        })
    }

    /// Close a tool row that `add_running_tool` opened.
    ///
    /// Returns `None` when no row carries `id`, which lets the caller decide
    /// between synthesising the missing row and ignoring the event.
    pub fn finish_tool(
        &mut self,
        id: &str,
        finished_at_ms: u64,
        status: TimelineStatus,
        detail_preview: impl Into<String>,
        expandable_details: impl Into<String>,
    ) -> Option<usize> {
        let idx = self.rows.iter().rposition(|row| row.id == id)?;
        let row = self.rows.get_mut(idx)?;
        row.status = status;
        row.finished_at_ms = Some(finished_at_ms);
        row.detail_preview = detail_preview.into();
        row.expandable_details = expandable_details.into();
        self.clamp_selected_idx();
        Some(idx)
    }

    /// Record a finished turn, with the usage it spent.
    #[allow(clippy::too_many_arguments)]
    pub fn add_turn_summary(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        started_at_ms: u64,
        finished_at_ms: u64,
        detail_preview: impl Into<String>,
        expandable_details: impl Into<String>,
        token_delta_input: Option<u64>,
        token_delta_output: Option<u64>,
        cost_delta_usd: Option<f64>,
    ) -> usize {
        self.push_row(TimelineRow {
            id: id.into(),
            title: title.into(),
            kind: TimelineKind::TurnSummary,
            status: TimelineStatus::Done,
            started_at_ms,
            finished_at_ms: Some(finished_at_ms),
            token_delta_input,
            token_delta_output,
            cost_delta_usd,
            detail_preview: detail_preview.into(),
            expandable_details: expandable_details.into(),
        })
    }

    /// Record a one-shot note, such as a status line or a cancellation.
    pub fn add_status_note(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        at_ms: u64,
        status: TimelineStatus,
        detail_preview: impl Into<String>,
        expandable_details: impl Into<String>,
    ) -> usize {
        self.push_note(
            TimelineKind::Status,
            id,
            title,
            at_ms,
            status,
            detail_preview,
            expandable_details,
        )
    }

    /// Record a one-shot plan-mode transition (entered or left).
    pub fn add_plan_note(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        at_ms: u64,
        status: TimelineStatus,
        detail_preview: impl Into<String>,
        expandable_details: impl Into<String>,
    ) -> usize {
        self.push_note(
            TimelineKind::Plan,
            id,
            title,
            at_ms,
            status,
            detail_preview,
            expandable_details,
        )
    }

    /// Push a completed one-shot row of `kind` (start and finish at `at_ms`).
    #[allow(clippy::too_many_arguments)]
    fn push_note(
        &mut self,
        kind: TimelineKind,
        id: impl Into<String>,
        title: impl Into<String>,
        at_ms: u64,
        status: TimelineStatus,
        detail_preview: impl Into<String>,
        expandable_details: impl Into<String>,
    ) -> usize {
        self.push_row(TimelineRow {
            id: id.into(),
            title: title.into(),
            kind,
            status,
            started_at_ms: at_ms,
            finished_at_ms: Some(at_ms),
            token_delta_input: None,
            token_delta_output: None,
            cost_delta_usd: None,
            detail_preview: detail_preview.into(),
            expandable_details: expandable_details.into(),
        })
    }

    fn push_row(&mut self, row: TimelineRow) -> usize {
        self.rows.push(row);
        let idx = self.rows.len() - 1;
        let removed = self.prune_to_limit();
        idx.saturating_sub(removed)
    }

    /// Drop the oldest rows until the cap is met, and return how many went.
    fn prune_to_limit(&mut self) -> usize {
        if self.rows.len() <= self.max_rows {
            self.clamp_selected_idx();
            return 0;
        }

        let removed = self.rows.len().saturating_sub(self.max_rows);
        self.rows.drain(..removed);
        self.selected_idx =
            Self::selected_index_after_prune(self.selected_idx, removed, self.rows.len());
        removed
    }

    /// Where the cursor lands after `removed_from_front` rows are dropped.
    ///
    /// A cursor on a surviving row follows that row; a cursor on a dropped row
    /// falls back to the first, because the row it named no longer exists.
    fn selected_index_after_prune(
        selected_idx: usize,
        removed_from_front: usize,
        remaining_len: usize,
    ) -> usize {
        if remaining_len == 0 {
            return 0;
        }

        if selected_idx < removed_from_front {
            0
        } else {
            (selected_idx - removed_from_front).min(remaining_len - 1)
        }
    }

    fn clamp_index(idx: usize, len: usize) -> usize {
        if len == 0 {
            0
        } else {
            idx.min(len - 1)
        }
    }
}

/// What `/timeline` was asked to do.
///
/// Parsed here rather than in either front end: the terminal owns the panel and
/// the command layer owns the help text, and both have to agree on the words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineAction {
    Show,
    Hide,
    Toggle,
    Clear,
}

/// Parse the `/timeline` argument. An empty argument toggles the panel.
pub fn parse_timeline_action(args: &str) -> Result<TimelineAction, String> {
    match args.trim() {
        "" | "toggle" => Ok(TimelineAction::Toggle),
        "show" | "on" => Ok(TimelineAction::Show),
        "hide" | "off" => Ok(TimelineAction::Hide),
        "clear" => Ok(TimelineAction::Clear),
        other => Err(format!(
            "Unknown argument '{other}'. Use: /timeline [show|hide|toggle|clear]"
        )),
    }
}

/// Shown wherever the timeline is asked for while the setting is off.
pub const TIMELINE_DISABLED_HINT: &str = "Timeline is disabled; turn it on in /settings.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finishing_a_tool_updates_the_row_it_opened() {
        let mut timeline = Timeline::new(8);

        let idx = timeline.add_running_tool(
            "tool-1",
            "Read file",
            100,
            "reading README.md",
            "full tool input",
        );
        assert_eq!(idx, 0);
        assert_eq!(timeline.rows[0].status, TimelineStatus::Running);
        assert_eq!(timeline.rows[0].finished_at_ms, None);

        let updated = timeline
            .finish_tool(
                "tool-1",
                175,
                TimelineStatus::Done,
                "read complete",
                "full tool output",
            )
            .expect("the row it opened must be findable");

        assert_eq!(updated, 0);
        let row = timeline.selected_row().expect("selected row");
        assert_eq!(row.id, "tool-1");
        assert_eq!(row.title, "Read file", "finishing must not retitle the row");
        assert_eq!(row.kind, TimelineKind::ToolCall);
        assert_eq!(row.status, TimelineStatus::Done);
        assert_eq!(row.started_at_ms, 100);
        assert_eq!(row.duration_ms(), Some(75));
        assert_eq!(row.detail_preview, "read complete");
        assert!(row.has_expandable_details());
    }

    #[test]
    fn finishing_an_unknown_id_reports_it_instead_of_guessing() {
        let mut timeline = Timeline::new(8);
        timeline.add_running_tool("tool-1", "Read file", 100, "preview", "details");

        let missed = timeline.finish_tool("tool-2", 175, TimelineStatus::Done, "x", "y");

        assert_eq!(missed, None);
        assert_eq!(timeline.rows[0].status, TimelineStatus::Running);
    }

    #[test]
    fn a_tool_row_carries_no_usage_because_usage_is_per_turn() {
        let mut timeline = Timeline::new(8);
        timeline.add_running_tool("tool-1", "Read file", 100, "preview", "details");
        timeline.finish_tool("tool-1", 175, TimelineStatus::Done, "done", "out");

        let row = &timeline.rows[0];
        assert_eq!(row.token_delta_input, None);
        assert_eq!(row.token_delta_output, None);
        assert_eq!(row.cost_delta_usd, None);
    }

    #[test]
    fn a_turn_summary_captures_usage_and_duration() {
        let mut timeline = Timeline::new(8);

        let idx = timeline.add_turn_summary(
            "turn-3",
            "Assistant turn 3 finished",
            1_000,
            1_420,
            "assistant completed",
            "stop_reason=end_turn",
            Some(123),
            Some(77),
            Some(0.018),
        );

        assert_eq!(idx, 0);
        let row = timeline.rows.last().expect("turn summary row");
        assert_eq!(row.kind, TimelineKind::TurnSummary);
        assert_eq!(row.status, TimelineStatus::Done);
        assert_eq!(row.duration_ms(), Some(420));
        assert_eq!(row.token_delta_input, Some(123));
        assert_eq!(row.token_delta_output, Some(77));
        assert_eq!(row.cost_delta_usd, Some(0.018));
    }

    #[test]
    fn a_status_note_starts_and_ends_at_the_same_instant() {
        let mut timeline = Timeline::new(8);

        timeline.add_status_note(
            "note-1",
            "Cancelled",
            500,
            TimelineStatus::Cancelled,
            "",
            "",
        );

        let row = &timeline.rows[0];
        assert_eq!(row.kind, TimelineKind::Status);
        assert_eq!(row.status, TimelineStatus::Cancelled);
        assert_eq!(row.duration_ms(), Some(0));
        assert!(!row.has_expandable_details());
    }

    #[test]
    fn a_todo_row_is_a_running_todo_that_finish_tool_closes() {
        let mut timeline = Timeline::new(8);

        timeline.add_running_todo("todo-1", "Todos (1/3)", 100, "1/3 done", "");
        let row = &timeline.rows[0];
        assert_eq!(row.kind, TimelineKind::Todo);
        assert_eq!(row.status, TimelineStatus::Running);

        // `finish_tool` is kind-agnostic and closes the todo under its id.
        timeline.finish_tool("todo-1", 200, TimelineStatus::Done, "3/3 done", "");
        let row = &timeline.rows[0];
        assert_eq!(row.status, TimelineStatus::Done);
        assert_eq!(row.duration_ms(), Some(100));
    }

    #[test]
    fn a_plan_note_is_a_one_shot_plan_row() {
        let mut timeline = Timeline::new(8);

        timeline.add_plan_note(
            "plan-1",
            "Entered plan mode",
            300,
            TimelineStatus::Done,
            "",
            "",
        );
        let row = &timeline.rows[0];
        assert_eq!(row.kind, TimelineKind::Plan);
        assert_eq!(row.duration_ms(), Some(0));
    }

    fn note(timeline: &mut Timeline, id: &str) {
        timeline.add_status_note(id, id, 1, TimelineStatus::Done, "", "");
    }

    #[test]
    fn the_oldest_rows_go_when_the_cap_is_reached() {
        let mut timeline = Timeline::new(3);
        for id in ["row-1", "row-2", "row-3", "row-4"] {
            note(&mut timeline, id);
        }

        assert_eq!(timeline.len(), 3);
        assert_eq!(
            timeline
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["row-2", "row-3", "row-4"]
        );
    }

    #[test]
    fn a_zero_cap_becomes_one_instead_of_holding_nothing() {
        let mut timeline = Timeline::new(0);
        assert_eq!(timeline.max_rows, 1);

        note(&mut timeline, "row-1");
        note(&mut timeline, "row-2");

        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline.rows[0].id, "row-2");
    }

    #[test]
    fn the_cursor_follows_its_row_through_a_prune() {
        let mut timeline = Timeline::new(3);
        for id in ["tool-1", "tool-2", "tool-3"] {
            note(&mut timeline, id);
        }
        timeline.set_selected_idx(2);

        note(&mut timeline, "tool-4");

        assert_eq!(timeline.selected_idx, 1);
        assert_eq!(
            timeline.selected_row().map(|row| row.id.as_str()),
            Some("tool-3")
        );
    }

    #[test]
    fn the_cursor_falls_to_the_first_row_when_its_own_row_is_dropped() {
        assert_eq!(Timeline::selected_index_after_prune(4, 2, 5), 2);
        assert_eq!(Timeline::selected_index_after_prune(1, 3, 4), 0);
        assert_eq!(Timeline::selected_index_after_prune(3, 1, 0), 0);
    }

    #[test]
    fn pushing_returns_the_index_the_row_ended_up_at() {
        let mut timeline = Timeline::new(2);
        note(&mut timeline, "row-1");
        note(&mut timeline, "row-2");

        let idx = timeline.add_status_note("row-3", "row-3", 1, TimelineStatus::Done, "", "");

        assert_eq!(idx, 1, "the new row is last after the front was pruned");
        assert_eq!(timeline.rows[idx].id, "row-3");
    }

    #[test]
    fn clearing_drops_every_row_and_resets_the_cursor() {
        let mut timeline = Timeline::new(8);
        note(&mut timeline, "row-1");
        note(&mut timeline, "row-2");
        timeline.set_selected_idx(1);

        timeline.clear();

        assert!(timeline.is_empty());
        assert_eq!(timeline.selected_idx, 0);
        assert_eq!(timeline.selected_row(), None);
    }

    #[test]
    fn a_row_survives_the_json_round_trip_the_bridge_sends_it_through() {
        let mut timeline = Timeline::new(8);
        timeline.add_turn_summary(
            "turn-1",
            "Assistant turn 1 finished",
            10,
            42,
            "preview",
            "details",
            Some(5),
            Some(6),
            Some(0.5),
        );
        let row = &timeline.rows[0];

        let json = serde_json::to_string(row).expect("serialize");
        assert!(json.contains(r#""kind":"turn_summary""#));
        assert!(json.contains(r#""status":"done""#));

        let back: TimelineRow = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&back, row);
    }
}
