# MikMik Configuration Reference

MikMik is configured through a layered system of JSON files, environment
variables, and command-line flags. This document describes every option.

---

## Configuration File Location

The global settings file lives at:

```
~/.config/mikmik/settings.json
```

The directory `~/.config/mikmik/` is created automatically on first run if it does
not exist. The file is standard JSON (or JSONC — comments are stripped before
parsing).

### Per-project settings

MikMik walks up from the current working directory looking for a project-level
settings file. The first file found wins (project settings take precedence over
global settings):

```
<project-root>/.mikmik/settings.json
<project-root>/.mikmik/settings.jsonc
```

A project settings file arrives with the checkout, and nobody reads it before
opening the directory. What it may set is therefore limited. Keys fall into
three groups.

**Taken from the project file.** Most keys: `model`, `mcpServers` (themselves
gated, see below), `agents`, `commands`, `modelOverrides`, and the rest. Keys
absent from the project file fall back to the global value. `theme` and
`output_format` are the exception among the harmless ones and stay with your
own settings: a `config` block always parses in full, so a project file that
never mentioned them was resetting them to their defaults.

**Taken only after you approve them.** `hooks`, `formatter`, `lsp_servers` and
`skills` each name a command to run or an address to fetch from. On the first
session in a checkout that declares any of them, MikMik shows them verbatim
and asks. "Always allow" records a fingerprint of exactly what was shown under
`~/.config/mikmik/project_trust.json`, never inside the repository; editing an
approved command changes the fingerprint and asks again. Headless (`--print`)
never runs them, because there is no way to ask, and says so on stderr.
Project-defined `mcpServers` follow the same shape through their own prompt.

**Never taken from the project file.** `permission_mode`, `permissionRules`,
`api_key`, `provider`, `provider_configs`, `providers`, `searxngUrl`, `env`,
`custom_system_prompt`, `append_system_prompt`, `workspace_paths`,
`additional_dirs`, `statusLine`, `acpAgents`, `remoteControl`,
`trustProjectMcpServers`, `skipDangerousModePermissionPrompt` and
`allowedBashPrefixes`. These decide whether a tool asks before acting, where
the conversation and the credential are sent, what the model is told before you
say anything, and which directories are reachable. A repository that could set
them would not need a hook. MikMik names the ignored keys on startup rather
than dropping them silently.

---

## Top-level Settings Structure

```json
{
  "version": 1,
  "provider": "anthropic",
  "config": { ... },
  "providers": { ... },
  "modelOverrides": { ... },
  "projects": { ... },
  "commands": { ... },
  "formatter": { ... },
  "agents": { ... },
  "skills": { ... },
  "permissionRules": [],
  "enabledPlugins": [],
  "disabledPlugins": [],
  "hasCompletedOnboarding": false,
  "showMessageTimestamps": false,
  "advisorModel": "claude-opus-4-6",
  "companion": { ... },
  "remoteControl": { ... },
  "acpAgents": { ... }
}
```

Most day-to-day options live inside the `config` object. Provider credentials
live in the `providers` map. Corrected model metadata for self-hosted or
unknown models lives in the `modelOverrides` map — see
[Model metadata overrides](providers.md#overriding-model-metadata).

### Favourite models

| Key              | Type             | Default | Description                                                              |
|------------------|------------------|---------|--------------------------------------------------------------------------|
| `favoriteModels` | array of strings | []      | Models starred in the model picker, each as `account/model`. A starred model is drawn with `★` and sorted first inside its own account's section. |

`Ctrl+F` in [`/model`](commands.md#model) writes this list. The key is always
qualified, because the same model reached through two accounts is two different
requests. A project settings file adds to the user's list rather than replacing
it.

### Plugin selection

| Key               | Type             | Default | Description                                                              |
|-------------------|------------------|---------|--------------------------------------------------------------------------|
| `enabledPlugins`  | array of strings | []      | Names `/plugin enable` has recorded. Discovery already loads every plugin it finds, so this list only cancels a previous `disable`. |
| `disabledPlugins` | array of strings | []      | Plugin names to skip. A listed plugin contributes no commands, hooks, skills, agents, or MCP servers. |
| `pluginConfig`    | object           | {}      | Values for the options a plugin declares under `userConfig`, keyed by plugin name then option name. Edited in `/settings`; the plugin reads them from `CLAUDE_PLUGIN_CONFIG`. See [Plugins](plugins.md#user_config). |

`/plugin enable <name>` and `/plugin disable <name>` write these lists. The
running session keeps the plugin set it loaded at startup until `/plugin
reload` rereads the directories and applies the change. A name in
`disabledPlugins` that matches no
discovered plugin is ignored. `mikmik --bare` skips plugin discovery
entirely, regardless of both lists.

### Skills

| Key            | Type             | Default | Description                                                            |
|----------------|------------------|---------|------------------------------------------------------------------------|
| `skills.paths` | array of strings | []      | Extra directories to search for skills. A relative path resolves against the working directory. |
| `skills.urls`  | array of strings | []      | Git repository URLs to fetch skills from. Each is cloned once and then cached. |

A skill is a prompt template. Discovery reads two layouts in every searched
directory: a flat `<name>.md` file, and a `<name>/SKILL.md` package, which
takes its name from the directory unless the frontmatter sets `name:`. The
searched directories are `.mikmik/skills/` and `.agents/skills/` walking up
from the working directory, then `<mikmik home>/skills/`, then `skills.paths`,
then `skills.urls`. Each installed plugin's `skills/` directory is added to the
search at startup. Run a skill by its name as a slash command, and list them
all with `/skills`.

### Transcript display

| Key                     | Type    | Default | Description                                                                                                                                                                    |
|-------------------------|---------|---------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `showMessageTimestamps` | boolean | false   | Print the local time beneath each message. Times are stored in UTC and converted using the machine's time zone. Messages from an earlier day also show their date (`13 Aug 14:32`). |

Toggle it from the TUI with `/config` → **Show message timestamps**. Turns
restored from a transcript recorded before this option existed carry no time
and render without one.

### Desktop notifications

The moments a session either stops and waits or has nothing left to do. Each is
sent through the operating system's own notification service, so it arrives
while the terminal is in the background.

| Key                    | Type    | Default | Description                                                             |
|------------------------|---------|---------|-------------------------------------------------------------------------|
| `notifications`        | boolean | true    | Master switch. Off means nothing is sent, whatever the keys below say.   |
| `notifyOnQuestion`     | boolean | true    | The model called `AskUserQuestion` and the turn is blocked on an answer. |
| `notifyOnPlanReady`    | boolean | true    | A plan is waiting for approval (see [`plan`](#plan)).                    |
| `notifyOnPermission`   | boolean | true    | A tool is waiting for permission and the turn is blocked on the answer. The dialog's own explanation is used as the body. |
| `notifyOnTurnComplete` | boolean | true    | The turn finished, tool round-trips included, and the prompt is free. The last thing the model said is used as the body. |
| `notifySound`          | boolean | false   | Play a short sound with each notification.                              |

Toggle them from the TUI with `/settings`. Delivery is best-effort: a machine
with no notification daemon, or a terminal that was never granted notification
permission, drops the notification without interrupting the turn.

The sound is the notification's own, not the terminal bell, so it arrives with
the banner and follows the system's Do Not Disturb. A notification that is not
delivered makes no sound either. Each platform is asked for its own default
alert sound, so what you hear is what you already set for notifications:
`NSUserNotificationDefaultSoundName` on macOS and `Default` on Windows. The
freedesktop spec has no equivalent token, so the XDG backend is sent
`message-new-instant`; a sound theme without that name leaves the notification
silent.

### Advisor

| Key            | Type   | Default | Description                                                                                                                                       |
|----------------|--------|---------|-------------------------------------------------------------------------------------------------------------------------------------------------------|
| `advisorModel` | string | unset   | Second model consulted for a review. A bare ID runs against the active provider; `provider/model` targets a specific one; `provider:account/model` also targets a specific stored login. Unset disables the advisor. |

Set it with [`/advisor <model>`](commands.md#advisor) rather than by hand. When
unset, the `Advisor` tool is not offered to the model at all.

### Companion

The small creature beside the input box. See [`/buddy`](commands.md#buddy).

| Key       | Type           | Default | Description                                                                                             |
|-----------|----------------|---------|-----------------------------------------------------------------------------------------------------------|
| `enabled` | boolean        | false   | Show the companion and describe it to the model. Off by default: on, it costs a model call to hatch and a block in every system prompt. |
| `model`   | string \| null | unset   | Model that hatches the companion and writes its replies. Unset uses the session model.                  |

```json
"companion": {
  "enabled": true,
  "model": "claude-haiku-4-5-20251001"
}
```

Toggle it with `/buddy on` / `/buddy off`, or from `/config` → **Companion**. The generated name and personality live in `companion.json` beside this file, not here; the body is never stored, because it is re-derived from your identity on every read.

### Remote control

Points the bridge at a relay you host yourself, so a phone or browser can drive a running session. See [Remote Control](remote-control) for the full setup.

There is no separate remote permission policy. `config.permission_mode` decides whether a tool asks at all; once it asks, the answer may come from the terminal or the remote client.

| Key             | Type           | Default | Description                                                                                                       |
|-----------------|----------------|---------|-------------------------------------------------------------------------------------------------------------------|
| `url`           | string         | unset   | Base address of your relay, for example `https://relay.example`. A trailing slash is trimmed.                     |
| `token`         | string         | unset   | Shared secret, at least 32 characters. Shorter values are refused and the bridge does not start.                  |
| `label`         | string \| null | unset   | Name shown in the session list. Falls back to the machine's hostname.                                             |

```json
"remoteControl": {
  "url": "https://relay.example",
  "token": "a-generated-token-of-at-least-32-characters",
  "label": "workstation"
}
```

This block is read from the user settings file only. A project settings file cannot set it, because pointing the bridge at a relay is a decision about the machine, not about the repository.

`MIKMIK_BRIDGE_URL` and `MIKMIK_BRIDGE_TOKEN` override it when set.

### External ACP agents

Agents that speak the [Agent Client Protocol](https://agentclientprotocol.com/), reachable through the `AcpAgent` tool. Keys are the names the model uses to pick one.

| Key       | Type              | Default  | Description                                                                                  |
|-----------|-------------------|----------|----------------------------------------------------------------------------------------------|
| `command` | string            | required | Executable to run.                                                                            |
| `args`    | string[]          | `[]`     | Arguments passed to it, usually whatever puts the agent in ACP mode.                          |
| `env`     | object            | `{}`     | Extra environment for the subprocess. Values go through `{env:VARNAME}` substitution.        |

```json
"acpAgents": {
  "cursor": {
    "command": "agent",
    "args": ["--force", "acp"]
  },
  "gemini": {
    "command": "gemini",
    "args": ["--experimental-acp"],
    "env": { "GEMINI_API_KEY": "{env:GEMINI_API_KEY}" }
  }
}
```

The tool is only offered to the model when this block names at least one agent. Everything the sub-agent asks to do is approved through the same permission prompt as a local tool. See [Tools](tools#acpagent) for the full behaviour.

This block is read from the user settings file only. An agent definition names an executable the model can invoke, so a repository able to add one would gain arbitrary code execution on your machine.

---

## The `config` Object

The `config` object holds runtime behaviour options.

### Model and token settings

| Key          | Type            | Default          | Description                                                                                                                          |
|--------------|-----------------|------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| `api_key`    | string \| null  | null             | Anthropic API key. Overrides `ANTHROPIC_API_KEY` env var. Prefer the env var in shared environments.                                 |
| `model`      | string \| null  | provider default | Model ID to use. When absent, the provider's default is used (e.g. `claude-sonnet-4-6` for Anthropic, `gpt-4o` for OpenAI).          |
| `max_tokens` | integer \| null | 32000            | Maximum tokens per model response.                                                                                                    |
| `provider`   | string \| null  | `"anthropic"`    | Active provider. See the [Providers](#providers) section.                                                                             |
| `effort`     | string \| null  | unset            | Reasoning effort a session starts at: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultracode`. Unset leaves it to the turn. |

### Permission mode

| Key               | Type   | Default     | Description                                                                                                       |
|-------------------|--------|-------------|-------------------------------------------------------------------------------------------------------------------|
| `permission_mode` | string | `"default"` | Controls how tool permissions are enforced. One of `"default"`, `"acceptEdits"`, `"bypassPermissions"`, `"plan"`. |

See [Permission Modes](#permission-modes) for a full description of each value.

`/yolo` writes this key: `on` sets `"bypassPermissions"` and `off` sets
`"default"`. There is no separate yolo setting, so a settings file cannot say
two contradictory things about the same state. Shift+Tab cycles the mode for
the session only and writes nothing.

### Interface and output

| Key             | Type           | Default     | Description                                                                                                                                                 |
|-----------------|----------------|-------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `theme`         | string         | `"default"` | Colour theme for the TUI. One of `"default"`, `"dark"`, `"light"`, `"deuteranopia"`. Sets the error, success, warning and accent colours; layout colours are fixed. |
| `output_style`  | string \| null | null        | Named output style. Eleven ship built in: `"default"`, `"concise"`, `"explanatory"`, `"learning"`, `"asd-ste100"`, `"caveman-lite"`, `"caveman"`, `"caveman-ultra"`, `"rocky-lite"`, `"rocky"`, `"rocky-ultra"`. More can be added as `.md` or `.json` files under `~/.config/mikmik/output-styles/`. |
| `output_format` | string         | `"text"`    | Output format for headless (`--print`) mode. One of `"text"`, `"json"`, `"stream-json"`.                                                                    |
| `verbose`       | boolean        | false       | Enable debug-level log output.                                                                                                                              |

### Context compaction

| Key                 | Type    | Default | Description                                                                            |
|---------------------|---------|---------|----------------------------------------------------------------------------------------|
| `auto_compact`      | boolean | true    | Automatically compact the conversation context when the context window nears capacity. |
| `compact_threshold` | integer | 90      | Percent of the context window that triggers auto-compaction. Clamped to 100.           |

`compact_threshold` was a fraction in the range 0.0-1.0 before, so a value
below 1 is still read and scaled: `0.9` means 90. Only the user settings file
sets it; a project file naming the key is ignored.

### Turn behaviour

| Key                  | Type    | Default | Description                                                                                                                                                                    |
|----------------------|---------|---------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `maxTurns`           | integer | 10      | How many agentic turns one run may take before it stops. `/turns off` writes the largest possible value, which no run can reach.                                               |
| `degradationSummary` | boolean | true    | Run one final tool-less turn asking the model to summarise its progress when the turn limit is reached. Set to `false` to stop at the limit and take the last message instead. |
| `autoPoke`           | boolean | true    | Append a reminder about incomplete todos to the system prompt after the second turn. Set to `false` when the todo list is a record rather than a work queue.                   |

`degradationSummary` and `autoPoke` each save one request per run when switched
off. Leaving any of the three unset keeps today's behaviour, so upgrading
changes nothing.

`maxTurns` is also set by `--max-turns` at launch and by `/turns` during a
session. `--max-turns` lasts for one launch; `/turns` writes the key, so the
limit survives a restart. An agent definition's own `max_turns` wins over it
while that agent is active; `/turns` says so rather than reporting a limit that
will not apply.

`autoPoke` is set by `/poke` during a session, which writes the key the same
way. `/poke default` removes the key rather than writing `true` into it, so the
file keeps saying nothing about a setting nobody chose.

### System prompt

| Key                    | Type           | Default | Description                                                                           |
|------------------------|----------------|---------|---------------------------------------------------------------------------------------|
| `custom_system_prompt` | string \| null | null    | Replace the default MikMik system prompt entirely with this text.                    |
| `append_system_prompt` | string \| null | null    | Append this text to the end of the assembled system prompt (after AGENTS.md content). |

The same two can be set per run from the command line, which overrides the settings file:

| Flag                              | Effect                                                              |
|-----------------------------------|---------------------------------------------------------------------|
| `--system-prompt <TEXT>`, `-s`    | Replace the base prompt with `TEXT`.                                |
| `--system-prompt-file <PATH>`     | Replace the base prompt with the file's contents. Fails if unreadable. |
| `--append-system-prompt <TEXT>`   | Append `TEXT` after the assembled prompt.                            |

`--system-prompt` and `--system-prompt-file` are mutually exclusive. Run `mikmik --dump-system-prompt` with the same flags to see exactly what a run would send.

### Tool access

| Key                | Type             | Default  | Description                                                                                |
|--------------------|------------------|----------|--------------------------------------------------------------------------------------------|
| `allowed_tools`    | array of strings | [] (all) | Restrict the tool set to this explicit list. An empty array means all tools are available. |
| `disallowed_tools` | array of strings | []       | Always deny these tools, regardless of other settings.                                     |

Tool names match the internal names: `Bash`, `Read`, `Write`, `Edit`, `Glob`,
`Grep`, `WebSearch`, `WebFetch`, `TodoWrite`, and MCP tool names prefixed with
their server name (`myserver_toolname`).

### Tool behaviour

| Key                   | Type    | Default | Description                                                                                                                                                                          |
|-----------------------|---------|---------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `includeIgnoredFiles` | boolean         | false   | Let `Glob` and `Grep` search files that `.gitignore` and `.ignore` exclude. Off by default, so a build directory does not drown the results.                                     |
| `searxngUrl`          | string \| unset | unset   | Base address of the SearXNG instance `WebSearch` prefers, for example `http://localhost:8080`. Overrides the `SEARXNG_URL` environment variable. Unset means no instance.        |
| `webSearchFallback`   | boolean         | false   | Let `WebSearch` continue with Brave or DuckDuckGo when the SearXNG instance is unreachable. Off by default, so a query aimed at a private instance stays there.                  |

All three are editable from `/settings`. Turning **SearXNG** on there prompts for
the address and writes it to `searxngUrl`; turning it off clears the key.

### Interface

| Key               | Type    | Default | Description                                                                                                                                                  |
|-------------------|---------|---------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `timelineEnabled` | boolean | false   | Record every tool call and finished turn, and offer the panel through `/timeline` and `Ctrl+Shift+L`. Off by default; while off nothing is collected at all. |
| `mouseCapture`    | boolean | true    | Let MikMik handle the mouse: wheel scrolling, right-click menus and drag-select. Turn it off to give the mouse back to the terminal. Applies at next start. |
| `liveToolOutput`  | boolean | false   | Show what a shell command prints while it is still running, under the tool block, instead of only its preview when it ends. Off by default: a command that prints steadily redraws the transcript on every frame. |

All three are editable from `/settings`, as **Execution timeline**, **Mouse
capture** and **Live tool output**. See [Commands](commands.md#timeline) for what the timeline panel
shows.

While MikMik captures the mouse, the terminal no longer sees it, so its own
selection and its right-click paste stop working. That matters most over SSH,
where the remote host often has no `wl-copy` or `xclip` for `Ctrl+V` to reach:
set `mouseCapture` to `false` and paste with the terminal's own shortcut
instead. `Shift+Insert` works either way.

Copying out needs nothing installed. When no clipboard tool answers, MikMik
hands the text to the terminal emulator with OSC 52, which works over SSH.

Live tool output is a terminal-only display. It never reaches a remote client:
the last ten lines are redrawn in place as they arrive, which is a redraw the
bridge has no way to express. A remote client still sees the finished result
when the command ends.

### Status line

`statusLine` runs a shell command and shows its output in its own rows directly
above the footer, so you can keep context usage, cost or git state permanently
in view.

```json
{
  "config": {
    "statusLine": {
      "type": "command",
      "command": "~/.config/mikmik/statusline.sh",
      "padding": 2,
      "refreshInterval": 5
    }
  }
}
```

| Key                    | Type    | Default | Description                                                                                                     |
|------------------------|---------|---------|-------------------------------------------------------------------------------------------------------------------|
| `type`                 | string  | command | Only `"command"` runs anything. Any other value leaves the status line off.                                       |
| `command`              | string  | —       | Runs in a shell, so a script path and an inline pipeline both work.                                              |
| `padding`              | number  | 0       | Extra columns of indentation on each side of the output.                                                          |
| `refreshInterval`      | number  | —       | Re-run every N seconds on top of the state-driven updates. Minimum 1. Leave it out to run only when state changes. |
| `hideVimModeIndicator` | boolean | false   | Suppress the built-in `-- INSERT --` line, for a status line that prints `vim.mode` itself.                        |

**Only your own global settings file can set this.** A project's
`.mikmik/settings.json` is ignored for `statusLine`, in whole: it can neither
replace the command nor introduce one. Without that rule, cloning a repository
would run whatever shell command the repository asked for.

The command runs when the session changes: at startup, when a reply arrives,
when the model, directory, permission mode, vim mode, output style, effort
level, context usage or cost move. Bursts collapse into a single run, a run
still going when the next change lands is killed, and an idle session with no
`refreshInterval` runs nothing at all. Output is capped at 4 KB and a command
that has neither finished nor printed within 10 seconds is abandoned.

Output may span several lines, and each is shown on its own row, up to half the
terminal height. ANSI colour is rendered rather than printed; OSC 8 hyperlinks
show their label as plain text. `COLUMNS` and `LINES` carry the terminal size,
which a script cannot measure for itself because its output is captured.

The session arrives as JSON on stdin:

```json
{
  "session_id": "…",
  "transcript_path": "~/.config/mikmik/projects/…/….jsonl",
  "version": "0.1.7",
  "cwd": "/work/project",
  "workspace": { "current_dir": "/work/project", "project_dir": "/work" },
  "model": { "id": "claude-opus-5", "display_name": "claude-opus-5" },
  "permission_mode": "Default",
  "output_style": { "name": "auto" },
  "effort": { "level": "high" },
  "vim": { "mode": "NORMAL" },
  "cost": { "total_cost_usd": 1.25, "total_duration_ms": 61000 },
  "context_window": {
    "total_input_tokens": 1500,
    "total_output_tokens": 500,
    "context_window_size": 200000,
    "used_percentage": 20.0,
    "remaining_percentage": 80.0,
    "current_usage": {
      "input_tokens": 1000,
      "output_tokens": 500,
      "cache_creation_input_tokens": 200,
      "cache_read_input_tokens": 300
    }
  },
  "exceeds_200k_tokens": false
}
```

`vim` is absent unless vim mode is on, and `transcript_path` is absent when the
session has no file yet. A script that reads the JSON with `jq` looks like this:

```bash
#!/bin/bash
input=$(cat)
model=$(printf '%s' "$input" | jq -r '.model.display_name')
dir=$(printf '%s' "$input" | jq -r '.workspace.current_dir')
pct=$(printf '%s' "$input" | jq -r '.context_window.used_percentage // 0' | cut -d. -f1)
printf '\033[32m[%s]\033[0m %s | %s%% context\n' "$model" "${dir##*/}" "$pct"
```

`/statusline` reports the configured command alongside the built-in status bar
items; see [Commands](commands.md).

### Directory access

| Key               | Type             | Default | Description                                                                                                              |
|-------------------|------------------|---------|--------------------------------------------------------------------------------------------------------------------------|
| `additional_dirs` | array of strings | []      | Additional filesystem paths MikMik is allowed to read and write. Equivalent to passing `--add-dir` on the command line. Each one becomes a named workspace root the model can address as `&root-name/path`; see [`--add-dir`](advanced.md#--add-dir). |

### MCP servers

| Key           | Type                       | Default | Description                                           |
|---------------|----------------------------|---------|-------------------------------------------------------|
| `mcp_servers` | array of `McpServerConfig` | []      | Model Context Protocol servers to connect at startup. |

Each `McpServerConfig` object:

```json
{
  "name": "my-server",
  "command": "/path/to/server",
  "args": ["--flag"],
  "env": { "MY_VAR": "value" },
  "type": "stdio"
}
```

`type` can be `"stdio"` (default) or `"http"` (for HTTP-SSE servers, in which
case `command` is the base URL).

Servers declared by an installed plugin join this list at startup. A server
that came with the project (from `<project>/.mikmik/settings.json` or a plugin
under `<project>/.mikmik/plugins/`) needs approval before it launches; see
[Plugins](plugins.md#mcp_servers).

### Language servers

| Key               | Type                       | Default | Description                                                                    |
|-------------------|----------------------------|---------|--------------------------------------------------------------------------------|
| `lsp_servers`     | array of `LspServerConfig` | []      | Language servers the LSP tool may use. Field list in [tools.md#lsp](tools.md#lsp). |
| `lsp_auto_detect` | boolean                    | true    | Consult the bundled catalogue of language servers.                             |

With `lsp_auto_detect` on, a catalogue server is used only when both are true:
the working directory carries one of the server's root markers, and the
server's binary resolves. So a machine with no language server installed starts
nothing, and a directory that is not a Rust project never starts a Rust server.

The binary is looked for in the project's own bin directories first
(`node_modules/.bin`, `.venv/bin`, `.venv/Scripts`, `venv/bin`, Ruby binstubs
in `bin` and `vendor/bundle/bin`, Go's `bin`), then on `PATH`. A project pins
its tooling, and the pinned copy is the one that matches the project's
configuration.

An `lsp_servers` entry whose `name` matches a catalogue server replaces it, so
overriding one binary or one argument does not mean copying the whole entry.

`lsp_auto_detect` is taken from the user's settings alone. A project's
settings file cannot switch it on, because detection starts a process and the
markers that trigger it are files the repository itself carries.

The settings screen has a **Detect language servers** row for the same key.

### Environment variables injected into tools

| Key   | Type                     | Default | Description                                                                                                                                                                                                         |
|-------|--------------------------|---------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `env` | object (string → string) | {}      | Environment variables for tool execution. Values may reference existing env vars using `{env:VARNAME}` syntax. |

Only your own global settings file can set this: `LD_PRELOAD`,
`DYLD_INSERT_LIBRARIES` and `PATH` all redirect an ordinary-looking command to
code of the setter's choosing, so a project file is ignored here.

The name overstates what is wired up today. `/remote-env` is the only reader in
the shipped binary; the tool runner does not consult it.

### Hooks

Hooks let you run shell commands in response to lifecycle events. They are
defined as a map from event name to an array of hook entries.

```json
"hooks": {
  "PreToolUse": [
    { "command": "echo tool=$TOOL_NAME", "blocking": false }
  ],
  "PostToolUse": [
    { "command": "/path/to/my-logger.sh", "tool_filter": "Bash", "blocking": false }
  ],
  "Stop": [
    { "command": "notify-send 'MikMik done'", "blocking": false }
  ]
}
```

Available events:

| Event              | When it fires                                              |
|--------------------|------------------------------------------------------------|
| `PreToolUse`       | Before a tool executes. Receives event JSON on stdin.      |
| `PostToolUse`      | After a tool returns its result.                           |
| `Stop`             | When the model finishes its turn (stop reason).            |
| `PostModelTurn`    | After the model samples a response, before tool execution. |
| `UserPromptSubmit` | When the user submits a prompt.                            |
| `Notification`     | General-purpose notification event.                        |

Hook entry fields:

| Field         | Type           | Description                                                         |
|---------------|----------------|---------------------------------------------------------------------|
| `command`     | string         | Shell command to execute.                                           |
| `tool_filter` | string \| null | Only run for this tool name (`PreToolUse`/`PostToolUse` only).      |
| `blocking`    | boolean        | If true, a non-zero exit code blocks the operation. Default: false. |
| `timeout_ms`  | integer        | How long the command may run before it is stopped. Default: 30000.  |

A hook that reaches its limit is stopped along with anything it started, and
the run continues. A `blocking` hook that reaches it blocks the operation
instead: a hook that never answered cannot be read as approval.

A hook declared by a project's `.mikmik/settings.json` runs only after you
have seen the command and approved it; see [Per-project
settings](#per-project-settings). The same applies to `formatter`,
`lsp_servers` and `skills`.

---

## Permission Modes

The `permission_mode` field (and `--permission-mode` CLI flag) controls how
tool calls are approved.

### `default`

Read-only operations (file reads, searches, glob) are permitted automatically.
Write and execute operations (file writes, shell commands) prompt the user for
confirmation in the TUI, or are denied in headless mode.

### `acceptEdits`

`Edit` is auto-approved. Everything else keeps the checks it has under
`default`, so shell commands and other write tools still prompt. Explicit
allow and deny rules are evaluated first either way.

### `bypassPermissions`

All permission checks are skipped entirely. Every tool call is allowed
unconditionally.

Use with caution: the model can read and modify any file reachable from the
current working directory without any user confirmation.

### Commands that destroy data always ask

Permission is granted per tool, not per command. Approving `Bash` once while
running `ls`, or adding a `make ` prefix to the allowlist, approved every later
shell command with it. Deletion cannot be undone and the approval carried no
information about it, so a command whose purpose is to destroy data prompts
again even when an allow rule or a prefix matches.

| Command                       | Why it counts                                            |
|-------------------------------|----------------------------------------------------------|
| `rm`                          | Deletes files                                            |
| `shred`, `wipefs`             | Overwrites data so it cannot be recovered                |
| `dd`, `truncate`              | Overwrites or empties a file or device                   |
| `mkfs`, `mkfs.*`              | Formats a filesystem                                     |
| `mv -f`, `mv --force`         | An explicit instruction to overwrite the target          |
| `git clean -f`, `--force`     | Deletes untracked files, which git cannot bring back     |

Every segment of the command line is inspected, so `make && rm -rf dist` is
caught by its second half. A path-qualified call is the same command:
`/bin/rm` counts as `rm`. A plain `mv a b` does not count, because it names a
rename far more often than an overwrite and the check cannot see whether the
target exists.

Two consequences:

- The dialog for such a command offers no "Allow commands matching `<prefix>*`"
  option, because the allowlist would never honour it.
- `bypassPermissions` still allows them. That mode is an explicit decision to
  stop being asked, so it is not overridden here.

A command classified as **Critical** risk (`rm -rf /`, a fork bomb, `mkfs` on a
device, piping a download into a shell) is not asked about at all: the bash
tool refuses to run it whatever the permission mode says.

Because of that, the mode is gated behind a warning dialog. It appears when a
session starts in `bypassPermissions` and again whenever the mode is switched
to it while a session is running, by `shift+tab`, `/yolo on`, `/permissions set
bypass-permissions`, or a settings file that a reload picks up. Declining at
startup ends the session; declining a mid-session switch puts the previous mode
back, on disk as well when the settings file already named `bypassPermissions`.

Accepting writes `skipDangerousModePermissionPrompt: true` and the warning is
never shown again on that machine, at startup or mid-session.

A remote client sees the same warning and may answer it, because nothing runs
while it is up and a session waiting on it would otherwise look idle from the
browser. See [Remote control](remote-control.md#the-bypass-warning).

The model cannot set this mode itself. The `Config` tool can read
`permission_mode` but refuses to write it, because a turn that switched the
checks off would be switching off the gate on its own next tool call.

### `plan`

Read-only mode. File reads and searches are allowed; file writes and command
execution are blocked. This matches the built-in `plan` agent's behaviour and
is useful for code analysis sessions where you want to prevent accidental
modifications.

Entering plan mode is the model's decision as often as it is yours. `/plan` and
`Tab` switch by hand; the model switches by calling `EnterPlanMode`, and the new
mode reaches the turn that is already running. What makes it reach for that tool
is the "Planning" section of the base system prompt, which lists the work that
warrants a plan, the work that does not, and asks for `AskUserQuestion` on
anything the request leaves open. Replacing the prompt with
`customSystemPrompt` removes that guidance.

Leaving plan mode is your decision, not the model's. When the model calls
`ExitPlanMode`, the plan is shown in a dialog and the turn waits there. Four
answers:

| Answer                                | Result                                                                   |
|---------------------------------------|--------------------------------------------------------------------------|
| Yes, clear context and switch to *M*  | Summarises the conversation, then sends the plan again as a fresh prompt. Permission mode becomes *M*. |
| Yes, and switch to *M* for this session | Permission mode becomes *M*, agent mode becomes `build`.                |
| Yes, manually approve edits           | Permission mode becomes `default`, agent mode becomes `build`.            |
| Tell MikMik what to change            | Nothing changes; the session is still planning.                           |

*M* is the permission mode that was in force when plan mode was entered, so a
session that planned from `bypassPermissions` returns to it. A session that was
already in `default`, or that started in plan mode, gets `acceptEdits` instead:
approving has to mean more than "carry on asking".

Clearing the context happens after the turn ends, not when you answer: a
conversation rewritten mid-turn would leave the pending tool call unanswered.
The model is told to stop, the conversation is summarised, and the plan is sent
again as the next message.

Anything typed in the note row reaches the model with every answer, so a
rejection carries its reason. `Esc` counts as "keep planning" and sends no
note. Headless (`--print`) has no dialog to ask through, so `ExitPlanMode`
returns there exactly as it always did rather than blocking on nobody.

Keys in the dialog:

| Key                | What it does                                                              |
|--------------------|---------------------------------------------------------------------------|
| `↑` `↓` `Tab`      | Move between the four answers and the note row.                            |
| `1`-`4`            | Pick an answer, unless you are already typing a note.                      |
| `Enter`            | Send the picked answer.                                                    |
| `shift+tab`        | Approve carrying the note. On the fourth answer it sends the plain approval, because the shortcut cannot mean "refuse". |
| `ctrl+g`           | Open the plan in `$VISUAL` or `$EDITOR`. What you save is what the model is told to implement. |
| `PgUp` `PgDn`      | Scroll a plan taller than the dialog.                                      |
| `Esc`              | Keep planning, sending no note.                                            |

Every `ExitPlanMode` call writes its plan to `<config dir>/plans/<session
id>/`, numbered in the order they were written (`001.md`, `002.md`, …), headless
included. The dialog shows the path of the plan it is asking about. Nothing is
deleted, so a session that planned several times keeps every version, and it is
the only lasting copy: the tool call itself scrolls away with the transcript.

A sub-agent shares the session id but never opens the dialog: the session it
would interrupt belongs to the user, not to it. Its `ExitPlanMode` returns the
way it does headless, and its plan lands as the next numbered file rather than
over the one being shown.

The permission mode can also be overridden per-session on the command line:

```bash
mikmik --permission-mode acceptEdits "refactor the auth module"
mikmik --dangerously-skip-permissions "..."  # equivalent to bypassPermissions
```

---

## AGENTS.md Memory Files

AGENTS.md files are plain Markdown documents that MikMik injects into the
system prompt at startup. They let you give the model persistent context about
your project, coding standards, or personal preferences without repeating
yourself in every session.

### File locations and priority

MikMik loads memory files from four locations, and nowhere else. It does not
walk up from the working directory, so an `AGENTS.md` in a subdirectory or in
a parent of the project is not read. The project root is the repository root,
so a session started in a subdirectory reads the same files as one started at
the top.

The four locations are processed in this order, each appended below the last:

| Scope   | Path                                | Description                                                                                                |
|---------|-------------------------------------|------------------------------------------------------------------------------------------------------------|
| Managed | `~/.config/mikmik/rules/*.md`             | Global policy files. All `.md` files in this directory are loaded in alphabetical order.                   |
| User    | `~/.config/mikmik/AGENTS.md`              | Your personal preferences and instructions, applied to all projects.                                       |
| Project | `<project-root>/AGENTS.md`          | Project-level context: architecture notes, conventions, workflows. Typically committed to version control. |
| Local   | `<project-root>/.mikmik/AGENTS.md` | Local overrides not committed to version control (add `.mikmik/` to `.gitignore`).                        |

Files from all four locations are concatenated into a single system-prompt
fragment, each under a `# Memory (<scope>, from <path>)` heading so the model
can attribute an instruction. If the same instruction appears at several
levels, the narrower scope (Project/Local) effectively wins because it appears
later in the prompt.

### CLAUDE.md compatibility

Files named `CLAUDE.md` are read from the same locations as `AGENTS.md`, for
compatibility with the TypeScript Claude Code CLI. A project may hold both.

Two independent keys decide which of the two names is read:

| Key                | Type    | Default | Description                                     |
|--------------------|---------|---------|-------------------------------------------------|
| `agentsMdEnabled`  | boolean | true    | Read `AGENTS.md` at every scope.                |
| `claudeMdEnabled`  | boolean | false   | Read `CLAUDE.md` at every scope.                |

Turn either on or off from the TUI with `/settings` → **Read AGENTS.md** and
**Read CLAUDE.md**. Both on reads both files; both off reads neither, which is
the same result as `--no-claude-md`. The Managed scope is unaffected: files
under `rules/` carry neither name and are always read.

Within one scope `AGENTS.md` comes before `CLAUDE.md` unless a `priority` in
the frontmatter says otherwise.

### YAML frontmatter

AGENTS.md files may begin with optional YAML frontmatter to control loading:

```markdown
---
memory_type: project
priority: 10
scope: project
---

# My Project Notes

Always use 4-space indentation. Prefer `anyhow` for error handling.
```

Frontmatter fields:

| Field         | Description                                                                      |
|---------------|----------------------------------------------------------------------------------|
| `memory_type` | Informal label. Read but not acted on.                                           |
| `priority`    | Integer sort order within one scope. Lower numbers come first; a file that sets no priority sorts after every file that does. |
| `scope`       | Informal label. Read but not acted on.                                           |

The frontmatter block itself is stripped before the file reaches the model.

### @include directives

AGENTS.md files support `@include` to pull in content from other files:

```markdown
# Project Guide

@include ./docs/architecture.md
@include ~/shared-notes/coding-standards.md
```

Paths may be relative to the including file, absolute, or tilde-expanded.
Circular includes are detected and skipped, and nesting stops at ten levels.
A file that cannot be read leaves an HTML comment in its place.

There is no size limit, on the including file or on what it pulls in. A deep
`@include` tree can therefore fill the context; what you write is what is
sent.

### Disabling memory loading

To skip every memory file for a session:

```bash
mikmik --no-claude-md "your prompt"
```

This covers all four scopes and both filenames. `--bare` implies it, and also
disables hooks and plugins.

---

## Auto memory

A second memory store, separate from AGENTS.md. AGENTS.md is a file you write
and commit; the auto memory directory is one MikMik keeps for you, outside the
checkout, and the model writes to it during a session.

| Key                 | Type    | Default | Description                                               |
|---------------------|---------|---------|-----------------------------------------------------------|
| `autoMemoryEnabled` | boolean | false   | Keep the directory and show it to the model.              |

Off by default. Turn it on with `/settings` → **Auto memory**, and `/memory`
reports the directory's path and what it holds.

### What is in the directory

The directory lives beside the project's transcripts, under
`~/.config/mikmik/projects/<encoded-project-root>/memory/`. The project root is
the repository root, so a session started in a subdirectory reads the same
memory.

It holds `MEMORY.md`, an index the model keeps by hand, plus one `.md` file per
topic. A topic file opens with YAML frontmatter:

```markdown
---
name: Deploy
description: How releases reach production
type: project
---

Tag the commit, then wait for the release workflow.
```

`type` is one of `user`, `feedback`, `project` or `reference`. `name` and
`description` are what a search scores against, so a file without them is
harder for the model to find.

Session extraction writes `session-notes.md` there on its own.

### What the model sees

While the feature is on, the system prompt carries a `<memory>` block naming
the directory, the `MEMORY.md` index (capped at 200 lines and 25 KB), and a
one-line manifest entry per file with its type, description and age. Bodies are
not loaded; the model reads one with the `Memory` tool, which is offered only
while the feature is on.

Each body the tool returns is prefixed with a staleness note when the file is
more than a day old, because a memory is a point-in-time observation and a
file:line citation in it may no longer hold.

### Environment variables

| Variable                      | Effect                                                                    |
|-------------------------------|---------------------------------------------------------------------------|
| `MIKMIK_DISABLE_AUTO_MEMORY`  | Truthy turns it off whatever the setting says; defined-but-falsy turns it on. |
| `MIKMIK_MEMORY_PATH_OVERRIDE` | Full path to use as the memory directory, bypassing the project layout.   |
| `MIKMIK_REMOTE_MEMORY_DIR`    | Base directory to derive the project's memory directory from.             |

`--bare` (`MIKMIK_SIMPLE`) turns auto memory off along with everything else.

---

## Providers

MikMik can send requests to multiple LLM providers. Set the active provider
via the `provider` key in settings or the `--provider` CLI flag.

### Provider IDs

| Provider ID      | Default model                             |
|------------------|-------------------------------------------|
| `anthropic`      | `claude-sonnet-4-6` (or latest)           |
| `openai`         | `gpt-4o`                                  |
| `google`         | `gemini-2.5-flash`                        |
| `groq`           | `llama-3.3-70b-versatile`                 |
| `cerebras`       | `llama-3.3-70b`                           |
| `deepseek`       | `deepseek-chat`                           |
| `mistral`        | `mistral-large-latest`                    |
| `xai`            | `grok-2`                                  |
| `openrouter`     | `anthropic/claude-sonnet-4`               |
| `togetherai`     | `meta-llama/Llama-3.3-70B-Instruct-Turbo` |
| `perplexity`     | `sonar-pro`                               |
| `cohere`         | `command-r-plus`                          |
| `deepinfra`      | `meta-llama/Llama-3.3-70B-Instruct`       |
| `github-copilot` | `gpt-4o`                                  |
| `ollama`         | `llama3.2`                                |
| `lmstudio`       | `default`                                 |
| `llamacpp`       | `default`                                 |
| `azure`          | `gpt-4o`                                  |
| `amazon-bedrock` | `anthropic.claude-sonnet-4-6-v1`          |
| `venice`         | `llama-3.3-70b`                           |

### Per-provider configuration

Each provider can have its own entry in the `providers` map (top-level in
`settings.json`) or in `config.provider_configs`. Provider-level `api_key`
and `api_base` override the corresponding environment variables.

```json
"providers": {
  "anthropic": {
    "api_key": "sk-ant-...",
    "api_base": "https://api.anthropic.com",
    "enabled": true,
    "models_whitelist": [],
    "models_blacklist": []
  },
  "openai": {
    "api_key": "sk-...",
    "enabled": true
  },
  "ollama": {
    "api_base": "http://localhost:11434",
    "enabled": true
  }
}
```

`ProviderConfig` fields:

| Field              | Type           | Description                                     |
|--------------------|----------------|-------------------------------------------------|
| `api_key`          | string \| null | API key for this provider.                      |
| `api_base`         | string \| null | Override the default API base URL.              |
| `enabled`          | boolean        | Whether this provider is active. Default: true. |
| `models_whitelist` | array          | If non-empty, only these model IDs are offered. |
| `models_blacklist` | array          | These model IDs are never offered.              |
| `options`          | object         | Provider-specific passthrough options.          |

#### `options`

Every key in `options` is copied verbatim into the request body mikmik sends
for that account. This is how an endpoint mikmik does not recognise asks for
behaviour it supports:

```json
"providers": {
  "my-gateway": {
    "api_base": "https://llm.internal.example/v1",
    "protocol": "openai",
    "options": {
      "reasoningEffort": "high"
    }
  }
}
```

MikMik also fills some of these fields itself for the vendors it knows, and
those built-in values win. `reasoningEffort` for a GitHub Copilot or Codex
account, for instance, comes from the effort level `/effort` and the model
picker set, so writing it in `options` there has no effect. The setting reaches
the request only for fields mikmik does not set for that wire format, which is
all of them for an endpoint it does not recognise.

Which built-in rules apply is decided by `protocol` (the wire format), not by
the name the account is filed under.

---

## Environment Variables

| Variable               | Description                                                                     |
|------------------------|---------------------------------------------------------------------------------|
| `ANTHROPIC_API_KEY`    | Anthropic API key. Checked after the `config.api_key` setting.                  |
| `ANTHROPIC_BASE_URL`   | Override the Anthropic API base URL.                                            |
| `MIKMIK_PROVIDER`     | Active provider. Equivalent to `--provider`.                                    |
| `MIKMIK_API_BASE`     | Override the API base URL for the active provider. Equivalent to `--api_base`.  |
| `MIKMIK_GOALS`        | Set to `0` to disable the goal system (`/goal` command and `GoalComplete`). |
| `OPENAI_API_KEY`       | API key for the `openai` provider.                                              |
| `GOOGLE_API_KEY`       | API key for the `google` provider.                                              |
| `GROQ_API_KEY`         | API key for the `groq` provider.                                                |
| `XAI_API_KEY`          | API key for the `xai` provider.                                                 |
| `MISTRAL_API_KEY`      | API key for the `mistral` provider.                                             |
| `OPENROUTER_API_KEY`   | API key for the `openrouter` provider.                                          |
| `DEEPSEEK_API_KEY`     | API key for the `deepseek` provider.                                            |
| `COHERE_API_KEY`       | API key for the `cohere` provider.                                              |
| `DEEPINFRA_API_KEY`    | API key for the `deepinfra` provider.                                           |
| `VENICE_API_KEY`       | API key for the `venice` provider.                                              |
| `GITHUB_TOKEN`         | Token for the `github-copilot` provider.                                        |
| `AZURE_API_KEY`        | API key for the `azure` provider.                                               |
| `HF_TOKEN`             | Token for the `huggingface` provider.                                           |
| `NVIDIA_API_KEY`       | API key for the `nvidia` provider.                                              |
| `MIKMIK_BRIDGE_URL`   | Relay address for the remote-control bridge. Overrides `remoteControl.url`.     |
| `MIKMIK_BRIDGE_TOKEN` | Bearer token for the remote-control bridge. Overrides `remoteControl.token`.    |
| `RUST_LOG`             | Tracing filter (e.g. `debug`, `mikmik_core=trace`).                            |

---

## Custom Slash Commands

User-defined slash commands can be added to the `commands` map:

```json
"commands": {
  "review": {
    "template": "Please review the following code for bugs and style: $ARGUMENTS",
    "description": "Review code",
    "agent": "plan",
    "model": null
  }
}
```

`CommandTemplate` fields:

| Field         | Description                                                                                    |
|---------------|------------------------------------------------------------------------------------------------|
| `template`    | Template string. `$ARGUMENTS` is replaced with whatever the user types after the command name. |
| `description` | Short description shown in `/help`.                                                            |
| `agent`       | Optional named agent to use (e.g. `"plan"`, `"build"`, `"explore"`).                           |
| `model`       | Optional model override for this command.                                                      |

Use the command with `/review path/to/file.rs`.

---

## Named Agents

Agents are named configurations that combine a system prompt prefix, model,
permission level, and turn limit. Three are built in:

| Agent     | Access      | Description                                                   |
|-----------|-------------|---------------------------------------------------------------|
| `build`   | full        | Read, write, and execute. For feature implementation.         |
| `plan`    | read-only   | Read files; no writes or commands. For analysis and planning. |
| `explore` | search-only | Search and read. For rapid codebase exploration.              |

You can define custom agents in `settings.json`:

```json
"agents": {
  "review": {
    "description": "Code review agent",
    "model": "anthropic/claude-haiku-4-5",
    "temperature": 0.3,
    "prompt": "You are a senior engineer doing code review. Be thorough and direct.",
    "access": "read-only",
    "visible": true,
    "max_turns": 30,
    "color": "magenta"
  }
}
```

`AgentDefinition` fields:

| Field         | Type            | Description                                                            |
|---------------|-----------------|------------------------------------------------------------------------|
| `description` | string \| null  | Description shown in `@agent` autocomplete.                            |
| `model`       | string \| null  | Model override for this agent.                                         |
| `temperature` | float \| null   | Sampling temperature override.                                         |
| `prompt`      | string \| null  | System prompt prefix (prepended before the main system prompt).        |
| `access`      | string          | Permission level: `"full"`, `"read-only"`, or `"search-only"`.         |
| `visible`     | boolean         | Whether to show in autocomplete. Default: true.                        |
| `max_turns`   | integer \| null | Maximum agentic turns.                                                 |
| `color`       | string \| null  | ANSI display color: `"cyan"`, `"magenta"`, `"green"`, `"yellow"`, etc. |

Invoke an agent with `@agentname` in the TUI or `--agent agentname` on the CLI.

---

## Managed Agents Configuration

The `managed_agents` key stores the managed-agents architecture configuration set via `/managed-agents configure`. It is written automatically by the command and rarely needs to be edited manually.

```json
"managed_agents": {
  "enabled": true,
  "manager_model": "anthropic/claude-opus-4-6",
  "executor_model": "anthropic/claude-sonnet-4-6",
  "executor_max_turns": 20,
  "max_concurrent": 3,
  "executor_isolation": true,
  "budget_split": {
    "type": "Percentage",
    "manager_pct": 20
  },
  "total_budget_usd": 5.00
}
```

`budget_split` types:

| Type         | JSON                                                                 | Description                        |
|--------------|----------------------------------------------------------------------|------------------------------------|
| `SharedPool` | `{ "type": "SharedPool" }`                                           | All agents draw from a single pool |
| `Percentage` | `{ "type": "Percentage", "manager_pct": 20 }`                        | Manager gets N% of total budget    |
| `FixedCaps`  | `{ "type": "FixedCaps", "manager_usd": 0.50, "executor_usd": 2.00 }` | Hard USD caps per role             |

Configure via `/managed-agents configure` or `/managed-agents preset <name>`. Set `enabled: false` to disable without removing the configuration.

---

## File Formatters

Formatters run automatically after MikMik writes a file whose extension
matches. They are defined in the `formatter` map:

```json
"formatter": {
  "prettier": {
    "command": ["prettier", "--write"],
    "extensions": [".ts", ".tsx", ".js", ".json"],
    "disabled": false
  },
  "rustfmt": {
    "command": ["rustfmt"],
    "extensions": [".rs"],
    "disabled": false
  }
}
```

| Field        | Description                                                       |
|--------------|-------------------------------------------------------------------|
| `command`    | Command array. `$FILE` or `{file}` marks where the path goes; without either, it is appended as the final argument. |
| `extensions` | File extensions this formatter handles (include the leading dot). |
| `disabled`   | Set to true to temporarily disable without removing the entry.    |

Only the first formatter whose extensions match runs. It is given 30 seconds,
and its failures are ignored: a file that did not get formatted is not worth
interrupting a turn for.

---

## Annotated Example `settings.json`

```json
{
  // Settings schema version
  "version": 1,

  // Active provider (can be overridden per-session with --provider)
  "provider": "anthropic",

  "config": {
    // Omit api_key here; use ANTHROPIC_API_KEY env var instead
    "api_key": null,

    // Model — leave null to use the provider's default
    "model": null,

    // Cap responses at 32 000 tokens
    "max_tokens": 32000,

    // In the TUI, ask before writing files or running commands
    "permission_mode": "default",

    // Dark theme for the TUI
    "theme": "dark",

    // Compact when the context window is 85% full
    "auto_compact": true,
    "compact_threshold": 85,

    // Show debug logs
    "verbose": false,

    // Plain text output in --print mode
    "output_format": "text",

    // Add a custom instruction to every session
    "append_system_prompt": "Always explain your reasoning before making changes.",

    // Block the Bash tool globally
    "disallowed_tools": ["Bash"],

    // Inject a variable into every tool execution
    "env": {
      "MY_PROJECT_TOKEN": "{env:HOME}/.project_token"
    },

    // Run a script after every tool use
    "hooks": {
      "PostToolUse": [
        {
          "command": "/home/user/scripts/audit-log.sh",
          "blocking": false
        }
      ]
    },

    // Connect an MCP server at startup
    "mcp_servers": [
      {
        "name": "filesystem",
        "command": "mcp-server-filesystem",
        "args": ["/home/user/projects"],
        "env": {},
        "type": "stdio"
      }
    ]
  },

  // Per-provider credentials and options
  "providers": {
    "anthropic": {
      "api_key": null,
      "enabled": true
    },
    "openai": {
      "api_key": "sk-...",
      "enabled": true
    },
    "ollama": {
      "api_base": "http://localhost:11434",
      "enabled": true
    }
  },

  // Correct metadata for self-hosted / unknown models (keyed by provider/model).
  // Overrides win over the models.dev catalog.
  "modelOverrides": {
    "custom-openai/my-local-llm": {
      "contextWindow": 32768,
      "maxOutputTokens": 4096,
      "name": "My Local LLM"
    }
  },

  // Custom slash commands
  "commands": {
    "test": {
      "template": "Run the tests for $ARGUMENTS and report any failures.",
      "description": "Run and report tests"
    }
  },

  // Auto-run prettier on JS/TS file writes
  "formatter": {
    "prettier": {
      "command": ["prettier", "--write"],
      "extensions": [".ts", ".tsx", ".js", ".jsx"],
      "disabled": false
    }
  }
}
```
