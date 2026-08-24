//! Agent / coordinator progress views for the TUI.
//! Mirrors src/components/agents/ (13 files).

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use std::path::{Path, PathBuf};

use crate::overlays::{
    begin_modal_buf, modal_header_line_area, render_modal_title_buf, MIKMIK_ACCENT, MIKMIK_MUTED,
    MIKMIK_PANEL_BG, MIKMIK_TEXT,
};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// The role of an agent in the manager-executor architecture.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AgentRole {
    #[default]
    Normal,
    Manager,
    Executor {
        parent_id: String,
    },
}

/// The current status of a sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
    WaitingForTool,
    Complete,
    Failed,
}

impl AgentStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingForTool => "waiting",
            Self::Complete => "done",
            Self::Failed => "failed",
        }
    }

    pub fn color(&self) -> Color {
        match self {
            Self::Idle => Color::DarkGray,
            Self::Running => Color::Green,
            Self::WaitingForTool => Color::Yellow,
            Self::Complete => Color::Cyan,
            Self::Failed => Color::Red,
        }
    }
}

/// A sub-agent or coordinator instance.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    /// Unique agent ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Current status.
    pub status: AgentStatus,
    /// Current tool being executed (if any).
    pub current_tool: Option<String>,
    /// Number of turns completed.
    pub turns_completed: u32,
    /// Brief description or last output snippet.
    pub last_output: Option<String>,
    /// Role in the managed agent architecture.
    #[allow(dead_code)]
    pub agent_role: AgentRole,
    /// Model name used by this agent.
    pub model_name: Option<String>,
    /// Cost in USD accumulated by this agent.
    pub cost_usd: f64,
}

/// A defined agent (from .mikmik/agents/*.md or plugin).
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    /// Backing markdown file path.
    pub file_path: PathBuf,
    /// Agent name.
    pub name: String,
    /// Source: "user" | "plugin:{name}" | "builtin".
    pub source: String,
    /// Model name.
    pub model: Option<String>,
    /// Memory scope.
    pub memory_scope: Option<String>,
    /// Description.
    pub description: String,
    /// Tool list (empty = all tools).
    pub tools: Vec<String>,
    /// If another agent overrides this one.
    pub shadowed_by: Option<String>,
    /// Markdown body / instructions.
    pub instructions: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEditorField {
    Name,
    Model,
    Memory,
    Tools,
    Description,
    Prompt,
}

impl AgentEditorField {
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::Model,
            Self::Model => Self::Memory,
            Self::Memory => Self::Tools,
            Self::Tools => Self::Description,
            Self::Description => Self::Prompt,
            Self::Prompt => Self::Name,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Name => Self::Prompt,
            Self::Model => Self::Name,
            Self::Memory => Self::Model,
            Self::Tools => Self::Memory,
            Self::Description => Self::Tools,
            Self::Prompt => Self::Description,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentEditorState {
    pub original_index: Option<usize>,
    pub name: String,
    pub model: String,
    pub memory_scope: String,
    pub tools: String,
    pub description: String,
    pub prompt: String,
    pub selected_field: AgentEditorField,
    pub error: Option<String>,
    pub saved_message: Option<String>,
}

impl Default for AgentEditorState {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentEditorState {
    pub fn new() -> Self {
        Self {
            original_index: None,
            name: String::new(),
            model: "claude-sonnet-4-6".to_string(),
            memory_scope: String::new(),
            tools: String::new(),
            description: String::new(),
            prompt: String::new(),
            selected_field: AgentEditorField::Name,
            error: None,
            saved_message: None,
        }
    }

    pub fn from_definition(def: Option<(usize, &AgentDefinition)>) -> Self {
        match def {
            Some((idx, def)) => Self {
                original_index: Some(idx),
                name: def.name.clone(),
                model: def
                    .model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
                memory_scope: def.memory_scope.clone().unwrap_or_default(),
                tools: def.tools.join(", "),
                description: def.description.clone(),
                prompt: def.instructions.clone(),
                selected_field: AgentEditorField::Name,
                error: None,
                saved_message: None,
            },
            None => Self::new(),
        }
    }

    fn selected_text_mut(&mut self) -> &mut String {
        match self.selected_field {
            AgentEditorField::Name => &mut self.name,
            AgentEditorField::Model => &mut self.model,
            AgentEditorField::Memory => &mut self.memory_scope,
            AgentEditorField::Tools => &mut self.tools,
            AgentEditorField::Description => &mut self.description,
            AgentEditorField::Prompt => &mut self.prompt,
        }
    }
}

// ---------------------------------------------------------------------------
// Screen routes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsRoute {
    List,
    Detail(usize),         // index into definitions
    Editor(Option<usize>), // None = create new
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Full state for the agents menu overlay.
#[derive(Debug, Clone)]
pub struct AgentsMenuState {
    pub visible: bool,
    pub route: AgentsRoute,
    pub definitions: Vec<AgentDefinition>,
    pub active_agents: Vec<AgentInfo>,
    pub list_scroll: usize,
    pub selected_row: usize,
    pub project_root: Option<PathBuf>,
    pub editor: AgentEditorState,
}

impl AgentsMenuState {
    pub fn new() -> Self {
        Self {
            visible: false,
            route: AgentsRoute::List,
            definitions: Vec::new(),
            active_agents: Vec::new(),
            list_scroll: 0,
            selected_row: 0,
            project_root: None,
            editor: AgentEditorState::new(),
        }
    }

    pub fn open(&mut self, project_root: &std::path::Path) {
        self.definitions = load_agent_definitions(project_root);
        self.selected_row = 0;
        self.list_scroll = 0;
        self.route = AgentsRoute::List;
        self.project_root = Some(project_root.to_path_buf());
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    pub fn select_prev(&mut self) {
        let row_count = self.definitions.len() + 1;
        if row_count == 0 {
            return;
        }
        if self.selected_row == 0 {
            self.selected_row = row_count - 1;
        } else {
            self.selected_row -= 1;
        }
    }

    pub fn select_next(&mut self) {
        let row_count = self.definitions.len() + 1;
        if row_count == 0 {
            return;
        }
        self.selected_row = (self.selected_row + 1) % row_count;
    }

    pub fn confirm_selection(&mut self) {
        match self.route {
            AgentsRoute::List => {
                if self.selected_row == 0 {
                    self.open_editor(None);
                } else {
                    let idx = self.selected_row - 1;
                    if idx < self.definitions.len() {
                        self.route = AgentsRoute::Detail(idx);
                    }
                }
            }
            AgentsRoute::Detail(idx) => self.open_editor(Some(idx)),
            AgentsRoute::Editor(_) => {}
        }
    }

    pub fn go_back(&mut self) {
        match &self.route {
            AgentsRoute::Detail(_) | AgentsRoute::Editor(_) => {
                self.route = AgentsRoute::List;
            }
            AgentsRoute::List => {
                self.close();
            }
        }
    }

    pub fn open_editor(&mut self, idx: Option<usize>) {
        self.editor = AgentEditorState::from_definition(
            idx.and_then(|index| self.definitions.get(index).map(|def| (index, def))),
        );
        self.route = AgentsRoute::Editor(idx);
    }

    pub fn editor_insert_char(&mut self, ch: char) {
        let field = self.editor.selected_text_mut();
        field.push(ch);
        self.editor.error = None;
        self.editor.saved_message = None;
    }

    pub fn editor_backspace(&mut self) {
        self.editor.selected_text_mut().pop();
    }

    pub fn editor_insert_newline(&mut self) {
        match self.editor.selected_field {
            AgentEditorField::Description | AgentEditorField::Prompt => {
                self.editor.selected_text_mut().push('\n');
            }
            _ => self.editor.selected_field = self.editor.selected_field.next(),
        }
    }

    pub fn editor_next_field(&mut self) {
        self.editor.selected_field = self.editor.selected_field.next();
    }

    pub fn editor_prev_field(&mut self) {
        self.editor.selected_field = self.editor.selected_field.prev();
    }

    pub fn save_editor(&mut self) -> Result<String, String> {
        validate_editor(&self.editor)?;
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| "Project root is unavailable.".to_string())?;
        let file_path = self
            .editor
            .original_index
            .and_then(|idx| self.definitions.get(idx).map(|def| def.file_path.clone()))
            .unwrap_or_else(|| {
                root.join(".mikmik")
                    .join("agents")
                    .join(format!("{}.md", slugify_agent_name(&self.editor.name)))
            });

        write_editor_to_disk(&file_path, &self.editor)?;
        self.definitions = load_agent_definitions(&root);

        let saved_idx = self
            .definitions
            .iter()
            .position(|def| def.file_path == file_path)
            .unwrap_or(0);
        self.selected_row = saved_idx + 1;
        self.route = AgentsRoute::Detail(saved_idx);
        let msg = format!("Saved agent to {}", file_path.display());
        self.editor.saved_message = Some(msg.clone());
        self.editor.error = None;
        Ok(msg)
    }
}

impl Default for AgentsMenuState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/// Load agent definitions from the project, the user directory, and every
/// installed plugin, highest precedence first.
///
/// A name that appears more than once keeps every copy, and each losing copy
/// records the source that shadows it, so the list shows what actually applies.
pub fn load_agent_definitions(project_root: &std::path::Path) -> Vec<AgentDefinition> {
    let mut sources: Vec<(String, std::path::PathBuf)> = vec![
        (
            "project".to_string(),
            project_root.join(".mikmik").join("agents"),
        ),
        (
            "user".to_string(),
            mikmik_core::config::Settings::config_dir().join("agents"),
        ),
    ];
    sources.extend(plugin_agent_sources());
    collect_agent_defs(&sources)
}

/// Read every `*.md` agent under `sources`, in the given precedence order.
fn collect_agent_defs(sources: &[(String, std::path::PathBuf)]) -> Vec<AgentDefinition> {
    let mut defs: Vec<AgentDefinition> = Vec::new();
    for (source, dir) in sources {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "md"))
            .collect();
        // read_dir order is filesystem order; sort so one directory's listing
        // does not reshuffle between runs.
        paths.sort();

        for path in paths {
            if let Some(mut def) = parse_agent_def(&path) {
                def.source.clone_from(source);
                def.shadowed_by = defs
                    .iter()
                    .find(|earlier| earlier.name == def.name)
                    .map(|winner| winner.source.clone());
                defs.push(def);
            }
        }
    }

    defs
}

/// The `agents/` directory of every installed plugin, labelled by plugin name.
fn plugin_agent_sources() -> Vec<(String, std::path::PathBuf)> {
    let Some(registry) = mikmik_plugins::global_plugin_registry() else {
        return Vec::new();
    };
    registry
        .enabled()
        .into_iter()
        .filter_map(|plugin| {
            plugin
                .agents_path
                .as_ref()
                .map(|dir| (format!("plugin:{}", plugin.name), dir.clone()))
        })
        .collect()
}

fn parse_agent_def(path: &std::path::Path) -> Option<AgentDefinition> {
    let content = std::fs::read_to_string(path).ok()?;
    let stem = path.file_stem()?.to_string_lossy().to_string();

    let (name, model, memory, description, tools, instructions) =
        if let Some(after) = content.strip_prefix("---") {
            let end = after.find("\n---")?;
            let front = &after[..end];
            let body = after[end + 4..].trim().to_string();
            let name = extract_yaml_str(front, "name").unwrap_or_else(|| stem.clone());
            let model = extract_yaml_str(front, "model");
            let memory = extract_yaml_str(front, "memory_scope")
                .or_else(|| extract_yaml_str(front, "memory"));
            let desc = extract_yaml_str(front, "description").unwrap_or_default();
            let tools = extract_yaml_list(front, "tools");
            (name, model, memory, desc, tools, body)
        } else {
            (
                stem,
                None,
                None,
                content.lines().next().unwrap_or("").to_string(),
                vec![],
                content.trim().to_string(),
            )
        };

    Some(AgentDefinition {
        file_path: path.to_path_buf(),
        name,
        source: "user".to_string(),
        model,
        memory_scope: memory,
        description,
        tools,
        shadowed_by: None,
        instructions,
    })
}

fn extract_yaml_str(front: &str, key: &str) -> Option<String> {
    for line in front.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            return Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

fn extract_yaml_list(front: &str, key: &str) -> Vec<String> {
    for line in front.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            let rest = rest.trim().trim_matches('[').trim_matches(']');
            return rest
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    Vec::new()
}

fn slugify_agent_name(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if matches!(ch, ' ' | '-' | '_' | '.') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn validate_editor(editor: &AgentEditorState) -> Result<(), String> {
    let name = editor.name.trim();
    if name.is_empty() {
        return Err("Agent name is required.".to_string());
    }
    if slugify_agent_name(name).is_empty() {
        return Err("Agent name must contain letters or numbers.".to_string());
    }
    if editor.model.trim().is_empty() {
        return Err("Model is required.".to_string());
    }
    if editor.description.trim().is_empty() {
        return Err("Description is required.".to_string());
    }
    if editor.prompt.trim().is_empty() {
        return Err("Prompt body is required.".to_string());
    }
    Ok(())
}

fn serialize_editor(editor: &AgentEditorState) -> String {
    let tools = editor
        .tools
        .split(',')
        .map(|tool| tool.trim())
        .filter(|tool| !tool.is_empty())
        .map(|tool| format!("\"{}\"", tool))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", editor.name.trim()));
    out.push_str(&format!("model: {}\n", editor.model.trim()));
    if !editor.memory_scope.trim().is_empty() {
        out.push_str(&format!("memory_scope: {}\n", editor.memory_scope.trim()));
    }
    out.push_str(&format!("description: {}\n", editor.description.trim()));
    if !tools.is_empty() {
        out.push_str(&format!("tools: [{}]\n", tools));
    }
    out.push_str("---\n\n");
    out.push_str(editor.prompt.trim());
    out.push('\n');
    out
}

fn write_editor_to_disk(path: &Path, editor: &AgentEditorState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create {}: {}", parent.display(), err))?;
    }
    std::fs::write(path, serialize_editor(editor))
        .map_err(|err| format!("Failed to write {}: {}", path.display(), err))
}

// ---------------------------------------------------------------------------
// Rendering: Agents Menu overlay
// ---------------------------------------------------------------------------

/// Render the agents menu overlay.
pub fn render_agents_menu(state: &AgentsMenuState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    let layout = begin_modal_buf(buf, area, 92, 30, 2, 1);
    let (title, subtitle, footer) = match &state.route {
        AgentsRoute::List => (
            "Agents".to_string(),
            format!(
                " {} active  ·  {} definitions",
                state.active_agents.len(),
                state.definitions.len()
            ),
            " enter open  ·  esc close".to_string(),
        ),
        AgentsRoute::Detail(idx) => (
            state
                .definitions
                .get(*idx)
                .map(|def| def.name.clone())
                .unwrap_or_else(|| "Agent".to_string()),
            " Review configuration and prompt details.".to_string(),
            " enter edit  ·  esc back".to_string(),
        ),
        AgentsRoute::Editor(Some(_)) => (
            "Edit agent".to_string(),
            " Update metadata, tools, and prompt instructions.".to_string(),
            " tab move  ·  ctrl+s save  ·  esc back".to_string(),
        ),
        AgentsRoute::Editor(None) => (
            "Create agent".to_string(),
            " Define a new reusable agent for this workspace.".to_string(),
            " tab move  ·  ctrl+s save  ·  esc back".to_string(),
        ),
    };
    render_modal_title_buf(buf, layout.header_area, &title, "esc");
    if let Some(subtitle_area) = modal_header_line_area(layout.header_area, 1) {
        Paragraph::new(Line::from(vec![Span::styled(
            subtitle,
            Style::default().fg(MIKMIK_MUTED),
        )]))
        .render(subtitle_area, buf);
    }

    match &state.route {
        AgentsRoute::List => render_agents_list(state, layout.body_area, buf),
        AgentsRoute::Detail(idx) => {
            if let Some(def) = state.definitions.get(*idx) {
                render_agent_detail(def, layout.body_area, buf);
            }
        }
        AgentsRoute::Editor(Some(_idx)) => {
            render_agent_editor(state, layout.body_area, buf);
        }
        AgentsRoute::Editor(None) => {
            render_agent_editor(state, layout.body_area, buf);
        }
    }
    Paragraph::new(Line::from(vec![Span::styled(
        footer,
        Style::default()
            .fg(MIKMIK_MUTED)
            .add_modifier(Modifier::ITALIC),
    )]))
    .render(layout.footer_area, buf);
}

fn render_agents_list(state: &AgentsMenuState, area: Rect, buf: &mut Buffer) {
    let mut lines = Vec::new();
    if !state.active_agents.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            " Active now",
            Style::default()
                .fg(MIKMIK_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));
        for agent in state.active_agents.iter().take(5) {
            lines.push(active_agent_line(agent));
        }
        lines.push(Line::from(""));
    }

    let create_selected = state.selected_row == 0;
    lines.push(agent_list_row(
        "[+ Create new agent]".to_string(),
        "Create a reusable workspace agent".to_string(),
        create_selected,
        area.width,
    ));
    lines.push(Line::from(""));

    let max_visible = (area.height as usize).saturating_sub(lines.len() + 1);
    let start = state
        .list_scroll
        .min(state.definitions.len().saturating_sub(max_visible));

    for (i, def) in state.definitions[start..].iter().enumerate() {
        if i >= max_visible {
            break;
        }
        let abs_idx = start + i;
        let selected = state.selected_row == abs_idx + 1;
        let model_str = def.model.as_deref().unwrap_or("default");
        let shadow_suffix = if def.shadowed_by.is_some() {
            " ⚠"
        } else {
            ""
        };
        lines.push(agent_list_row(
            def.name.clone(),
            format!("{}  ·  {}{}", model_str, def.source, shadow_suffix),
            selected,
            area.width,
        ));
    }
    Paragraph::new(lines)
        .style(Style::default().bg(MIKMIK_PANEL_BG))
        .render(area, buf);
}

fn render_agent_detail(def: &AgentDefinition, area: Rect, buf: &mut Buffer) {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" Name       ", Style::default().fg(MIKMIK_MUTED)),
        Span::styled(
            def.name.clone(),
            Style::default()
                .fg(MIKMIK_TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ({})", def.source),
            Style::default().fg(MIKMIK_MUTED),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" Model      ", Style::default().fg(MIKMIK_MUTED)),
        Span::raw(def.model.as_deref().unwrap_or("default").to_string()),
    ]));
    if let Some(mem) = &def.memory_scope {
        lines.push(Line::from(vec![
            Span::styled(" Memory     ", Style::default().fg(MIKMIK_MUTED)),
            Span::raw(mem.clone()),
        ]));
    }
    if !def.tools.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" Tools      ", Style::default().fg(MIKMIK_MUTED)),
            Span::raw(def.tools.join(", ")),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(" Tools      ", Style::default().fg(MIKMIK_MUTED)),
            Span::styled("All tools", Style::default().fg(MIKMIK_MUTED)),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![Span::styled(
        " Description",
        Style::default()
            .fg(MIKMIK_ACCENT)
            .add_modifier(Modifier::BOLD),
    )]));
    for line in def.description.lines() {
        lines.push(Line::from(vec![Span::raw(format!(" {}", line))]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![Span::styled(
        " Prompt",
        Style::default()
            .fg(MIKMIK_ACCENT)
            .add_modifier(Modifier::BOLD),
    )]));
    for line in def.instructions.lines().take(8) {
        lines.push(Line::from(vec![Span::styled(
            format!(" {}", line),
            Style::default().fg(MIKMIK_TEXT),
        )]));
    }

    if let Some(shadow) = &def.shadowed_by {
        lines.push(Line::default());
        lines.push(Line::from(vec![Span::styled(
            format!("⚠ Shadowed by: {}", shadow),
            Style::default().fg(Color::Yellow),
        )]));
    }

    Paragraph::new(lines)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .style(Style::default().bg(MIKMIK_PANEL_BG))
        .render(area, buf);
}

fn render_agent_editor(state: &AgentsMenuState, area: Rect, buf: &mut Buffer) {
    let editor = &state.editor;
    let selected_style = Style::default()
        .fg(Color::White)
        .bg(MIKMIK_ACCENT)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(MIKMIK_TEXT);

    let field_style = |field: AgentEditorField| {
        if editor.selected_field == field {
            selected_style
        } else {
            normal_style
        }
    };

    let mut lines = vec![
        render_editor_field("Name", &editor.name, field_style(AgentEditorField::Name)),
        render_editor_field("Model", &editor.model, field_style(AgentEditorField::Model)),
        render_editor_field(
            "Memory",
            &editor.memory_scope,
            field_style(AgentEditorField::Memory),
        ),
        render_editor_field("Tools", &editor.tools, field_style(AgentEditorField::Tools)),
        render_editor_field(
            "Description",
            &editor.description,
            field_style(AgentEditorField::Description),
        ),
        Line::default(),
        Line::from(vec![Span::styled(
            " Prompt",
            Style::default()
                .fg(MIKMIK_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]),
    ];

    let prompt_style = field_style(AgentEditorField::Prompt);
    let prompt_lines = if editor.prompt.is_empty() {
        vec![Line::from(vec![Span::styled(
            "(empty)",
            prompt_style.add_modifier(Modifier::ITALIC),
        )])]
    } else {
        editor
            .prompt
            .lines()
            .map(|line| Line::from(vec![Span::styled(line.to_string(), prompt_style)]))
            .collect::<Vec<_>>()
    };
    lines.extend(prompt_lines);
    lines.push(Line::default());

    if let Some(msg) = editor.saved_message.as_ref() {
        lines.push(Line::from(vec![Span::styled(
            msg.clone(),
            Style::default().fg(Color::Green),
        )]));
    }
    if let Some(err) = editor.error.as_ref() {
        lines.push(Line::from(vec![Span::styled(
            err.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]));
    }

    Paragraph::new(lines)
        .style(Style::default().bg(MIKMIK_PANEL_BG))
        .render(area, buf);
}

fn render_editor_field(label: &str, value: &str, value_style: Style) -> Line<'static> {
    let display = if value.is_empty() {
        "(empty)".to_string()
    } else {
        value.to_string()
    };
    Line::from(vec![
        Span::styled(format!(" {label:<10} "), Style::default().fg(MIKMIK_MUTED)),
        Span::styled(display, value_style),
    ])
}

fn agent_list_row(title: String, meta: String, selected: bool, width: u16) -> Line<'static> {
    let bg = if selected {
        MIKMIK_ACCENT
    } else {
        MIKMIK_PANEL_BG
    };
    let title_style = if selected {
        Style::default()
            .fg(Color::White)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MIKMIK_TEXT).bg(bg)
    };
    let meta_style = if selected {
        Style::default().fg(Color::Rgb(248, 220, 236)).bg(bg)
    } else {
        Style::default().fg(MIKMIK_MUTED).bg(bg)
    };
    let mut spans = vec![
        Span::styled(" ", Style::default().bg(bg)),
        Span::styled(title, title_style),
        Span::styled(format!("  {}", meta), meta_style),
    ];
    let used: usize = spans.iter().map(|span| span.content.len()).sum();
    let pad = width.saturating_sub(used as u16) as usize;
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Rendering: one live agent
// ---------------------------------------------------------------------------

/// The prefix `AgentTool` gives a sub-agent's registry entry.
const SUBAGENT_TASK_PREFIX: &str = "subagent: ";

/// The agents a session is running right now, as the menu shows them.
///
/// Read from the background-task registry rather than from a field the TUI
/// keeps: no `QueryEvent` reports a sub-agent's progress, and `AgentTool`
/// hands its sub-agent a `None` event channel, so a TUI-side field had
/// nothing to fill it.
///
/// The registry knows a task's name and status and nothing else, so
/// `current_tool`, `turns_completed` and `cost_usd` stay empty. Filling them
/// needs an event the sub-agent does not emit today.
pub fn live_agents(
    tasks: &[mikmik_core::tasks::BackgroundTask],
    managed: Option<&mikmik_core::ManagedAgentConfig>,
    session_id: &str,
) -> Vec<AgentInfo> {
    let managed = managed.filter(|config| config.enabled);

    let mut agents: Vec<AgentInfo> = tasks
        .iter()
        .filter(|task| task.is_running())
        .filter_map(|task| {
            let name = task.name.strip_prefix(SUBAGENT_TASK_PREFIX)?;
            Some(AgentInfo {
                id: task.id.clone(),
                name: name.to_string(),
                status: AgentStatus::Running,
                current_tool: None,
                turns_completed: 0,
                last_output: task.output.last().cloned(),
                agent_role: match managed {
                    Some(_) => AgentRole::Executor {
                        parent_id: session_id.to_string(),
                    },
                    None => AgentRole::Normal,
                },
                model_name: managed.map(|config| config.executor_model.clone()),
                cost_usd: 0.0,
            })
        })
        .collect();

    // The manager is the session itself, so it has no registry entry. It is
    // only worth a row while it has something to manage.
    if let (Some(config), false) = (managed, agents.is_empty()) {
        agents.insert(
            0,
            AgentInfo {
                id: session_id.to_string(),
                name: "manager".to_string(),
                status: AgentStatus::Running,
                current_tool: None,
                turns_completed: 0,
                last_output: None,
                agent_role: AgentRole::Manager,
                model_name: Some(config.manager_model.clone()),
                cost_usd: 0.0,
            },
        );
    }

    agents
}

/// One row describing a running agent: role badge, name, model, status.
///
/// A line rather than a widget, so the agents menu can lay it out with
/// everything else it draws instead of reserving an area for a second block.
pub fn active_agent_line(agent: &AgentInfo) -> Line<'static> {
    let (prefix, role_badge, role_color, indent) = match &agent.agent_role {
        AgentRole::Manager => ("\u{25cf} ", "[MGR]", Color::Magenta, ""),
        AgentRole::Executor { .. } => ("  \u{25cb} ", "[EXE]", Color::Cyan, "  "),
        AgentRole::Normal => ("  \u{25cb} ", "", Color::DarkGray, "  "),
    };
    let tool_str = agent
        .current_tool
        .as_deref()
        .map(|t| format!(" \u{2192} {}", t))
        .unwrap_or_default();
    let model_str = agent
        .model_name
        .as_deref()
        .map(|m| format!(" ({})", m))
        .unwrap_or_default();
    let cost_str = if agent.cost_usd > 0.0 {
        format!("  ${:.4}", agent.cost_usd)
    } else {
        String::new()
    };

    let mut spans = vec![
        Span::styled(indent.to_string(), Style::default()),
        Span::styled(prefix, Style::default().fg(agent.status.color())),
    ];
    if !role_badge.is_empty() {
        spans.push(Span::styled(
            format!("{} ", role_badge),
            Style::default().fg(role_color).add_modifier(Modifier::BOLD),
        ));
    }
    spans.extend(vec![
        Span::styled(agent.name.clone(), Style::default().fg(MIKMIK_TEXT)),
        Span::styled(model_str, Style::default().fg(MIKMIK_MUTED)),
        Span::styled(
            format!(" [{}]", agent.status.label()),
            Style::default().fg(agent.status.color()),
        ),
        Span::styled(tool_str, Style::default().fg(Color::Yellow)),
        Span::styled(cost_str, Style::default().fg(MIKMIK_MUTED)),
    ]);

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_subagent(name: &str) -> mikmik_core::tasks::BackgroundTask {
        mikmik_core::tasks::BackgroundTask::new(format!("{}{}", SUBAGENT_TASK_PREFIX, name))
    }

    fn managed_config() -> mikmik_core::ManagedAgentConfig {
        mikmik_core::ManagedAgentConfig {
            enabled: true,
            manager_model: "anthropic/claude-opus-4-6".to_string(),
            executor_model: "anthropic/claude-sonnet-4-6".to_string(),
            executor_max_turns: 10,
            max_concurrent_executors: 4,
            total_budget_usd: None,
            preset_name: None,
            executor_isolation: false,
        }
    }

    /// The menu used to read a field nothing ever wrote, so it was always
    /// empty however many agents were running.
    #[test]
    fn a_running_sub_agent_reaches_the_menu() {
        let tasks = vec![running_subagent("review the auth module")];
        let agents = live_agents(&tasks, None, "sess-1");

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "review the auth module");
        assert_eq!(agents[0].status, AgentStatus::Running);
    }

    #[test]
    fn a_finished_agent_is_not_active_any_more() {
        let mut task = running_subagent("done already");
        task.status = mikmik_core::tasks::TaskStatus::Completed;

        assert!(live_agents(&[task], None, "sess-1").is_empty());
    }

    /// The registry holds every background task, not only sub-agents.
    #[test]
    fn an_unrelated_background_task_is_not_an_agent() {
        let task = mikmik_core::tasks::BackgroundTask::new("cargo build");

        assert!(live_agents(&[task], None, "sess-1").is_empty());
    }

    #[test]
    fn managed_mode_names_the_manager_and_its_executors() {
        let tasks = vec![running_subagent("write the tests")];
        let config = managed_config();
        let agents = live_agents(&tasks, Some(&config), "sess-1");

        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].agent_role, AgentRole::Manager);
        assert_eq!(
            agents[0].model_name.as_deref(),
            Some("anthropic/claude-opus-4-6")
        );
        assert_eq!(
            agents[1].agent_role,
            AgentRole::Executor {
                parent_id: "sess-1".to_string()
            }
        );
        assert_eq!(
            agents[1].model_name.as_deref(),
            Some("anthropic/claude-sonnet-4-6")
        );
    }

    /// A manager with nothing to manage is not worth a row.
    #[test]
    fn managed_mode_shows_no_manager_without_executors() {
        assert!(live_agents(&[], Some(&managed_config()), "sess-1").is_empty());
    }

    #[test]
    fn a_configured_but_inactive_managed_mode_names_no_roles() {
        let mut config = managed_config();
        config.enabled = false;
        let tasks = vec![running_subagent("plain sub-agent")];

        let agents = live_agents(&tasks, Some(&config), "sess-1");

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_role, AgentRole::Normal);
        assert_eq!(agents[0].model_name, None);
    }

    /// The badge is the whole point of naming the roles.
    #[test]
    fn each_role_draws_its_own_badge() {
        let agents = live_agents(
            &[running_subagent("scout")],
            Some(&managed_config()),
            "sess-1",
        );

        let manager: String = active_agent_line(&agents[0])
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let executor: String = active_agent_line(&agents[1])
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();

        assert!(manager.contains("[MGR]"), "{manager}");
        assert!(executor.contains("[EXE]"), "{executor}");
        assert!(executor.contains("scout"), "{executor}");
    }

    fn write_agent(dir: &std::path::Path, file: &str, body: &str) {
        std::fs::create_dir_all(dir).expect("agent dir");
        std::fs::write(dir.join(file), body).expect("agent file");
    }

    #[test]
    fn a_lower_source_records_what_shadows_it() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let project = tmp.path().join("project");
        let plugin = tmp.path().join("plugin");
        write_agent(
            &project,
            "reviewer.md",
            "---\nname: reviewer\ndescription: Project copy\n---\nProject body.",
        );
        write_agent(
            &plugin,
            "reviewer.md",
            "---\nname: reviewer\ndescription: Plugin copy\n---\nPlugin body.",
        );

        let defs = collect_agent_defs(&[
            ("project".to_string(), project),
            ("plugin:toolkit".to_string(), plugin),
        ]);

        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].source, "project");
        assert_eq!(defs[0].shadowed_by, None);
        assert_eq!(defs[1].source, "plugin:toolkit");
        assert_eq!(defs[1].shadowed_by.as_deref(), Some("project"));
    }

    #[test]
    fn a_plugin_agent_is_listed_under_its_plugin() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let plugin = tmp.path().join("agents");
        write_agent(&plugin, "auditor.md", "Audit the diff.");

        let defs = collect_agent_defs(&[("plugin:toolkit".to_string(), plugin)]);

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "auditor");
        assert_eq!(defs[0].source, "plugin:toolkit");
        assert_eq!(defs[0].shadowed_by, None);
    }

    #[test]
    fn a_missing_directory_is_skipped() {
        let defs = collect_agent_defs(&[(
            "user".to_string(),
            std::path::PathBuf::from("/nonexistent/agents"),
        )]);
        assert!(defs.is_empty());
    }
}
