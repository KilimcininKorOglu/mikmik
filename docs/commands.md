# MikMik Slash Commands Reference

This document is the complete reference for every slash command available in MikMik, the Rust reimplementation of Claude Code CLI. Commands are invoked by typing `/command-name` at the REPL prompt.

---

## Table of Contents

1. [Command System Overview](#command-system-overview)
2. [Shell Commands](#shell-commands)
3. [Session & Navigation](#session--navigation)
4. [Model & Provider](#model--provider) — `/model`, `/providers`, `/connect`, `/thinking`, `/effort`, `/advisor`, `/fast`
5. [Configuration & Settings](#configuration--settings) — `/config`, `/turns`, `/poke`, `/yolo`, `/keybindings`, `/permissions`, `/hooks`, `/privacy-settings`, `/mcp`, `/output-style`, `/theme`, `/statusline`, `/timeline`, `/vim`, `/voice`, `/terminal-setup`
6. [Code & Git](#code--git) — `/commit`, `/diff`, `/undo`, `/review`, `/security-review`, `/init`, `/search`
7. [Search & Files](#search--files) — `/files`, `/context`
8. [Memory & Context](#memory--context) — `/memory`, `/memories`, `/usage`, `/cost`, `/stats`, `/status`, `/insights`
9. [Agents & Tasks](#agents--tasks) — `/agents`, `/tasks`, `/todos`, `/goal`, `/managed-agents`, `/agent`
10. [Planning & Review](#planning--review) — `/plan`, `/ultraplan`, `/ultrareview`
11. [MCP & Integrations](#mcp--integrations) — `/mcp`, `/skills`, `ultracode`, `/plugin`, `/chrome`
12. [Authentication](#authentication) — `/login`, `/logout`, `/accounts`, `/switch`, `/refresh`
13. [Display & Terminal](#display--terminal) — `/theme`, `/output-style`, `/statusline`, `/timeline`, `/vim`, `/terminal-setup`, `/mobile`, `/color`, `/stickers`, `/buddy`
14. [Diagnostics & Info](#diagnostics--info) — `/doctor`, `/version`, `/update`
15. [Export & Sharing](#export--sharing) — `/export`, `/copy`
16. [Advanced & Internal](#advanced--internal) — `/thinking`, `/connect`, `/fork`, `/effort`, `/summary`, `/remote-control`, `/remote-env`, `/workspace`, `/sandbox-toggle`, `/think-back`, `/thinkback-play`
17. [Command Availability](#command-availability)

---

## Command System Overview

A command name is resolved in this order. The first match wins, so a built-in
name cannot be shadowed:

1. **Built-in commands**, by name or by alias.
2. **Templates** from the `commands` map in `settings.json`.
3. **Skills** discovered in `.mikmik/skills/`, `.agents/skills/`, the config
   directory, and the paths and git URLs `skills` names.
4. **Plugin commands**, from every enabled plugin's `commands/` directory.

Commands support aliases: `/h`, `/?` and `/help` all reach the same handler.

### What a command returns

A command either prints text, opens a picker, sends a prompt to the model, or
changes the session. Some change the configuration and print at the same time,
which is why `/effort high` both switches the level and says so.

A command marked hidden is left out of `/help` and the palette. It still runs
when typed.

### Usage Syntax

```
/command-name [arguments]
```

Everything after the command name arrives as a single string.

---

## Shell Commands

A line that starts with `!` runs as a shell command instead of going to the
model.

```
!ls -la
!git status
!!literal      sends "!literal" to the model
```

The command runs in the same shell the model's Bash tool uses, so `cd`,
`export`, a function you define and an alias you set outlive the call and both
see the same shell. The 120-second timeout and the 100 KB output limit are the
tool's, unchanged.

Three things follow from this being a command you typed rather than one the
model issued:

- **No permission prompt.** The permission rules bound what the model may do.
  Asking you to confirm a command you just wrote adds nothing. The Critical-risk
  classifier still applies, so `rm -rf /` is refused on this path too.
- **The output never reaches the model.** It is drawn in the transcript as a
  system annotation, which is not part of the conversation, so a shell command
  costs no tokens and does not enter the context.
- **Plan mode refuses it.** Plan mode promises to touch nothing, and a shell
  command is a way of touching something.

While a turn is streaming, Enter on a bang line hands the text back to the
prompt rather than queueing it: a queued bang would reach the model as a plain
message once the turn ended.

A non-zero exit code is drawn as a warning; the command itself is shown above
its output. The call blocks the interface while it runs, so a long command
leaves the display still until it finishes or the timeout fires.

---

## Session & Navigation

### /help
**Aliases:** `h`, `?`

Display the available commands with their descriptions, grouped by category. Commands marked hidden are left out; they still run when typed.

```
/help
/h
/?
```

---

### /clear
**Aliases:** `reset`, `c`

Clear the current conversation history. The session id and its on-disk file are retained — only the in-memory message list is wiped, so you stay in the same session. Use `/new` to start a genuinely fresh session instead.

```
/clear
```

---

### /new

Start a fresh session, mirroring opencode's `/new`. The transcript resets to a blank home and a brand-new session id begins, while your current model, provider, effort level and working directory carry over. The new session is *lazy* — it is not written to disk until your first message, so opening `/new` without typing anything leaves no trace.

Unlike `/clear` (which keeps the same session id and history file), `/new` opens a clean, separate session.

```
/new
```

---

### /move

Re-home the current session to another worktree or directory of the **same project**, mirroring opencode's `/move`. Any uncommitted changes in the current directory are carried over to the destination and reset in the original location; the model is informed of the new working directory on its next turn.

The destination must belong to the same git repository (typically a linked `git worktree`); moving to an unrelated project is refused. Pass `--no-changes` to re-home the session without relocating working-tree changes.

```
/move <directory>
/move ../myapp-feature
/move --no-changes /path/to/other/worktree
```

**Adaptation note:** opencode presents an interactive worktree picker and can create a new worktree on the fly. MikMik takes the destination directory as an argument and re-homes the live session's working directory (mikmik has no separate session-per-worktree registry). Uncommitted changes are relocated with `git diff`/`git apply` and the source is reset with `git checkout` (index preserved) plus `git clean` (untracked removed), matching opencode's `move-session` change handling.

---

### /exit
**Aliases:** `quit`

Exit the MikMik REPL. Equivalent to pressing `Ctrl+D`. Unsaved session state is flushed before exit.

```
/exit
/quit
```

---

### /resume
**Aliases:** `continue`

Resume a previous session from the session store. Displays a list of recent sessions with timestamps and summaries. Select one to restore its message history and file state.

```
/resume
/resume <session-id>
```

Sessions are stored in one directory shared by every project, so the list mixes them together. Keys in the browser:

| Key      | Action                                                        |
|----------|---------------------------------------------------------------|
| `↑` `↓`  | Move between sessions                                         |
| `Enter`  | Resume the selected session                                   |
| `r`      | Rename it                                                     |
| `a`      | Show each session's working directory under its row           |
| `p`      | Show the selected session's full ID and untruncated directory |
| `Esc`    | Close                                                         |

Both toggles are off again every time the browser opens. Resuming moves the working directory as well as the transcript, so tools run where that session left off.

---

### /session
**Aliases:** `remote`

Show the current session, or list the stored ones.

```
/session         — the current session; the remote URL when the bridge is up
/session list    — the ten most recent sessions
```

Resume one with `/resume <id>`.

---

### /fork

Fork the current session into a new independent one, so two approaches can be
explored without losing either. The argument is a message index, not a name.

```
/fork        — fork at the current end of the conversation
/fork 5      — fork after message 5
```

---

### /rename

Rename the current session. The new name is used in session listings and exports.

```
/rename <new-name>
```

---

### /rewind

Rewind the conversation to a previous message. Displays a numbered list of messages; enter a number to truncate history to that point and resume from there.

```
/rewind
/rewind <message-index>
```

---

### /checkpoint

List the points the conversation can be returned to, or return to one. A checkpoint is recorded at the end of every turn.

```
/checkpoint list
/checkpoint restore <n>
```

Restoring drops the turns after that point from the conversation. They are not deleted: the session transcript keeps them on a sibling branch, and the checkpoint before them can be restored again.

Not to be confused with `/checkpoints` (plural), which lists the turns that changed files, or `/revert`, which rolls those files back.

---

### /compact

Summarize and compress the conversation history to reduce context window usage. The model is asked to produce a dense summary of the prior exchange; that summary replaces the raw messages.

```
/compact
```

---

## Model & Provider

### /model

Open the interactive model picker. The selected model is used for all subsequent inference in the current session.

```
/model
```

The picker lists every provider the current configuration can reach, grouped under per-provider headings. Rows are grouped by provider rather than by model, because two providers can serve the same model id through different credentials and endpoints. Selecting a row switches the provider as well as the model, so `/connect` is only needed to add credentials, not to move between providers you already have.

Type to filter across the whole list. If nothing matches, pressing Enter uses what you typed as a model id, which is how you reach a model the picker does not know about.

An account earns a section by having an entry in `providers` or a credential, so a provider you configured but never gave a key to is listed and fails the moment you pick from it. `Ctrl+O` hides those sections, leaving only accounts a credential resolves for, and hides nothing in a single-account list. The filter is off again every time the picker opens.

`Tab` and `Shift+Tab` move the cursor to the first row of the next and previous account, which is faster than scrolling past a long section. Both walk only the sections currently on screen, so a filter or `Ctrl+O` narrows the route with them.

`Ctrl+F` stars the model under the cursor. A starred model is drawn with `★` and sorted to the top of its own account's section, not to the top of the list, so the sections stay intact. Stars are saved to `favoriteModels` in `settings.json` as `account/model`, which is why the same model reached through two accounts is two separate stars.

The command takes no arguments; anything after `/model` is ignored.

Local runtimes and custom endpoints report their real model list from a live query when the session is on them. Other sections show the models.dev catalog, so a locally-loaded model may not appear until you switch to that provider.

---

### /providers

List all configured AI providers and their connection status. Shows provider name, base URL, and whether credentials are present.

```
/providers
```

---

### /connect

Connect to a remote AI provider or configure a custom provider endpoint. Supports OpenAI-compatible APIs, Anthropic direct, and others.

```
/connect
/connect <provider-name>
/connect openai https://api.openai.com/v1
```

---

### /thinking

Configure extended thinking for the current session. Extended thinking allows the model to reason through problems before responding, at the cost of additional tokens.

```
/thinking
/thinking on
/thinking off
```

See also `/effort` for a higher-level interface to thinking depth.

---

### /effort

Set the reasoning effort level. With no argument it reports the level in force,
and says `unset` when nothing was chosen: unset is not the same as `medium`,
because it sends no reasoning configuration at all.

| Level       | Description                                                    |
|-------------|----------------------------------------------------------------|
| `none`      | Thinking disabled; the model answers directly                  |
| `minimal`   | The smallest reasoning budget                                  |
| `low`       | Quick, with minimal overhead                                   |
| `medium`    | Balanced. `normal` is accepted as an alias                     |
| `high`      | Deep reasoning; slower responses                               |
| `xhigh`     | Extended reasoning for hard problems                           |
| `max`       | Maximum available budget                                       |
| `ultracode` | Maximum reasoning plus the ultracode delegation workflow       |

```
/effort              — report the level in force
/effort high
/effort ultracode
```

Only `low`, `high` and `normal` also move the output token limit, to 4096,
32768, and back to the model default. The other levels leave it alone.

---

### /advisor

Set the second model that reviews the work, and how it reviews it. The advisor is a critic: it says what is wrong rather than continuing the work.

There are three ways to reach it. The main model calls the [`Advisor`](tools.md#advisor) tool itself when it judges a decision hard to reverse or a call genuinely close. In `runtime` mode a watcher reads every turn on its own and speaks unasked. You can also run it yourself over the last reply with `review`.

```
/advisor                            show the current setting
/advisor claude-opus-4-6            set the advisor model
/advisor openai/gpt-4o              set a model on another provider
/advisor anthropic:personal/sonnet  run the advisor on another account
/advisor review                     have the advisor review the last reply
/advisor mode runtime               let it read every turn on its own
/advisor status                     show the mode, the roster and the spend
/advisor dump                       show what the watching advisor read
/advisor off                        disable the advisor
/advisor unset                      disable the advisor
```

#### Modes

`mode` takes `tool` (the default: the model consults the advisor when it decides to), `runtime` (a watcher reads every turn and interrupts), `both`, or `off`. A new mode takes effect on the next session, because the roster and the tool list are assembled at startup. [Advisor configuration](configuration.md#advisor) covers what the watcher does with what it finds, how to run several at once, and how to tell them what to watch for.

`status` reports the mode, the model and the account it resolves to, the roster entries with their files, any `ADVISOR.md` in force, and what the advisor's model has spent so far. `dump` reads back what a watcher saw and said in this session, from the watcher's own transcript.

Any non-empty model ID is accepted. The advisor runs client-side, so it works on every configured provider, not only Anthropic. A bare ID runs against the session's active provider; `provider/model` targets a specific one.

#### Running the advisor on a second account

`provider:account/model` authenticates the advisor as one of your stored logins while the session stays on the active one. This is the only way to use two accounts at once: [`/switch`](#switch) moves a single pointer, so it changes the main model and the advisor together.

```
/accounts                           list stored accounts and their IDs
/advisor anthropic:personal/sonnet  main model on the active account,
                                    advisor on "personal"
```

Only `anthropic` and `codex` keep separate accounts; every other provider stores a single credential, and naming an account for one is rejected. The account ID is checked when you set it, so a typo is reported straight away rather than surfacing later as a failed advisor call.

Accounts do not pool quota. Each one keeps its own rate limit, so this splits usage between them rather than combining them.

A colon inside a model ID is not read as an account: `ollama/llama3:8b` still names a model, because the account separator is only looked for before the `/`.

The setting persists to `~/.config/mikmik/settings.json` under `advisorModel`. `/advisor review` uses a new model immediately; the `Advisor` tool is offered to the main model from the next session, because the tool list is assembled at startup.

Advisor calls are capped at two per turn. Their tokens are added to the session cost on their own model's line, at that model's rates.

---

### /fast
**Aliases:** `speed`

Toggle fast mode. In fast mode, MikMik switches to the active provider's smaller, faster model for quick responses. Useful when you want rapid answers and deep reasoning is not required.

```
/fast          — toggle fast mode on/off
/fast on       — enable fast mode
/fast off      — disable fast mode
```

Setting persists to `~/.config/mikmik/ui-settings.json`.

---

## Configuration & Settings

### /config
**Aliases:** `settings`

Show or change configuration values. With no arguments it prints the whole
configuration as JSON and the usage below it.

```
/config                                — print the configuration
/config get <key>                      — read one value
/config set theme dark
/config set output-style asd-ste100
/config set model claude-opus-4-6
/config set permission-mode accept-edits
/config unset model                    — back to the provider default
/config unset output-style
```

Four keys are readable and writable here: `theme`, `output-style`, `model` and
`permission-mode`. Everything else is edited in `/settings` or in
`settings.json`. `unset` accepts `model` and `output-style` only.

---

### /turns

Show or change how many agentic turns one run may take before it stops.

```
/turns              show the limit in force
/turns 25           stop after 25 turns
/turns off          no limit
/turns default      back to the configured default
```

The limit persists for the session and is saved as `maxTurns` in `settings.json`.
`--max-turns` sets it for one launch; an agent definition's own `max_turns` wins
over both while that agent is active, and `/turns` names that agent's limit when
one is in force.

`off`, `none`, `unlimited` and `0` all mean no limit. Reaching the limit normally
spends one final turn asking the model to summarise its progress; the
`degradationSummary` setting turns that off.

---

### /poke

Show or change whether unfinished todos nudge the model between turns.

```
/poke            show whether the nudge is on
/poke on         nudge the model about unfinished todos
/poke off        stop nudging
/poke default    back to the configured default (on)
```

After a turn that leaves todos unfinished, MikMik appends a short reminder
listing what is left, so the run continues instead of stopping halfway. Turn it
off for a session where you drive each step yourself. The setting is saved as
`autoPoke` in `settings.json`; `default` removes the key rather than writing
the default value into it.

---

### /yolo

Run every tool without asking for permission.

```
/yolo            switch it the other way
/yolo on         stop asking for permission
/yolo off        go back to asking
/yolo status     show the mode in force
```

Yolo mode is `permissionMode: "bypassPermissions"` under a shorter name, so
there is no separate setting for it: `/yolo on`, `--dangerously-skip-permissions`
and setting `permissionMode` by hand all describe the same state. Every tool
runs unasked, including ones that write files and run shell commands.

Shift+Tab cycles the mode too, but only for the session. `/yolo` writes it to
`settings.json`, so it survives a restart. `/yolo off` returns to `default`
rather than to whatever mode preceded bypass: nothing records that, and
guessing `acceptEdits` would hand back more than was taken away.

Turning it on raises the bypass warning first, unless you have already accepted
it once on this machine. Refusing puts the previous mode back, including the
value `/yolo on` had already written to `settings.json`. See [Permission
Modes](configuration.md#permission-modes).

---

### /keybindings

Open the interactive keybinding configurator. Displays all bound actions with their current shortcuts. Select an action to rebind it. Changes are written to `~/.config/mikmik/keybindings.json`.

```
/keybindings
```

See [keybindings.md](./keybindings.md) for the full keybindings reference.

---

### /permissions
**Aliases:** `allowed-tools`

View and manage tool permission rules. Permissions control which tools run
without prompting, which are blocked, and which always ask.

```
/permissions                    — show the current permissions
/permissions set <mode>         — default, accept-edits, bypass-permissions, plan
/permissions allow Bash         — allow one tool
/permissions deny Write         — deny one tool
/permissions reset              — clear the overrides
```

`allow` and `deny` write a rule into `permissionRules` in `settings.json`, which
is the list a tool call is decided against. Each is a single verdict per tool:
`allow` after `deny` on the same tool replaces the denial rather than adding a
second rule. A rule that names a path, which the permission dialog writes when
you approve a tool for one file, is left alone by both. `reset` clears every
rule and returns the mode to `default`.

A settings file written before this carried the verdicts in `allowed_tools` and
`disallowed_tools`, where nothing read them. Those entries move into
`permissionRules` the first time the file is loaded, so a tool you denied then
is denied now.

`/permissions set bypass-permissions` raises the bypass warning first. See
[Permission Modes](configuration.md#permission-modes).

---

### /hooks

Show the event hooks configured in `settings.json`. In the TUI it opens an
overlay that also adds, edits and removes them; elsewhere it prints the listing.
It takes no arguments.

```
/hooks
```

Settings hooks fire on six events: `PreToolUse`, `PostToolUse`, `PostModelTurn`,
`Stop`, `UserPromptSubmit` and `Notification`. Plugins declare hooks separately,
on a longer list of events. See [Hooks](hooks.md).

---

### /privacy-settings

Open MikMik privacy settings. Launches a browser to the Anthropic privacy portal where you can review data usage preferences, conversation retention, and account privacy options.

```
/privacy-settings
```

---

### /mcp

Manage Model Context Protocol (MCP) servers. They extend MikMik with external
tools, resources, and prompt templates.

```
/mcp                          — list the configured servers with live status
/mcp list                     — the same
/mcp status                   — detailed connection status for every server
/mcp auth <server>            — show the OAuth instructions for a server
/mcp connect <server>         — reconnect a disconnected server
/mcp logs <server>            — recent errors and logs for a server
/mcp resources [server]       — list resources from connected servers
/mcp prompts [server]         — list prompt templates from connected servers
/mcp get-prompt <server> <prompt> [key=value ...]   — expand a prompt template
```

Servers are added and removed by editing the `mcpServers` key in
`~/.config/mikmik/settings.json`, not through this command.

---

### /output-style

Select the voice the model writes in. The chosen style's prompt is added to the
system prompt, so it changes how answers are written, never what the model may
do. The choice is persisted and takes effect on the next request.

```
/output-style                    — list the styles and show the current one
/output-style asd-ste100
/output-style default            — back to the standard voice
```

| Style | What it does |
|---|---|
| `default` | Standard MikMik responses |
| `concise` | Short and direct, minimal explanation |
| `explanatory` | Reasoning, alternatives and pitfalls spelled out |
| `learning` | Explains patterns and decisions as it implements them |
| `asd-ste100` | Controlled technical writing: short sentences, active voice, one instruction each, technical terms untranslated |
| `caveman-lite` / `caveman` / `caveman-ultra` | Telegraphic prose at three intensities |
| `rocky-lite` / `rocky` / `rocky-ultra` | Rocky from *Project Hail Mary*, at three intensities |

Code blocks, technical terms, error messages, file paths and git operations are
unchanged by every style.

Your own styles go in `~/.config/mikmik/output-styles/` or a repository's
`.mikmik/output-styles/`, as `.md` or `.json`; they are listed automatically.
Plugin-defined styles are listed too.

Typing `caveman`, `rocky` or `normal` as a single word inside a prompt applies
that persona to one turn without persisting it.

---

### /theme

Open the interactive theme picker. Preview and select a colour theme for the
MikMik TUI.

```
/theme
/theme dark
/theme light
/theme deuteranopia
```

The theme decides the colours the interface puts on a state: an error, a
success, a warning, and the accent that marks who a line came from. Layout
colours (borders, padding, the input frame) are the same under every theme.

`deuteranopia` is the reason the distinction matters. Red-green colour
blindness makes the usual error red and success green hard to tell apart, so
that theme replaces the pair with blue, yellow and grey.

---

### /statusline

Choose which built-in items the TUI status bar shows: cost, token count, model name and elapsed time. With no arguments it reports the current setting, and names the external status line command if one is configured.

```
/statusline
/statusline show cost
/statusline hide tokens
/statusline show all
```

The external status line is a separate feature and is configured in `settings.json` rather than here. It runs a command of your own and shows its output in its own rows above the footer; see [Configuration](configuration.md#status-line).

---

### /timeline

Show, hide or clear the live execution timeline panel. The panel lists every tool call and finished turn as it happens, with the status of each step, how long it took, and what the turn spent.

```
/timeline
/timeline show
/timeline hide
/timeline clear
```

With no argument it toggles, the same as `Ctrl+Shift+L`. Once the panel has focus, `Up` and `Down` move its cursor, `Right` expands the selected row, `Left` collapses it and `Esc` returns to the prompt.

`Enter` always belongs to the prompt, so a message can still be sent while the panel holds focus.

The panel takes its share out of the transcript: on a wide terminal it sits to the right, on a narrow one along the bottom, and below 32×8 it is not drawn at all.

Recording is off until `timelineEnabled` is turned on in `/settings` (see [Configuration](configuration.md#interface)); while it is off nothing is collected and every entry point says so.

---

### /vim

Toggle vim keybinding mode on or off. In vim mode the input field behaves like a vim editor (normal/insert/visual modes). Persisted to config.

```
/vim
/vim on
/vim off
```

---

### /voice

Enable or disable voice input (push-to-talk). There is no speech output.

```
/voice           — toggle
/voice on
/voice off
/voice status
```

`Alt+V` starts recording; `Alt+V` or `Esc` stops it and transcribes. The setting
persists to `~/.config/mikmik/ui-settings.json`.

Transcription goes to a Whisper-compatible API. `OPENAI_API_KEY` is the key it
looks for, with `ANTHROPIC_API_KEY` as a fallback. Point it at a local server
with `WHISPER_ENDPOINT_URL`, for example
`http://localhost:8080/v1/audio/transcriptions`.

On Linux, ALSA must be installed (`sudo apt install libasound2-dev`), and a
build made with `--no-default-features` has no voice support at all.

---

### /terminal-setup

Run the terminal capability detection and setup wizard. Checks for true-color support, font ligatures, Unicode rendering, and configures MikMik accordingly.

```
/terminal-setup
```

---

## Code & Git

### /commit

Ask the model to commit the work in the repository. It reads `git status` and
`git diff HEAD`, so staged and unstaged changes are both in scope, splits
unrelated changes into separate commits, stages each group by explicit path,
and follows the message conventions it reads from `git log`.

The split is the model's judgement, not a computed one. Stage the files
yourself first if you want a particular grouping; the model commits what it
finds either way.

```
/commit
/commit fix the parser         — pass your own message
```

---

### /diff

Show git diff output for the working directory.

```
/diff           — every unstaged change
/diff --stat    — a summary of the changed files
/diff --staged  — the staged changes
/diff <ref>     — against a branch, tag, or commit
```

---

### /undo

Revert every file change the most recent assistant turn made. It takes no
arguments.

```
/undo
```

`/revert [<n>|<uuid>]` reverts a specific turn instead, and `/checkpoints` lists
the turns that changed files.

---

### /review

Send a diff to the model for a structured review, and optionally post the
result as a comment on the matching GitHub pull request.

```
/review              — the staged changes (`git diff --cached`)
/review main         — `git diff main...HEAD`
/review origin/main
```

Posting to GitHub needs `GITHUB_TOKEN` with repo scope. The PR number is
detected from `git remote`, or taken from `CLAUDE_PR_NUMBER`.

---

### /security-review

Run a security-focused review pass. The model looks specifically for vulnerabilities, credential exposure, injection risks, and other security concerns in modified files.

```
/security-review
/security-review <file-path>
```

---

### /init

Create an `AGENTS.md` in the working directory from a template. That file is
read at the start of every session and injected into the system prompt. An
existing `AGENTS.md` is left alone.

```
/init
```

---

### /search

Search past sessions, not the codebase. Titles and message content are matched
in the local SQLite session database, and the 50 best matches are returned,
newest first.

```
/search <query>
/search refactor authentication
```

To search the codebase, ask the model: it has `Grep` and `Glob`.

---

## Search & Files

### /files

List the files mentioned in the conversation. Paths are found in the message
text and checked against the filesystem, so only ones that exist are listed. It
takes no arguments.

```
/files
```

---

### /context
**Aliases:** `/ctx`, `/ctx-viz`, `/context-visualizer`

Report context window usage. Prints two figures: the token count the API returned for the last request, measured against the active model's own window, and an estimate of the current messages split into conversation, tool results and attachments. Ends with a compaction recommendation once the window passes 75% full.

```
/context
```

The system prompt and the tool definitions are not broken out; nothing records their size. The measured figure includes them.

See [Advanced Features](advanced.md#context-window-management) for what each figure covers and why they describe different moments.

---

## Memory & Context

### /memory

Show and edit the `AGENTS.md` files that give the session its project context.

```
/memory                 — show every AGENTS.md that was found
/memory edit            — open the project AGENTS.md in your editor
/memory edit global     — open ~/.config/mikmik/AGENTS.md
/memory clear           — empty the project AGENTS.md
/memory clear global    — empty the global one
```

Locations, in priority order: `<project>/.mikmik/AGENTS.md`,
`<project>/AGENTS.md`, then `~/.config/mikmik/AGENTS.md`. Use `/init` to create
one from a template.

While auto memory is on, `/memory` also names the second store and points at
`/memories`.

---

### /memories

Inspect or clear the [auto memory directory](configuration.md#auto-memory), the
second memory store. `/memory` above is the one you write and commit;
`/memories` is the one MikMik keeps for itself, outside the checkout.

```
/memories                 — path, MEMORY.md, and the file list
/memories stats           — file count, total size, and MEMORY.md against its caps
/memories diagnose        — why consolidation has or has not run
/memories clear           — list what would be deleted
/memories clear confirm   — delete it
/memories rebuild         — clear the consolidation state so the time gate opens
```

`clear` needs the literal word `confirm`. Without it, nothing is deleted and the
command lists the files instead. The directory itself is kept either way, so the
next session writes into it again.

`diagnose` reports all three consolidation gates: hours since the last run,
transcripts newer than it, and whether another process holds the lock. All three
have to pass at the end of a turn.

`rebuild` does not start a consolidation. A slash command has no session to
spawn a sub-agent in, so it deletes the state file, which opens the time gate;
the session gate still decides whether the run happens.

Every subcommand reports that the feature is off when `autoMemoryEnabled` is
unset, because there is then no directory to report on.

---

### /usage

Display a detailed token usage breakdown for the current session. Shows input tokens, output tokens, cache reads, cache writes, and estimated cost per API call.

```
/usage
```

---

### /cost

Show the total token usage and estimated cost for the current session. Provides a quick summary without the per-call breakdown of `/usage`.

```
/cost
```

---

### /stats

Display session statistics: number of messages, tool calls, files modified, tokens used, session duration, and model used.

```
/stats
```

---

### /status

Show the current session status. Includes active model, permission mode, thinking config, connected MCP servers, and loaded plugins.

```
/status
```

---

### /insights

Generate an analytical report of the current session. Prints a structured breakdown of conversation statistics including turn count, token usage (input/output/total), average tokens per exchange, estimated cost, total tool calls, and the most frequently invoked tool.

```
/insights
```

Sample output:
```
Session Insights
──────────────────────────────────────
Conversation
├─ User turns          : 12
├─ Assistant turns     : 12
└─ Completed exchanges : 12

Tokens
├─ Input               : 48320
├─ Output              : 9140
├─ Total               : 57460
└─ Avg per exchange    : 4788

Cost
└─ Estimated USD       : $0.1823

Tools
├─ Total calls         : 34
└─ Most used           : Bash (18 calls)
```

---

## Agents & Tasks

### /agents

Manage the agent definitions in `.mikmik/agents/`. An agent is a named
configuration: a prompt, a model, an access level and a turn limit. Running
agents are watched with `/tasks` and the `monitor` tool, not here.

```
/agents                       — open the agents view
mikmik agents list
mikmik agents create <name>
mikmik agents edit <name>
mikmik agents delete <name>
```

---

### /tasks
**Aliases:** `bashes`

Ask the model to list the background tasks and their status. It takes no
arguments: the command expands to a prompt, and the model answers with the
`TaskList` tool.

```
/tasks
```

Use the `monitor` tool through the model to fetch a task's output or stop it.

---

### /todos

List the todos the model recorded for this session with the `TodoWrite` tool.
Each line shows its status, and a confidence percentage when the model supplied
one.

```
/todos
```

The list persists across turns. A reminder about incomplete items is appended to
the system prompt after the second turn; the `autoPoke` setting turns that off.

---

### /goal

Set a durable multi-turn autonomous goal. When a goal is active, MikMik continues working across turns until the goal is marked complete, paused, or a 200-turn runaway guard fires. Designed for complex, sustained tasks that would otherwise require repeated manual re-prompting.

```
/goal <objective>                    — set a new goal and begin working autonomously
/goal --tokens 250K <objective>      — set a goal with a soft token budget cap
/goal                                — show current goal status
/goal status                         — show current goal status
/goal pause                          — pause the active goal
/goal resume                         — resume a paused goal
/goal clear                          — delete the current goal
/goal complete                       — request a completion audit
```

When the model believes the goal has been achieved, it calls the `Goal` tool with op `complete`, an audit summary and evidence. Goals can be disabled globally by setting `MIKMIK_GOALS=0` in your environment.

See [Goal System](./advanced.md#goal-system) in the advanced guide.

---

### /guided-goal

The conversational door to the same goal system. Where `/goal <objective>` sets a goal from one line, `/guided-goal` draws it out first: MikMik states the single verifiable outcome it understands, names the done-condition that proves it, and asks whether to cap the token budget. Once the objective is clear it creates the goal itself — through the `Goal` tool's `create` op — and begins working autonomously.

```
/guided-goal                         — start the guided setup from scratch
/guided-goal <rough idea>            — start from a rough idea to refine
```

Use `/goal <objective>` when you already know the objective; use `/guided-goal` when you want help shaping it.

---

### /managed-agents

Configure the manager-executor agent architecture, where a manager model delegates subtasks to one or more executor agents working in parallel. Includes budget controls and isolation options.

```
/managed-agents                                       — show current configuration
/managed-agents status                                — show current configuration
/managed-agents presets                               — list built-in presets
/managed-agents preset <name>                         — apply a named preset
/managed-agents setup                                 — show setup instructions
/managed-agents enable                                — enable managed agents
/managed-agents disable                               — disable managed agents
/managed-agents reset                                 — remove all managed-agent configuration
/managed-agents configure manager-model <model>       — set the manager model
/managed-agents configure executor-model <model>      — set the executor model
/managed-agents configure executor-turns <n>          — set executor max turns
/managed-agents configure concurrent <n>              — set max concurrent executors
/managed-agents configure isolation on|off            — toggle executor isolation
/managed-agents budget <amount>                       — set total budget in USD (0 to clear)
```

Model format: `provider/model` (e.g., `anthropic/claude-opus-4-6`, `openai/gpt-4o`). Configuration persists to `~/.config/mikmik/settings.json` under `managed_agents`.

> **Preview feature.** Behaviour may change across releases.

See [Managed Agents](./advanced.md#managed-agents) in the advanced guide.

---

### /agent

List all available named agents, or show details for a specific agent. Named agents are predefined configurations with their own system prompts, model bindings, and access levels. Useful for discovering what agents are available before starting a session.

```
/agent             — list all visible named agents with access levels
/agent <name>      — show full details for a specific named agent
```

To activate an agent, start MikMik with `--agent <name>`. See [agents.md](./agents.md) for defining custom agents.

---

## Planning & Review

### /plan

Enter plan mode (read-only). In plan mode the model reads files and reasons
about changes but writes, edits and commands are blocked. Use it to draft an
approach before allowing writes.

```
/plan
/plan <description>       — name the task the plan is for
/plan exit                — leave plan mode
```

`Tab` on an empty prompt leaves plan mode too. The model leaves it by calling
`ExitPlanMode` and having the plan approved; the model enters it by calling
`EnterPlanMode`. See [Plan mode](configuration.md#plan).

---

### /ultraplan

Extended planning with a raised thinking budget, for more thorough analysis
before acting.

```
/ultraplan                        — medium effort
/ultraplan --effort=high
/ultraplan --effort=maximum
```

`--effort` accepts `medium`, `high` or `maximum` only.

---

### /ultrareview

Run an exhaustive multi-dimensional code review over the current working directory or a specified path. Goes significantly beyond `/review` and `/security-review`, covering:

- **Security** — OWASP Top 10, injection vulnerabilities, cryptographic weaknesses, path traversal, race conditions, dependency risks
- **Performance** — algorithmic complexity, allocations, N+1 queries, blocking I/O, memory leaks
- **Maintainability** — function length, nesting depth, DRY violations, naming, dead code
- **Error handling** — swallowed errors, panic paths, missing input validation
- **Test coverage** — missing tests, brittle tests, missing edge cases
- **API design, documentation, accessibility, and architecture**

Each finding is tagged by category and severity.

```
/ultrareview            — the working directory
/ultrareview <path>     — one file or directory
```

---

## MCP & Integrations

### /mcp

Documented above under [Configuration & Settings](#configuration--settings).

---

### /skills

List the skills the model can load. It takes no arguments.

```
/skills
```

Two sets are listed. Prompt templates in `.mikmik/commands/` and
`<config dir>/commands/` are named without a leading slash: the `Skill` tool
loads them by name, and the slash router does not read that directory.
Discovered skills, from `.mikmik/skills/`, `.agents/skills/`, the config
directory, and the paths and git URLs `skills` names, are slash commands and are
listed as such.

---

### ultracode (top effort + keyword)

Run a disciplined **ultracode** workflow for serious coding tasks. Ultracode is mikmik's take on Claude Code's `ultrathink`: a supervised procedure that classifies the task, picks a mode, and — when it genuinely helps — delegates bounded work across mikmik's native agent primitives, then integrates and verifies in the parent session.

Ultracode is the **highest effort level** — it sits past `max` on the "Smarter" end of the effort ladder and runs the model's top reasoning **plus** the workflow procedure. (It is no longer a `/skill`.) There are two ways to trigger it:

- **In the effort selector.** Run `/effort` and pick **ultracode** — the rightmost level, past the `│` divider, drawn with an animated purple spectrum. Applies for subsequent turns until you change the effort.
- **As a keyword.** Type the single word `ultracode` anywhere in a normal prompt. The keyword renders with a purple gradient in the input, and for that turn the effort is set to ultracode (its operating procedure is injected as a system-prompt addendum). No keyword means no change to normal prompts.

```
please ultracode <task>    — activate ultracode for this one turn via the inline keyword
/effort  →  ultracode      — set ultracode as the current effort level
```

**What it does**

1. **Classify** the task by type, risk, blast radius, verification needs, and whether independent packets exist.
2. **Pick a mode** — *Direct* (small, tightly-coupled work), *Workflow* (multi-phase work executed as isolated passes), or *Delegated* (the default for non-trivial work with independent packets).
3. **Delegate** in delegated mode using native primitives: `Agent` for bounded subagents (with `isolation: "worktree"` / `run_in_background: true`), `TeamCreate` for parallel swarms, and `TaskCreate` for background tasks. It fans out **2–4** subagents (cap ~5) on non-overlapping packets while the parent keeps the blocking critical path.
4. **Integrate** every result in the parent, checking claimed edits against the files and rejecting evidence-free outputs.
5. **Verify** with checks scaled to risk (targeted tests → lint/typecheck → build → smoke → independent review), reporting any skipped checks honestly.

**Composes with `/goal`.** Ultracode governs *how* a turn plans, delegates, integrates, and verifies; `/goal <objective>` keeps the work going *across* turns. Combine them for long, autonomous objectives — the goal loop spans turns while ultracode structures each one.

---

### /plugin
**Aliases:** `plugins`, `marketplace`

Manage plugins. A plugin registers slash commands, hooks, skills, agents, output styles, MCP servers and language servers.

```
/plugin
/plugin list
/plugin info <name>
/plugin enable <name>
/plugin disable <name>
/plugin install <source>
/plugin update <name>
/plugin remove <name>
/plugin reload
```

`install` takes a local directory, an `owner/repo` on GitHub (optionally
`owner/repo@branch`), or a git URL. A repository holding a
`.claude-plugin/marketplace.json` installs every plugin it lists. See
[Plugins](plugins.md#installing-from-a-repository).

`/plugin reload` (and its alias `/reload-plugins`) rereads the plugin
directories and applies the result to the running session: hooks, slash
commands, skills, agents, output styles, language servers and MCP servers.
See [Plugins](plugins.md#reload-plugins) for what each contribution does on a
reload.

---

### /chrome

Browser automation via Chrome DevTools Protocol (CDP). Connects to a running Chrome or Chromium instance and lets MikMik control it — navigate pages, click elements, fill forms, evaluate JavaScript, and take screenshots.

First, launch Chrome with remote debugging enabled:

```bash
chrome --remote-debugging-port=9222 --no-first-run
```

Then:

```
/chrome connect [--port 9222]      — connect to Chrome on the given port (default: 9222)
/chrome navigate <url>             — navigate to a URL
/chrome screenshot                 — take a screenshot, saved to a temp file
/chrome click <selector>           — click a CSS selector
/chrome fill <selector> <text>     — fill an input field
/chrome eval <js>                  — evaluate JavaScript and return the result
/chrome disconnect                 — disconnect from Chrome
```

Useful for testing web applications, scraping, or automating browser-based workflows without a separate browser-automation tool.

---

## Authentication

MikMik supports **multiple named accounts per provider**, Anthropic (Claude.ai or Console) and Codex (OpenAI ChatGPT subscription). Each login stores its credentials in `~/.config/mikmik/auth.json`, keyed by account name, and registers the account under `providers` in `settings.json`. The `provider` field in `settings.json` names the active one.

See [Authentication Guide](./auth.md#multiple-accounts) for the full story and on-disk layout.

### /login

Authenticate with Anthropic or Codex via OAuth PKCE. Opens a browser for the flow, then stores the credential in `auth.json` and registers the account in `settings.json`.

```
/login                            — Claude.ai OAuth (Bearer token, default)
/login --console                  — Console OAuth (creates an API key)
/login --codex                    — Codex / ChatGPT OAuth
/login --label work               — name the new account "work"
/login --codex --label personal   — Codex login, name the account "personal"
```

`--label` also accepts `-l <name>` and `--label=<name>`.

If a stored account carries the same email address or account UUID, that account is refreshed in place, so re-logging-in is idempotent. The match wins over `--label`: a label names a **new** account only, it cannot rename an existing one.

Without `--label`, the name comes from the email local part, then the account UUID, then the literal `account`. A name already taken gets a `-2`, `-3` suffix.

---

### /logout

Remove credentials. By default removes only the **active** account for the provider; the other stored accounts remain switchable.

```
/logout                — clear the active Anthropic account and the API key in settings
/logout --codex        — clear the active Codex account
/logout --all          — remove every Anthropic account and clear the API key
/logout --codex --all  — remove every Codex account
```

---

### /accounts

List every stored account, grouped by the protocol it speaks. There is one active account overall, and it is marked with `*`.

```
/accounts
```

Sample output:

```
anthropic:
  * personal [pro]  kuber@personal.example
    work [max]  kuber@company.example
codex:
    chatgpt  acct_01H...
```

An Anthropic OAuth row carries the subscription tier in brackets and the email address. A Codex row carries the account id. A plain API key row carries neither, because a key stores no identity.

With nothing stored, the command says so and points at `/connect`.

---

### /switch

Point the session at a stored account. Every account is switched the same way, whatever protocol it speaks, so the command takes a name and no flags. Run `/accounts` first to see the names.

```
/switch work                     — make "work" the active account
/switch chatgpt                  — make the Codex account "chatgpt" active
```

An unknown name is refused, and the error lists the names that are stored.

---

### /refresh

**Clear** the saved provider state, not refresh a token. The command wipes the saved provider selection, drops the API key, provider and model from the running config, and rebuilds the client, the provider registry and the model registry from scratch. Afterwards run `/connect` to authenticate and pick a provider again.

```
/refresh
```

The command takes no arguments and refuses to run while a response is streaming.

---

## Display & Terminal

### /theme

Documented above under [Configuration & Settings](#configuration--settings).

---

### /output-style

Documented above under [Configuration & Settings](#configuration--settings).

---

### /statusline

Documented above under [Configuration & Settings](#configuration--settings).

---

### /vim

Documented above under [Configuration & Settings](#configuration--settings).

---

### /terminal-setup

Documented above under [Code & Git](#code--git).

---

### /mobile

Display a QR code and download links for the Claude mobile app.

```
/mobile             — QR code for claude.ai/mobile, which covers both platforms
/mobile ios         — QR code for the iOS App Store
/mobile android     — QR code for Google Play
/mobile session     — QR code linking to the active remote session
```

`session` needs a remote session to be running; see
[`/remote-control`](#remote-control).

---

### /color

Set the accent colour of the prompt bar.

```
/color               — report the current colour
/color blue          — a named colour
/color #ff6b6b       — a hex code, `#RGB` or `#RRGGBB`
/color default       — back to the theme default
```

Named colours: red, green, blue, yellow, cyan, magenta, white, orange, purple.
The choice is written to `~/.config/mikmik/ui-settings.json` and applied at the
next start.

---

### /stickers

Opens the MikMik sticker page (`stickermule.com/claudecode`) in your default browser. Falls back to printing the URL if no browser can be launched.

```
/stickers
```

---

### /buddy
**Aliases:** `companion`

Show the companion that sits beside the input box, hatching it on first use.

```
/buddy         show the companion, hatching it if it is new
/buddy on      show it beside the prompt and tell the model it is there
/buddy off     hide it and stop describing it to the model
/buddy forget  discard the name and personality, keeping the body
```

The companion has two halves. Its body (species, rarity, eye, hat, shiny, and five stats) is rolled from your identity, so it is the same on every run and cannot be edited into something rarer by hand. That identity is your active stored account when you have one, and the machine otherwise, so logging in with the same account on a second machine gives you the same body while a machine with no stored login gets its own. Its name and personality are written once, by a model, on the first `/buddy`, and then kept in `companion.json` under the [config root](configuration.md).

Off by default. Turning it on costs one model call to hatch, adds a short block to every system prompt, and takes 13 columns beside the prompt box. On a narrow terminal the sprite is dropped and the prompt keeps the full width.

Address it by name in a message and it answers in one line above the prompt. That answer is a second model call, so it happens only when the name appears as a word of its own: a message about `src/mossback.rs` does not wake a companion called Mossback.

The model used for both calls is the session model unless `companion.model` is set:

```json
{
  "companion": {
    "enabled": true,
    "model": "claude-haiku-4-5-20251001"
  }
}
```

Both calls are billed to that model and appear under it in [`/cost`](#cost).

The companion is decoration. Its stats are shown on the card and affect nothing.

---

## Diagnostics & Info

### /doctor

Run the MikMik diagnostics suite. Checks configuration integrity, provider connectivity, tool availability, MCP server health, and reports any issues.

```
/doctor
```

---

### /version
**Aliases:** `v`

Display the current MikMik version string and build metadata.

```
/version
/v
```

---

### /update
**Aliases:** `upgrade`

Check for available updates. Queries the GitHub releases API and displays the latest version. If a newer version exists, prints the download URL or upgrade instructions. Does not auto-update.

```
/update
/upgrade
```

---

## Export & Sharing

### /export

Export the current conversation as Markdown or JSON. Without `--output` it
prints to the terminal.

```
/export                                      — JSON, to the terminal
/export --format markdown                    — readable Markdown
/export --format json --output chat.json
/export --output conversation.md             — the extension picks Markdown
```

---

### /copy

Copy the most recent assistant response to the system clipboard. Pass a number to copy the Nth most-recent response. On Linux a `wl-clipboard` or `xclip` backend is used; on macOS and Windows the native clipboard API is used.

```
/copy         — copy the most recent response
/copy 2       — copy the second most recent response
/copy N       — copy the Nth most recent response
```

A format may be named as well. Without one the response text is copied
unchanged, which is what `/copy` has always done.

| Format     | What is copied                                                     |
|------------|--------------------------------------------------------------------|
| `markdown` | Role heading, thinking blocks folded into `<details>`, tool calls as fenced JSON |
| `text`     | The same content with markdown formatting stripped out              |
| `code`     | Only the fenced code blocks, separated by rules                     |
| `json`     | The message as a JSON object, with its token counts and cost        |

```
/copy code        — every code block in the most recent response
/copy json 3      — the third most recent response as JSON
/copy 3 json      — the same; the order does not matter
```

---

## Advanced & Internal

### /thinking

Documented above under [Model & Provider](#model--provider).

---

### /connect

Documented above under [Model & Provider](#model--provider).

---

### /fork

Documented above under [Session & Navigation](#session--navigation).

---

### /effort

Documented above under [Model & Provider](#model--provider).

---

### /summary

Generate a summary of the current session. The model produces a condensed description of what was accomplished. Primarily used internally for session metadata.

```
/summary
```

---

### /remote-control
**Aliases:** `rc`

Manage the bridge that lets a phone or browser drive this session through a relay you host yourself. See [Remote Control](remote-control.md) for the relay setup and the settings block.

```
/remote-control          — show the resolved relay, token source, and permission mode
/remote-control start    — enable the bridge at startup
/remote-control stop     — disable the bridge at startup
/remote-control status   — same as no argument
```

With no argument it reports the relay address it resolved and which source each value came from, so a session configured through `settings.json` and one redirected by `MIKMIK_BRIDGE_URL` are told apart. A token shorter than 32 characters is reported as unusable and the bridge does not start.

`start` and `stop` change the setting only. The bridge connects on the next launch.

---

### /workspace
**Aliases:** `ws`

Show the organisation's configuration server: the providers it assigns you, the
settings policy it enforces, and your own settings backup. See
[Workspace server](workspace-server.md).

```
/workspace          — server, session, providers, policy and sync settings
/workspace sync     — upload this machine's settings now
/workspace pull     — take the providers and the policy again now
```

The listing separates the company's providers from your own. Editing a company
one is undone by the next pull, so the two have to be told apart at a glance. It
also names the keys the policy decides, because a setting that will not take
otherwise leaves you debugging your own config.

A policy fetched by `pull` applies from the next session: the settings layers
were merged when this one opened.

Signing in and out is `mikmik workspace login` and `mikmik workspace logout`. A
password does not belong in a prompt this transcript records.

---

### /remote-env

Manage the environment variables stored under the `env` key in `settings.json`
and forwarded to remote sessions.

```
/remote-env                    — list them
/remote-env list               — the same
/remote-env set <KEY> <VALUE>
/remote-env unset <KEY>
```

This is the only reader of the `env` key in the shipped binary; the tool runner
does not consult it.

---

### /context

Documented above under [Search & Files](#search--files).

---

### /sandbox-toggle
**Aliases:** `sandbox`

Enable or disable sandboxed execution of shell commands. When sandbox mode is on, bash/shell commands run in an isolated environment to limit unintended side effects. Supported on macOS, Linux, and WSL2.

```
/sandbox-toggle                          — toggle sandbox mode on/off
/sandbox-toggle on                       — enable sandbox mode
/sandbox-toggle off                      — disable sandbox mode
/sandbox-toggle status                   — show current state and excluded patterns
/sandbox-toggle exclude <pattern>        — add a command pattern to the exclusion list
```

> A restart is recommended after toggling for full effect. On Windows (non-WSL), sandbox mode is not supported.

---

### /think-back
**Aliases:** `thinkback`

Display the extended-thinking traces from previous model responses in the current session. Only available when extended thinking was used for those responses. Pass a number to view the Nth most-recent trace.

```
/think-back         — show the most recent thinking trace
/think-back 2       — show the second most recent thinking trace
/thinkback          — alias
```

Thinking traces appear when the model uses extended thinking mode (see `/thinking`). If no traces are found, MikMik suggests enabling extended thinking.

---

### /thinkback-play

Replay a previous extended-thinking trace as a formatted, step-numbered walkthrough. Useful for reviewing the model’s reasoning path in detail.

```
/thinkback-play         — replay the most recent thinking trace
/thinkback-play 2       — replay the second most recent thinking trace
```

---

## Command Availability

Every command is registered unconditionally. The list does not change with the
provider, the account, or how the session was started, so a command that exists
here exists everywhere.

Two commands report a limit at run time rather than being hidden:

| Command           | What it reports                                                                 |
|-------------------|---------------------------------------------------------------------------------|
| `/sandbox-toggle` | Sandboxed execution needs macOS, Linux or WSL2. On native Windows it says so.   |
| `/voice`          | Voice needs a microphone, and needs a build that was not made with `--no-default-features`. |
