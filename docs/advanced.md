# Advanced Features

This document covers MikMik's advanced capabilities beyond basic coding assistance.

---

## Extended thinking

Extended thinking gives the model additional computation budget to reason through hard problems before it responds.

### Commands

```
/thinking          Toggle extended thinking on or off for the session
/effort <level>    Set the effort level; see the ladder below
```

### CLI flags

```
mikmik --thinking <tokens>    Set a specific token budget for thinking
mikmik --effort <level>       Set the effort level
```

### Effort levels

The ladder is defined once, in the `effort` module of `mikmik-core`, in ascending order:

| Level       | Description                                                        |
|-------------|--------------------------------------------------------------------|
| `none`      | Thinking disabled; the model answers directly                      |
| `minimal`   | The smallest reasoning budget                                      |
| `low`       | Quick implementation with minimal overhead                         |
| `medium`    | Balanced reasoning; the default. `normal` is accepted as an alias  |
| `high`      | Deep reasoning; best quality for most tasks                        |
| `xhigh`     | Extended reasoning for hard problems                               |
| `max`       | Maximum available budget                                           |
| `ultracode` | Maximum reasoning plus the ultracode delegation workflow           |

`/effort` with no argument reports the level in force, and says `unset` when nothing was chosen. Unset is not the same as `medium`: it sends no reasoning configuration at all.

Only `low`, `high` and `normal` also move the output token limit (4096, 32768, and back to the model default). The other levels leave it alone.

---

## Auto-compaction

The context window has a finite size. Auto-compaction automatically summarises the conversation history when token usage approaches the limit, keeping the session alive without interruption.

### How it works

MikMik tracks token usage after every model turn. When input tokens reach `compact_threshold` percent of the context window, it summarises the history and replaces the messages with the summary plus any trailing context.

The `PreCompact` hook fires before compaction (exit code 2 blocks it). The `PostCompact` hook fires after.

Compaction runs through a `CompactBackend`, so it works on both dispatch arms: the raw Anthropic client, and any registered provider. `compact_model` picks a different model for the summary than the turn itself uses.

### Controlling auto-compaction

Two settings in [Configuration](configuration) govern it:

| Setting             | Default | Effect                                                 |
|---------------------|---------|--------------------------------------------------------|
| `auto_compact`      | `true`  | Set to `false` to keep `/compact` manual only           |
| `compact_threshold` | `90`    | Percent of the context window that triggers compaction  |

`compact_threshold` is clamped to 100. Only the user scope sets it; a project `settings.json` cannot. A value below 1 is read as the old fraction format and scaled, so `0.9` means 90.

### Manual compaction

```
/compact [custom instructions]
```

Runs compaction immediately. Optionally pass custom instructions to guide the summary (e.g. `/compact focus on the database schema changes`).

---

## Context window management

### /context

```
/context
```

Aliases: `/ctx`, `/ctx-viz`, `/context-visualizer`.

Reports two figures, because they describe different moments and neither one alone is the whole answer.

**Measured at the last request.** The token count the API itself returned for the last turn (`usage.total_input()`, which adds the plain input, the cache writes and the cache reads). This covers everything the request carried: the system prompt, the tool definitions and the messages. It is exact, and it is as of that request, so messages added since are not in it.

The denominator is the active model's own context window, resolved through `ModelRegistry::context_window_for` from the route. It is not a constant: Opus 5 carries 1M tokens and a Haiku carries 200K.

**Estimated now.** A per-category breakdown of the current message list:

| Category     | What lands here                                                        |
|--------------|------------------------------------------------------------------------|
| Conversation | User and assistant turns, including each tool call and its arguments   |
| Tool results | Every message carrying a `ToolResult` block                            |
| Attachments  | An `@file` injection, a paste placeholder, or IDE-supplied context     |

The estimate counts roughly four characters per token and pads the result by a third. It counts the same way compaction does, so `/context` and the auto-compact threshold cannot disagree about how large a conversation is.

The system prompt and the tool definitions are absent from this section on purpose. Nothing records their size, and re-assembling the system prompt outside `build_system_prompt` would drift from what a run actually sends. The measured figure above already covers them.

Once the window passes 75% full, the command ends with a compaction recommendation: collapse repeated reads when tool results dominate, a partial summary in between, and a full `/compact` past 90%.

Two fractions of the window drive the footer bar's colour, and both are defined once in the `constants` module of `mikmik-core`:

- **Warning:** at or above 80% of the window, the bar turns yellow.
- **Critical:** at or above 95%, it turns red.

The footer bar and the compact warning read the same two constants, so they agree at every point.

---

## Session management

Sessions are stored as JSONL files under `~/.config/mikmik/projects/<base64url(project-root)>/<session-id>.jsonl`. Each line in the file is a JSON object representing a message or event in the conversation. The per-session metadata index lives at `~/.config/mikmik/sessions/<id>.json`.

The transcript directory is derived from the project root: the git repository the working directory sits in, or the working directory itself when there is none. A session started in a subdirectory therefore files its transcript under the repository, which is where `/stats`, `/rewind` and the welcome screen's recent activity look for it.

`--print` writes the same three stores as an interactive session, so a headless conversation can be listed, resumed, searched and counted like any other.

### Commands

| Command                  | Description                                                                                       |
|--------------------------|---------------------------------------------------------------------------------------------------|
| `/resume [id or search]` | Resume a previous session by ID or fuzzy search term. Alias: `/continue`.                         |
| `/session`               | Show the remote session URL and QR code (available in remote mode).                               |
| `/fork`                  | Fork the current session into a new branch with fresh UUIDs, preserving the full message history. |
| `/rename <title>`        | Rename the current session. Appends a custom-title entry to the JSONL file.                       |
| `/export`                | Export the current session transcript.                                                            |
| `/rewind`                | Step back to an earlier point in the conversation.                                                |
| `/checkpoint`            | List the points recorded at the end of each turn, or return to one.                               |

`--resume <id>` continues a named session and `-c` / `--continue` continues the most recent one. Both work in `--print` as well as at the keyboard; headless exits non-zero when the named session cannot be loaded, rather than starting a fresh conversation.

### JSONL transcript format

Every message in the transcript is a newline-delimited JSON object. The key fields present on most entries:

```jsonl
{"uuid":"<uuid>","parentUuid":"<parent-uuid>","type":"user","message":{...},"timestamp":1234567890}
{"uuid":"<uuid>","parentUuid":"<parent-uuid>","type":"assistant","message":{...},"timestamp":1234567891}
```

The `parentUuid` field forms a linked chain that allows MikMik to reconstruct the conversation tree. `/fork` rewrites all UUIDs while preserving the chain structure.

Special entry types include `summary` (compaction summaries), `custom-title` (from `/rename`), and various ephemeral progress indicators that are filtered out when reading the transcript for display.

---

## Worktree support

Subagents spawned via the `Agent` tool can operate in isolated git worktrees to avoid interfering with the main working tree.

### Tools

- `EnterWorktree` — checks out a new worktree and switches the agent's working directory to it.
- `ExitWorktree` — removes the worktree and returns the agent to the original directory.

### Custom worktree backends

For non-git repositories or specialised isolation requirements, the `WorktreeCreate` and `WorktreeRemove` hook events let you substitute an external worktree manager:

- `WorktreeCreate` receives `{"name": "<slug>"}` and must write the absolute path of the created directory to stdout.
- `WorktreeRemove` receives `{"worktree_path": "<path>"}` and is responsible for cleanup.

This means worktrees can be Docker containers, virtual machines, or any directory-backed isolation primitive.

---

## Plan mode

Plan mode restricts the model to read-only operations, allowing it to research a codebase and propose a plan before making any changes.

### Entering plan mode

```
/plan [description]
mikmik --permission-mode plan
```

When in plan mode:
- Write and execute operations require explicit permission.
- The model can read files, search, and reason freely.
- Exiting plan mode (via `ExitPlanMode`) returns to the permission mode that was in force before.

The model switches itself in with `EnterPlanMode` when a request needs research before code. The switch is real and takes effect on the running turn: the permission mode becomes `plan`, the tool roster is rebuilt, and the previous permission mode is remembered so that leaving plan mode restores it. No approval is asked, because the change is restrictive.

`EnterPlanMode` needs the TUI. In headless (`--print`) and ACP sessions there is nothing to switch, so the tool says the mode did not change rather than reporting success.

`ExitPlanMode` is the other half: the model presents the plan and the user approves or rejects it. Tab on an empty prompt cycles the agent mode, and Shift+Tab cycles the permission mode; both routes leave plan mode through the same path, so `/plan` followed by Tab does not leave the session running under plan mode's permissions.

---

## Goal system

The goal system lets MikMik work autonomously across multiple turns toward a single, verifiable objective. Instead of prompting repeatedly, you set the goal once and MikMik iterates until the goal is complete, paused, or the built-in runaway guard fires.

### Setting a goal

```
/goal <objective>
/goal --tokens 250K Refactor the auth module to use JWT tokens
```

Once a goal is set, MikMik begins working immediately. It continues across turns without waiting for user input until one of these conditions is met:

- The model calls `GoalComplete` with an audit summary and evidence
- You run `/goal pause` or `/goal clear`
- The runaway guard fires (200-turn hard limit)
- A token budget is set and exhausted

### Monitoring and controlling goals

```
/goal                  — show current goal status
/goal status           — show current goal status
/goal pause            — pause the active goal (you can resume it later)
/goal resume           — resume a paused goal
/goal clear            — delete the current goal entirely
/goal complete         — manually request a completion audit
```

### How completion works

When the model believes the objective has been met, it calls `GoalComplete` rather than simply responding. This tool requires two arguments:

- `audit_summary` — a concise description of what was accomplished
- `evidence` — specific, verifiable evidence (files changed, tests passing, output produced)

MikMik displays both to the user before marking the goal complete. The model is expected to genuinely audit the outcome before calling; calling without real evidence is rejected.

### Goal status lifecycle

| Status      | Meaning                                             |
|-------------|-----------------------------------------------------|
| `Active`    | Goal is set and work is ongoing                     |
| `Paused`    | Work paused by user; goal is preserved              |
| `Completed` | Model called `GoalComplete` with accepted audit |
| `Failed`    | Runaway guard fired or budget exhausted             |

### Disabling the goal system

```bash
MIKMIK_GOALS=0 mikmik
```

Set `MIKMIK_GOALS=0` in the environment to completely disable goal-related commands and the `GoalComplete`. Useful in environments where autonomous multi-turn execution is undesirable.

---

## Managed agents

Managed agents enable a **manager-executor** architecture where a manager model delegates subtasks to one or more executor agents running in parallel. The manager reasons about the high-level plan; executors carry out individual tasks.

### Enabling managed agents

```
/managed-agents enable
```

Or apply a built-in preset:

```
/managed-agents presets               — list available presets
/managed-agents preset <name>         — apply a preset (configures all parameters)
```

### Architecture

```
User → Manager model
         ├─ Executor 1 (e.g., implementing feature A)
         ├─ Executor 2 (e.g., writing tests for A)
         └─ Executor 3 (e.g., updating docs)
```

The manager model does not execute tools itself — it delegates to executor agents and synthesizes their outputs. Executors can run concurrently up to the `concurrent` limit.

### Configuration

```
/managed-agents configure manager-model  anthropic/claude-opus-4-6
/managed-agents configure executor-model anthropic/claude-sonnet-4-6
/managed-agents configure executor-turns 20
/managed-agents configure concurrent     3
/managed-agents configure isolation      on
```

Model format: `provider/model` (e.g., `anthropic/claude-opus-4-6`, `openai/gpt-4o`).

### Budget

The manager and every executor draw from one pool, because a sub-agent runs on its parent's cost tracker. When the pool is spent the run stops and reports what it cost against what was allowed.

`--max-budget-usd` on the command line overrides the configured budget for that run.

```
/managed-agents budget 5.00           — set a total $5 budget (0 to clear)
/managed-agents disable               — turn managed agents off
/managed-agents reset                 — remove the configuration entirely
/managed-agents setup                 — print the setup instructions
```

### Viewing configuration

```
/managed-agents status
```

Configuration persists to `~/.config/mikmik/settings.json` under `managed_agents`.

> **Preview feature.** The managed-agents API is under active development and may change in future releases.

---

## Personas and writing styles

A persona is an output style, selected through `/output-style` or the settings
screen, not a mode with its own command.

| Style | What it does |
|---|---|
| `asd-ste100` | Controlled technical writing: short sentences, active voice, one instruction each, technical terms untranslated |
| `caveman-lite` | Trimmed prose. Full sentences, nothing wasted |
| `caveman` | Drops articles and unnecessary verbs; compressed but readable |
| `caveman-ultra` | Fewest words that still carry the answer. Fragments over sentences |
| `rocky-lite` | Ordinary grammar with Rocky's vocabulary |
| `rocky` | Rocky from *Project Hail Mary*: dropped articles, `', question?'`, triple emphasis |
| `rocky-ultra` | Rocky throughout, emphasis used freely |

Code blocks, technical terms, error messages, file paths and git operations are
unchanged by every persona: the style governs prose only.

```
/output-style caveman-ultra
/output-style default          — back to the standard voice
```

Typing `caveman`, `rocky` or `normal` as a single word inside a prompt applies
the persona to that one turn without persisting it.

---

## Headless mode

Headless mode runs MikMik non-interactively, suitable for scripts, CI pipelines, and programmatic orchestration.

### --print flag

```bash
mikmik --print "refactor this function to use async/await"
mikmik -p "summarise the changes in this PR"
```

Processes the prompt and exits after printing the final response to stdout. No interactive UI is shown.

Input can also be piped via stdin:

```bash
cat my_prompt.txt | mikmik --print
echo "explain this code" | mikmik -p
```

### --output-format

```bash
mikmik --print --output-format json "..."
mikmik --print --output-format stream-json --verbose "..."
```

| Format        | Description                                                                      |
|---------------|----------------------------------------------------------------------------------|
| (default)     | Plain text output — only the final assistant message.                            |
| `json`        | Full message array as JSON (requires `--verbose`).                               |
| `stream-json` | Newline-delimited JSON stream of messages as they arrive (requires `--verbose`). |

`stream-json` is the format used by the Agent SDK transport. It emits every message event as it arrives, making it suitable for real-time processing pipelines.

---

## Budget control

Limit resource consumption per invocation using CLI flags:

```bash
mikmik --max-budget-usd 2.00 "..."   # Stop after spending $2.00
mikmik --max-turns 10 "..."          # Stop after 10 model turns
mikmik --max-tokens 50000 "..."      # Stop after 50,000 output tokens
```

When a limit is reached, MikMik exits with a corresponding error message:
- `Error: Reached max turns (<n>)`
- `Error: Exceeded USD budget (<amount>)`

These flags are intended for automated use where runaway sessions would be costly.

---

## The Buddy companion system

Every MikMik user gets a persistent companion derived deterministically from their user ID. The companion appears as a small sprite in the terminal UI and occasionally comments on activity.

### How companions are generated

The companion's visual traits (species, eyes, hat, rarity, shiny status, stats) are generated by hashing the user ID with a seeded PRNG (Mulberry32). This means the companion is always the same for a given user — it cannot be faked by editing config files, because the bones are regenerated from the hash on every read.

Only the soul (name, personality) is persisted, to `companion.json` in the config directory, and only after it has been "hatched" (named by the model on first encounter).

### Species

18 species are available: duck, goose, blob, cat, dragon, octopus, owl, penguin, turtle, snail, ghost, axolotl, capybara, cactus, robot, rabbit, mushroom, chonk.

### Rarity tiers

| Rarity    | Weight | Stars |
|-----------|--------|-------|
| common    | 60%    | ★     |
| uncommon  | 25%    | ★★    |
| rare      | 10%    | ★★★   |
| epic      | 4%     | ★★★★  |
| legendary | 1%     | ★★★★★ |

Rarity affects the floor value of the companion's stats. A legendary companion has a minimum stat floor of 50, while a common companion starts at 5.

### Stats

Each companion has five stats: DEBUGGING, PATIENCE, CHAOS, WISDOM, SNARK. One stat is the peak (higher rolls), one is the dump stat (lower rolls), and the rest are scattered around the rarity floor.

### Persistence

The stored format in `companion.json`:

```json
{
  "name": "Vortox",
  "personality": "a chaotic little axolotl who celebrates every bug as a feature",
  "hatched_at": "2026-04-05T18:14:38Z"
}
```

The bones (species, rarity, stats, eyes, hat, shiny) are never stored and are always recomputed from `hash(userId)`.

---

## Voice mode

```
/voice
```

Voice input using the device microphone. When active, spoken input is transcribed and submitted as a prompt. Transcription goes to an OpenAI Whisper-compatible endpoint, `https://api.openai.com/v1/audio/transcriptions` by default, with the `whisper-1` model. Both the URL and the model are configurable, so any compatible server works.

The `/voice` command is a toggle. `MIKMIK_VOICE_ENABLED=1` pre-enables voice mode and `MIKMIK_VOICE_DISABLED=1` turns it off.

Voice is a compile-time Cargo feature. A build made with `--no-default-features` drops it, along with the ALSA dependency it needs on Linux.

---

## Vim keybindings

```
/vim
```

Toggles vim-style modal keybindings for the input buffer. When enabled, the prompt input operates in normal/insert/visual modes, allowing navigation and editing with standard vim motions.

The setting persists to user settings. The `--vim` CLI flag enables vim mode for the session without persisting.

---

## Bridge and remote sessions

MikMik can be driven from a phone or another browser through a relay you host yourself. The CLI dials out and long-polls, so the machine running the session needs no inbound port.

```
/remote-control
```

Shows the relay the bridge resolved, where each value came from, and whether the token is usable. `/remote-control start` enables the bridge at startup.

The relay ships in `relay/` and runs in Docker. Setup, the settings block, the permission model and the client API are in [Remote Control](remote-control).

```
/session
```

Shows the current remote session URL and a QR code for scanning on mobile.

The connection lives inside the MikMik process. If the process dies, the connection is lost.

The web client sees the same permission prompts the terminal does, including the `bypassPermissions` warning gate, and answers them over the same path.

---

## Editor sessions

`mikmik acp` runs the agent as an [Agent Client Protocol](https://agentclientprotocol.com) server over stdio, which is how Zed and other ACP-aware editors drive it. Point the editor's agent configuration at the `mikmik` binary with the `acp` argument.

A session started this way reports what it can be reconfigured with, so the editor renders native pickers:

| Selector | Values |
|----------|--------------------------------------------------------|
| Model    | The models the active account serves                    |
| Account  | Every account with a credential                         |
| Effort   | The reasoning ladder the current model supports         |
| Mode     | Ask, accept edits, or bypass permissions                |

Those choices last for that session. Nothing is written to `settings.json`, so a session started from a terminal keeps its own model and effort. To change the starting values instead, set `config.model`, `config.provider` and `config.effort` in [Configuration](configuration).

Every turn is written to the same session store the terminal reads, so a conversation started in an editor outlives the editor:

| Method | What it does |
|---------------------|-------------------------------------------------------------|
| `session/list`       | The sessions on file, filtered by directory, a page at a time |
| `session/load`       | Reopens one and replays the whole transcript                  |
| `session/resume`     | Reopens one without the replay                                |
| `session/fork`       | Copies the conversation into a second session                 |
| `session/close`      | Writes the session out and lets go of it                      |
| `session/set_model`  | Picks the model, alongside the model selector above           |

A session with no name of its own is named after the first thing asked of it, and the name is reported as it is set.

Slash commands work too: the agent announces the whole set the terminal offers, and a prompt naming one is answered by the command rather than sent to the model. Commands that open a picker on a terminal answer in text here, so `/rewind` numbers the messages, `/hooks` prints what is configured, and `/import-config` prints the preview that `/import-config apply` then carries out.

Editing tools report each file they rewrote as a diff, and a stored todo list is published as the session's plan, so an editor draws both natively. Every tool call also names the files it is about, so an editor can follow the agent from file to file.

A permission request carries what it is approving, not just the tool's name: a write arrives as a diff against the file on disk, an edit as the text it replaces and the text replacing it, a command as the command. Four answers are offered, including rejecting a tool permanently.

The agent uses whatever the editor offered to host:

| Capability | What changes |
|--------------------------|--------------------------------------------------------------|
| `fs.read_text_file`      | Reads see the buffer the user is looking at, unsaved edits included |
| `fs.write_text_file`     | Writes go through the editor, so they are undoable            |
| `terminal`               | Commands run in the editor's shell and are shown as they run  |

Each is honoured on its own, and anything the editor does not host stays with the agent.

A session can also bring its own MCP servers: whatever `session/new`, `session/load`, `session/resume` or `session/fork` names is connected for that session alone, over stdio, HTTP or SSE, headers included. A request that names none shares the agent's configured servers.

Images in a prompt reach the model. Audio does not, and `initialize` says so.

For VS Code, which has no ACP client of its own, the repository ships an extension under `editors/vscode/`.

---

## AGENTS.md hierarchical memory

MikMik reads instruction files before every session. Four scopes are loaded, in this order:

1. **Managed** — every `*.md` file in `<config dir>/rules/`, sorted by name.
2. **User** — `<config dir>/AGENTS.md`, then `<config dir>/CLAUDE.md`.
3. **Project** — `AGENTS.md`, then `CLAUDE.md`, in the project root.
4. **Local** — `.mikmik/AGENTS.md`, then `.mikmik/CLAUDE.md`, in the project root.

`AGENTS.md` is preferred (universal cross-tool standard); `CLAUDE.md` is loaded next at every scope for compatibility with other Claude tooling. If both exist at the same scope, both are loaded.

The project root is the git repository the working directory sits in, or the working directory itself when there is none. A session started in a subdirectory therefore reads the repository's files, not the subdirectory's.

Each file may pull in others with a line starting `@include `. The path resolves relative to the including file, `~` is expanded, nesting is bounded, and a cycle is skipped with an HTML comment rather than a failure. A missing include leaves a comment too, so nothing fails silently.

YAML frontmatter is stripped before the content reaches the model.

The `InstructionsLoaded` hook event fires when instruction files are loaded.

The `/memory` command opens the memory management UI for viewing, editing, and organising instruction files.

---

## Security and permissions

### Permission modes

| Mode                | Description                                                                 |
|---------------------|-----------------------------------------------------------------------------|
| `default`           | Prompts the user before executing dangerous or write operations.            |
| `plan`              | Read-only; write and execute require explicit approval.                     |
| `acceptEdits`       | Accepts file edits without prompting; other tools still prompt.             |
| `bypassPermissions` | Skips the permission system entirely. Intended for trusted automation only. |

The active mode is set with `--permission-mode <mode>` or via the `PermissionRequest` hook.

### Tool risk classification

Every tool declares a permission level that determines the default behaviour:

| Level       | Examples                        | Default behaviour                     |
|-------------|---------------------------------|---------------------------------------|
| `Forbidden` | Directly destructive commands   | Always blocked, in every mode         |
| `Dangerous` | Sandbox bypass                  | Prompt required                       |
| `Execute`   | Bash                            | Prompt required                       |
| `Write`     | Write, Edit                     | Prompt in default mode                |
| `ReadOnly`  | Read, Glob, Grep, WebFetch      | Allowed automatically                 |
| `None`      | Purely informational tools      | Allowed automatically                 |

`None` and `ReadOnly` never reach the permission manager at all, so no rule can gate them.

### Bash command risk classification

Within the Bash tool, commands are further classified by analysing the command string against known patterns. A command the classifier rates `Critical` (`rm -rf /`, a fork bomb, `dd if=`) is raised to `Forbidden`, which no permission mode can approve.

The `PermissionRequest` hook can intercept any tool call before the user prompt is displayed, allowing automated allow/deny decisions based on context.

---

## Output styles

```
/output-style [style]
```

Controls how MikMik writes its responses. The command opens a picker when called without arguments. Eleven styles ship built in:

| Style | What it does |
|---|---|
| `default` | The standard voice |
| `concise` | Shorter answers, less preamble |
| `explanatory` | Explains the reasoning behind each step |
| `learning` | Teaches while it works |
| `asd-ste100` | Controlled technical writing (see Personas above) |
| `caveman-lite`, `caveman`, `caveman-ultra` | Three compression levels |
| `rocky-lite`, `rocky`, `rocky-ultra` | Three intensity levels |

More styles are loaded from disk as `.md` (`# Label`, description, then the prompt) or `.json` files.

A style governs prose only. Code blocks, technical terms, error messages, file paths and git operations are unchanged.

---

## Custom commands

There are two ways to add one.

**In `settings.json`,** under the `commands` map:

```json
{
  "commands": {
    "review": {
      "template": "Review the output of `git diff --staged`. Focus on correctness, edge cases, and naming.",
      "description": "Review the staged git diff",
      "agent": "plan"
    }
  }
}
```

`$ARGUMENTS` in the template is replaced with whatever the user types after the command name. `agent` and `model` are optional overrides. See [Configuration](configuration.md#custom-slash-commands).

**As a markdown file,** in `.mikmik/commands/` for one project or `<config dir>/commands/` for every project. The file name is the command name, so `.mikmik/commands/review.md` becomes `/review`, and the body is the prompt.

Custom commands appear alongside built-in commands in the `/` menu, and plugins contribute their own `commands/` directory the same way.

Skills work through a third set of directories, `.mikmik/skills/`, `.agents/skills/` and `<config dir>/skills/`. Project skill directories are found by walking up from the working directory, so a skill defined at the repository root is visible from every subdirectory. A skill defined twice keeps the first one found and reports the duplicate.

---

## Formatters

A formatter runs after every `Write` and `Edit`, without a hook. Declare them under `formatter` in `settings.json`, keyed by a name of your choosing:

```json
{
  "formatter": {
    "prettier": {
      "command": ["prettier", "--write", "$FILE"],
      "extensions": [".ts", ".tsx", ".js", ".jsx", ".json", ".css", ".md"]
    },
    "ruff": {
      "command": ["ruff", "format", "$FILE"],
      "extensions": [".py"]
    },
    "rustfmt": {
      "command": ["rustfmt", "$FILE"],
      "extensions": [".rs"]
    }
  }
}
```

| Field        | Description                                                              |
|--------------|--------------------------------------------------------------------------|
| `command`    | The command and its arguments. `$FILE` or `{file}` is the file's path    |
| `extensions` | The extensions this formatter handles, each with its leading dot         |
| `disabled`   | `true` keeps the entry but stops it running                              |

The file's path is appended when the command names neither `$FILE` nor `{file}`. Only the first formatter whose extensions match runs. It is given 30 seconds, and its failures are ignored: a file that did not get formatted is not worth interrupting a turn for.

---

## Environment management

### --add-dir

```bash
mikmik --add-dir /path/to/additional/project "..."
```

Grants MikMik read access to an additional directory outside the working directory. Useful when a task spans multiple repositories or when config files live outside the project root.

Multiple `--add-dir` flags can be combined.

Each directory is named and shown to the model, so it can reach the directory without being handed an absolute path. The working directory is always `&main`; every other directory takes its name from its last path component, lowercased with anything outside `[a-z0-9._-]` folded to a dash, so `_ai-engine` becomes `&_ai-engine` and `My Project (API)` becomes `&my-project-api`. Two directories with the same name are told apart with a counter: `&lib` and `&lib-2`. The names are derived from the configuration, so the same flags always produce the same names and nothing is persisted.

Path arguments then accept `&<root-name>/<relative-path>` for a file and `&<root-name>` for the directory itself. A relative path without `&` still resolves against the working directory, and a mistyped root name is rejected with the list of known roots rather than being joined onto the working directory. The same names come from `additional_dirs` and `workspace_paths` in `settings.json`.

### Environment variables in config

Environment variables can be set in `settings.json` under an `env` key. These are injected into tool executions:

```json
{
  "env": {
    "NODE_ENV": "development",
    "DATABASE_URL": "postgres://localhost/mydb"
  }
}
```

---

## LSP integration

The `LSP` tool provides code intelligence by talking to a language server.
Servers for common projects are detected automatically; the rest are declared
in `settings.json` under `lsp_servers`.

### Operations

| Operation         | Description                                                         |
|-------------------|---------------------------------------------------------------------|
| `hover`           | Type information and documentation for the symbol at a position     |
| `definition`      | Where a symbol is defined                                           |
| `type_definition` | Where the symbol's type is defined                                  |
| `implementation`  | What implements the interface or trait at a position                |
| `references`      | Every reference to a symbol                                         |
| `symbols`         | One file's symbols, or the workspace's                              |
| `diagnostics`     | Errors and warnings for a file                                      |
| `rename`          | Rename a symbol everywhere it is used                               |
| `rename_file`     | Move a file or directory and update every reference                 |
| `code_actions`    | List the fixes and refactorings offered, and apply one              |
| `status`          | Which servers are configured, running, or missing their binary      |
| `capabilities`    | What a server says it supports                                      |
| `reload`          | Re-read the configuration and push it to the servers again          |
| `request`         | Send a raw LSP request                                              |

### Input schema

```json
{
  "action": "hover",
  "file": "src/main.rs",
  "line": 42,
  "symbol": "parse_config"
}
```

`symbol` names the token on that line, which is more reliable than counting
columns. The full parameter list is in [tools.md#lsp](tools.md#lsp).

### Configuration

LSP servers are configured in `settings.json` under `lsp_servers`. The field
list, the routing rules and the precedence rules are in
[tools.md#lsp](tools.md#lsp).

If no language server is running for the file, the tool names the reason for
each server that could have served it. Path resolution is relative to the
current working directory.
