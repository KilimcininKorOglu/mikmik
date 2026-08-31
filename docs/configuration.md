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
`additional_dirs`, `statusLine`, `acpAgents`, `remoteControl`, `workspace`,
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
  "showToolDuration": false,
  "showTurnDuration": false,
  "showUsageLimits": false,
  "advisorModel": "claude-opus-4-6",
  "advisorMode": "tool",
  "memoryModel": "claude-haiku-4-5",
  "companion": { ... },
  "remoteControl": { ... },
  "workspace": { ... },
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

### Agents

An agent is a named persona: a system-prompt prefix, an optional model and
turn budget, and a tool-access level. Three come built in (`build`, `plan`,
`explore`). Define more in the `config.agents` map of a settings file, or drop
a markdown file in an agents folder.

An agent file is `<name>.md` with optional `---` frontmatter. The `name:` field
names the agent (the file stem is the fallback); the body after the frontmatter
is the agent's prompt. Recognised keys:

| Key           | Description                                                                 |
|---------------|-----------------------------------------------------------------------------|
| `name`        | Agent name; falls back to the file stem.                                    |
| `description` | One-line summary shown by `/agent`.                                         |
| `model`       | Model override, e.g. `anthropic/claude-haiku-4-5`.                          |
| `access`      | Tool access: `full`, `read-only`, or `search-only`.                        |
| `max_turns`   | Turn budget for this agent.                                                 |
| `temperature` | Sampling temperature override.                                              |
| `color`       | Display colour.                                                             |
| `visible`     | `false` hides the agent from `@agent` autocomplete.                        |
| `tools`       | Claude Code compatibility: a comma-separated tool list. When `access` is absent, an all-search list infers `search-only`, anything else stays `full`. |

Searched folders, lowest priority first: `~/.claude/agents/`,
`<mikmik home>/agents/` (`~/.config/mikmik/agents/`), then `.claude/agents/`
and `.mikmik/agents/` walking up from the working directory. A later source
overrides an earlier one of the same name, so a project `.mikmik/agents/` file
wins over a `settings.json` entry, which wins over a built-in. `.claude/agents/`
files are read for Claude Code compatibility.

Select an agent for the session with `/agent <name>` or the `--agent` flag; list
them with `/agent`. The `Agent` tool also takes an `agent` parameter, so the
model can spawn any named agent as a sub-agent: its prompt, model, `max_turns`
and access-derived tool set become the sub-agent's defaults, and any field the
spawn sets explicitly still wins.

### Transcript display

| Key                     | Type    | Default | Description                                                                                                                                                                    |
|-------------------------|---------|---------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `showMessageTimestamps` | boolean | false   | Print the local time beneath each message. Times are stored in UTC and converted using the machine's time zone. Messages from an earlier day also show their date (`13 Aug 14:32`). |
| `showToolDuration`      | boolean | false   | Print how long each tool call took, at the bottom right of the tool block. Reads `240ms`, `1.4s` or `2m05s`. |
| `showTurnDuration`      | boolean | false   | Keep a line under the input box after a turn finishes, reading `✻ Worked for 2m 5s`. It stays until the next message is sent. |
| `showUsageLimits`       | boolean | false   | Show the active account's quota/rate limits in the timeline sidebar: one row per meter (a 5-hour window, a 7-day window, and so on) with its used percent. Turning it on lets the app fetch the account's usage from its own endpoint, at most once a minute while the panel is open. |

Toggle them from the TUI with `/config` → **Show message timestamps**, **Show
tool duration** and **Show turn duration**. All three take effect on the next
draw; none needs a restart. Turns restored from a transcript recorded before
either duration option existed carry no time and render without one.

`showTurnDuration` shares its line with the status message, and the status
message wins: a session that is reconnecting says so rather than reporting how
long the last turn took.

`showToolDuration` measures the tool's own work. The wait for a permission
prompt the central gate raises is not counted, because that is how long you
took to answer rather than what the call cost; a tool that prompts inside its
own `execute()`, as `Bash` does, still counts it. A call that was blocked or
cancelled before it ran reports nothing at all rather than zero.

Durations of tools that ran at the same time overlap, so a turn's tool
durations can add up to more than the turn took.

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

### Shell

| Key                | Type   | Default  | Description                                              |
|--------------------|--------|----------|----------------------------------------------------------|
| `bashEngine`       | string  | `brush`  | `brush` or `system`. Which shell the `Bash` tool runs commands in. |
| `bundledUtilities` | string  | `prefer` | `prefer` or `fallback`. Which copy of `ls`, `sort` and the rest a command reaches for. |
| `outputFilter`     | boolean | `false`  | Compress noisy command output (make, terraform, tsc, pytest, …) before the model reads it. Opt-in. |

Inside the `config` object, not at the top level:

```json
"config": {
  "bashEngine": "system"
}
```

`brush` is the shell embedded in the MikMik binary. Nothing is spawned to interpret a command, and the shell outlives it, so the working directory, exported variables, aliases and shell functions all persist between calls. The same shell runs on macOS, Linux and Windows.

`system` runs the machine's own `bash`, spawned once per command, which is what MikMik did before the embedded shell existed. The working directory and exported variables are carried forward; a shell function defined by one command is not. Use it if a command behaves differently under the embedded shell: brush states that it is not production-complete, and `select` and some edge cases are unsupported.

Windows ignores the setting and always uses `brush`. `system` there meant `cmd /C`, which fails on the first pipeline.

Either way an external program is still a real process. The embedded shell removes the shell and its built-ins from the path, not the `cargo` or `git` the command names.

#### Which utilities a command gets

82 coreutils (`ls`, `cat`, `sort`, `wc` and the rest) plus `find`, `xargs`, `sed` and `jq` ship inside the binary. They run in the MikMik process: no fork, no exec, and nothing to install. That is what makes a command work on Windows or in a stripped container image.

```json
"config": {
  "bundledUtilities": "fallback"
}
```

`prefer`, the default, runs the carried copy for every name it carries. It is the faster answer (`ls -1` costs 164 us against 2.54 ms for the machine's own binary on an Apple laptop) and it behaves the same on every machine.

`fallback` runs the carried copy only for a name the machine does not have. Use it if a script was written against GNU coreutils and depends on a flag or an output detail the carried copy gets differently: the carried set is [uutils](https://github.com/uutils/coreutils), a reimplementation that aims at GNU compatibility rather than claiming it.

The shell's own built-ins win either way, so `echo`, `printf`, `test`, `true` and `false` keep bash semantics.

`bashEngine: "system"` ignores the setting: the machine's own `bash` looks up commands its own way.

#### Compressing command output

`outputFilter`, off by default, runs a command-aware filter over the `Bash` tool's output before the model reads it. It shrinks noisy output by 60-90%: 63 commands have a declarative filter (make, terraform, ping, df, gradle, xcodebuild and the rest), and tsc, pytest, mypy and prettier have a richer filter that groups errors by file. Turn it on inside the `config` object:

```json
"config": {
  "outputFilter": true
}
```

A never-worse guard reverts to the raw output whenever filtering would grow it, and a command with no matching filter passes through untouched, so the filter can only help or no-op. When the filter drops lines, or a command fails, the raw output is saved under the config directory and a hint is appended — `[full output: <path>]`, or `[see remaining: tail -n +N <path>]` when only a tail was cut — so the model can read what was cut without re-running the command.

### Edit guard

| Key         | Type   | Default | Description                                                    |
|-------------|--------|---------|----------------------------------------------------------------|
| `editGuard` | string | `off`   | `off`, `stale` or `strict`. How strictly an edit is held to what this session read. |

Inside the `config` object, not at the top level:

```json
"config": {
  "editGuard": "strict"
}
```

`Edit` addresses a file by content: `old_string` is both the address and its
own proof, because text that is not in the file cannot match. That proof covers
one thing. It says the text is in the file **now**. It says nothing about
whether the file is still the one the model read, and nothing about whether the
model ever saw that text.

`Read` and `Write` record the file's content hash and the line numbers they
displayed. `editGuard` decides what the editing tools do with that record.

| Level    | Refuses                                                                                          |
|----------|--------------------------------------------------------------------------------------------------|
| `off`    | Nothing. The behaviour this tree had before the guard existed.                                    |
| `stale`  | An edit to a file that changed after this session read it.                                        |
| `strict` | The above, and an edit to lines the session never displayed. The error quotes those lines back.    |

Two checks are on from `stale` up and have no level of their own: an edit that
would write back the bytes already on disk is refused, and the third identical
failed match against one file stops repeating advice that has failed twice.

Every check is silent for a file this session never read. Enforcing
read-before-edit is a different policy and is not implemented; an edit to an
unread file behaves exactly as it did before.

A partial `Read` narrows what `strict` allows, which is the point: an
`offset`/`limit` read of a large file used to leave every other line editable
blind. Two partial reads add up, and a whole-file read leaves nothing unseen.
After an edit the record follows the change, so consecutive edits to one file
work without re-reading.

A project's `settings.json` may raise this level and may never lower it. A
checkout that could set `off` would switch off a guard the user turned on, and
the first thing that would hide is a file the same checkout changed underneath
the agent.

### Advisor

| Key                   | Type   | Default | Description                                                                                                                                       |
|-----------------------|--------|---------|-------------------------------------------------------------------------------------------------------------------------------------------------------|
| `advisorModel`        | string | unset   | Second model consulted for a review. A bare ID runs against the active provider; `provider/model` targets a specific one; `provider:account/model` also targets a specific stored login. Unset disables the advisor. |
| `advisorMode`         | string | `tool`  | `tool`, `runtime`, `both` or `off`. See below.                                                                                                   |
| `advisorSyncBacklog`  | number | 3       | How many reviews the watcher may be behind before the agent waits for it at the end of a turn. `0` never waits. The wait is capped at 30 seconds, and a watcher that is failing releases the agent at once. |
| `advisorImmuneTurns`  | number | 3       | How many turns a delivered interruption silences the next `concern` for. A `blocker` is exempt.                                                   |

Set them with [`/advisor`](commands.md#advisor) or from the settings screen
rather than by hand. When `advisorModel` is unset, no advisor runs at all,
whatever the mode says. A project's `settings.json` cannot change any of these
four keys: each decides that a second model runs, or which one, and that is the
user's call.

#### The four modes

| Mode      | Behaviour                                                                                                            |
|-----------|----------------------------------------------------------------------------------------------------------------------|
| `tool`    | The default. The main model consults the advisor through the [`Advisor`](tools.md#advisor) tool when it decides to.  |
| `runtime` | A watcher reads every turn on its own and speaks unasked. The `Advisor` tool is not offered.                          |
| `both`    | Both at once.                                                                                                        |
| `off`     | Neither, even with `advisorModel` set.                                                                               |

#### The watcher

In `runtime` mode a second model reads each turn as it happens: what the agent
wrote, what it called, and what came back. It has its own read-only tools
(`Read`, `Grep`, `Glob`) to check a suspicion before raising it, and it answers
with the [`Advise`](tools.md#advise) tool or with silence.

A note carries one of three severities:

| Severity  | While a turn is streaming | After the turn ended                    |
|-----------|---------------------------|-----------------------------------------|
| `nit`     | Waits for the next turn boundary | Waits for the next turn boundary  |
| `concern` | Stops the turn            | Stays in the conversation for next time |
| `blocker` | Stops the turn            | Wakes the finished turn                 |

A repeated note never gets through twice, and content-free notes ("looks good",
"continue") are dropped. The watcher reads tool output, which is untrusted, and
what it writes goes into the agent's context, so every note is scanned for
destructive shell directives and instruction-override patterns before it
crosses that line.

The watcher keeps its own JSONL transcript beside the session's, so its tokens
never count against the session's context and its turns never appear in
`/resume`. Its spend is a separate line in `/cost`, at its own model's rates.

#### The roster

Without a roster one unnamed watcher runs on `advisorModel`. To run several,
each with its own brief, put a markdown file in `<config root>/advisors/` or
`<project root>/.mikmik/advisors/`:

```markdown
---
name: Architecture
enabled: true
model: anthropic/claude-sonnet-5
tools: Read, Grep, Glob
---

Watch cross-module coupling and public-API growth.
```

A **project** entry gives only `name`, `enabled` and its body. Its `model` and
`tools` are ignored and replaced with the default model and the read-only tool
set: a repository cannot decide which endpoint costs you money, or what runs on
your machine. A **user** entry sets both.

#### ADVISOR.md

Guidance for the watcher only, never for the main model: the traps in this
project, the dangerous APIs, the boundaries worth watching. Read from
`<config root>/ADVISOR.md`, `<project root>/ADVISOR.md` and
`<project root>/.mikmik/ADVISOR.md`, in that order.

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

A separate top-level key controls whether the bridge opens on its own:

| Key                       | Type    | Default | Description                                                                                     |
|---------------------------|---------|---------|-------------------------------------------------------------------------------------------------|
| `remoteControlAtStartup`  | boolean | false   | Open the remote-control bridge when the session starts, so a web or mobile client can join without a command. Editable from `/settings`. |

### Workspace server

The organisation's configuration server: the providers it assigns you, the
settings policy it enforces, and your own settings backup. See
[Workspace server](workspace-server.md).

| Key    | Type   | Default  | Description                                                                        |
|--------|--------|----------|------------------------------------------------------------------------------------|
| `url`  | string | unset    | Base address of the server, for example `https://mikmik.firma.com`. A trailing slash is trimmed. |
| `sync` | object | see below| When this installation talks to it.                                                 |

The address must be `https`, unless the host is `localhost`, `127.0.0.1` or
`[::1]`. Signing in sends a password, and the answer carries every provider key
the organisation assigned you; in the clear, one network hop reads both.

No credential lives here. The session token goes to `auth.json`, which is
written `0o600`, and it carries the address it was issued for, so a token good
for one organisation is never sent to another.

| `sync` key        | Type          | Default | Description                                                                       |
|-------------------|---------------|---------|-------------------------------------------------------------------------------------|
| `onChange`        | boolean       | `true`  | Upload once the writes to `settings.json` stop.                                      |
| `intervalMinutes` | number \| null| unset   | Upload on a timer as well. Values below 5 are raised to 5.                          |
| `pullAtStartup`   | boolean       | `true`  | Take the providers and the policy when a session starts.                            |

```json
"workspace": {
  "url": "https://mikmik.firma.com",
  "sync": {
    "onChange": true,
    "intervalMinutes": 60,
    "pullAtStartup": true
  }
}
```

Every trigger is on unless it is written off: the section exists only because
you signed in to a server, and a backup that never runs is not there on the day
the machine is rebuilt.

This block is read from the user settings file only. A project settings file
cannot set it, because the server it names decides which providers this
installation may use and pushes a policy the user cannot override.

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
| `output_format` | string         | `"text"`    | Output format for headless (`--print`) mode. In `settings.json` one of `"text"`, `"json"`, `"streamjson"` (lowercase, no separator); the `--output-format` CLI flag spells the last one `stream-json`.                                                                    |
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
| `maxConcurrentSubagents` | integer      | 0       | Ceiling on how many sub-agents (the `Agent` tool) run at once in a session. `0` means unlimited, so the default changes nothing. A higher value queues extra spawns, including a foreground batch fan-out, at the limit. Managed-orchestrator mode uses its own `max_concurrent_executors` instead. |

All four are editable from `/settings`. Turning **SearXNG** on there prompts for
the address and writes it to `searxngUrl`; turning it off clears the key.
**Max concurrent sub-agents** is a plain number, `0` for unlimited.

### Interface

| Key               | Type    | Default | Description                                                                                                                                                  |
|-------------------|---------|---------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `timelineEnabled` | boolean | false   | Record every tool call, finished turn, todo update, and plan-mode change, and offer the panel through `/timeline` and `Ctrl+Shift+L`. When enabled the panel shows from the start of the session. Off by default; while off nothing is collected at all. |
| `mouseCapture`    | boolean | true    | Let MikMik handle the mouse: wheel scrolling, right-click menus and drag-select. Turn it off to give the mouse back to the terminal. Applies at next start. |
| `liveToolOutput`  | boolean | false   | Show what a shell command prints while it is still running, under the tool block, instead of only its preview when it ends. Off by default: a command that prints steadily redraws the transcript on every frame. |

All three are editable from `/settings`, as **Execution timeline**, **Mouse
capture** and **Live tool output**. See [Commands](commands.md#timeline) for what the timeline panel
shows.

### Which tools are offered

Each of these decides whether a group of tools reaches the model at all. A
withheld tool is absent rather than refused, so it costs nothing at run time
and no schema on any turn. All four are off by default and editable from
`/settings`.

| Key                  | Type    | Default | Description                                                                                                       |
|----------------------|---------|---------|-------------------------------------------------------------------------------------------------------------------|
| `teamsEnabled`       | boolean | false   | Offer `TeamCreate` and `TeamDelete`. `SendMessage` is unaffected: it also carries messages between the sub-agents `Agent` starts. |
| `cronEnabled`        | boolean | false   | Offer `CronCreate`, `CronDelete` and `CronList`. A job already scheduled keeps running either way.                 |
| `replEnabled`        | boolean | false   | Offer the persistent Python and JavaScript `REPL`.                                                                |
| `computerUseEnabled` | boolean | false   | Offer the desktop control tool. The `computer-use` Cargo feature is a separate axis and is on by default; a build made with `--no-default-features` does not carry the tool whatever this says. |
| `browserEnabled`     | boolean | false   | Offer the `browser` tool. Also needs a browser to drive (see below), or it stays out of the roster even when this is on. |
| `browserCdpUrl`      | string  | unset   | A running browser's CDP endpoint, e.g. `http://127.0.0.1:9222`. When set, `browser` attaches to it instead of launching one. |
| `browserExecutable`  | string  | unset   | Path to a Chrome or Chromium binary the `browser` tool launches headless when no `browserCdpUrl` is set. Unset falls back to a Chrome found on the PATH. |

A project's `.mikmik/settings.json` cannot turn any of them on. Each decides
whether a capability is offered, so a repository able to set one could hand
itself a shell, the desktop, scheduled execution, or a fleet of agents.

Four more groups are decided by the machine and the directory rather than by a
setting, on the same reasoning: a tool that could only report its own absence
is not offered.

- `ListMcpResources`, `ReadMcpResource` and `mcp__auth` need a connected MCP server.
- `EnterWorktree` and `ExitWorktree` need the session to be inside a git repository.
- `LSP` needs a configured language server, or one that auto-detection finds installed for this tree.
- `PowerShell` needs `pwsh` on the PATH, or Windows.
- `browser` needs a browser to drive: a `browserCdpUrl`, a `browserExecutable`, or a Chrome or Chromium on the PATH.

Measured in this repository: the roster went from 44 tools and 30,246
characters of tool definitions to 35 and 25,730 with nothing configured.

### Deferred tool schemas

| Key              | Type    | Default | Description                                                                                         |
|------------------|---------|---------|-------------------------------------------------------------------------------------------------------|
| `schemaDeferral` | boolean | false   | Declare only the core tools each turn, plus whatever `ToolSearch` has found so far in this session. |

Off by default, which is what MikMik has always done: every tool in the roster
is declared on every turn. On, a turn declares a fixed core set (`Read`,
`Write`, `Edit`, `Bash`, `Grep`, `Glob`, `Agent`, `TodoWrite`,
`AskUserQuestion`, `EnterPlanMode`, `ExitPlanMode`, `ToolSearch`) and anything
`ToolSearch` has answered. A found tool stays declared for the rest of the
session, so the model never searches twice for the same one.

Measured in this repository with nothing else configured: 35 tools and 25,730
characters with the setting off, 12 and 11,483 on the first turn with it on,
and 14 and 14,663 after a search that found two more.

Withholding a schema does not withhold the tool. A call is looked up in the
roster, so a model that names an undeclared tool correctly still runs it; the
setting decides only what is advertised. Turning it on also adds a short
section to the system prompt telling the model that the tools it can see are
not all the tools there are.

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

| Key                    | Type                       | Default | Description                                                                    |
|------------------------|----------------------------|---------|--------------------------------------------------------------------------------|
| `lsp_servers`          | array of `LspServerConfig` | []      | Language servers the LSP tool may use. Field list in [tools.md#lsp](tools.md#lsp). |
| `lsp_auto_detect`      | boolean                    | true    | Consult the bundled catalogue of language servers.                             |
| `lsp_warmup_on_start`  | boolean                    | false   | Start the project's servers with the session instead of on the first request.   |
| `lsp_idle_timeout_ms`  | number                     | unset   | Stop a language server after this long without a request.                      |
| `lsp_diagnostics_on_write` | boolean                | true    | Append the language server's new problems to the result of a write.            |
| `lsp_format_on_write`  | boolean                    | false   | Format a file with its language server after writing it.                       |

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

**A separate configuration file.** Language servers can also be configured
outside `settings.json`, which keeps a long server list out of the settings
file and lets a repository ship one. Four names are read, in this order of
preference within a directory: `lsp.json`, `.lsp.json`, `lsp.toml`,
`.lsp.toml`. Four directories are searched, lowest precedence first:

| Precedence | Directory                                  |
|-----------:|--------------------------------------------|
| Lowest     | The home directory                         |
|            | The configuration directory (`$CLAURST_HOME` or `~/.config/mikmik`) |
|            | `<project>/.mikmik/`                       |
| Highest    | The project root                           |

The shape is a map of server name to the fields in
[tools.md#lsp](tools.md#lsp), either under a `servers` key or at the top level:

```json
{
  "idle_timeout_ms": 300000,
  "servers": {
    "rust-analyzer": { "args": ["--log-file", "/tmp/ra.log"] },
    "eslint": { "disabled": true },
    "my-ls": {
      "command": "my-ls",
      "args": ["--stdio"],
      "file_patterns": ["*.xyz"],
      "root_markers": [".xyz-project"]
    }
  }
}
```

Merging is per field: the entry above changes rust-analyzer's arguments and
keeps its root markers, settings and everything else. `settings` and
`initialization_options` are replaced as a whole rather than merged key by key,
because a server reads each as one document.

A name that matches no known server is a new server, and needs `file_patterns`,
`root_markers`, and either `command` or a `tcp` address. An entry that lacks
them is reported in the log and dropped. A file that cannot be parsed is skipped the same way: refusing
to start over a stray comma in an optional file would be worse than running
without it.

`settings.json` wins over these files, and these files win over the catalogue.

`lsp_auto_detect` is taken from the user's settings alone. A project's
settings file cannot switch it on, because detection starts a process and the
markers that trigger it are files the repository itself carries.

The settings screen has a **Detect language servers** row for the same key.

**Starting early.** With `lsp_warmup_on_start` on, the servers detected for the
working directory start with the session rather than on the first request. A
server indexes the whole project before it can answer, and that wait otherwise
lands on the first request. It is off by default because it starts a process for
a session that may never touch code, and a large project's server holds a lot of
memory. The warmup runs in the background, so the session never waits for it,
and the servers that came up are named in a notification. Only detected servers
are started: one named in `lsp_servers` for another language has no reason to
run here. `lsp_auto_detect` has to be on as well, since that is what finds them.
Like the two keys above, a project's settings file cannot switch it on.

The settings screen has a **Start language servers early** row for the same key.

**Server lifetime.** A server starts on the first request that needs it and
runs until the session ends, when it is shut down and its process tree with it.
`lsp_idle_timeout_ms` stops one earlier, after the given time with no request.
Unset and zero both mean "keep it". Stopping a server is not free: the next
request pays for indexing the project again, which for a large workspace is
several seconds.

A server that fails to start is not retried for three minutes. Without that,
every request pays the same startup timeout again and one missing binary makes
the whole session slow.

One server runs per working directory, not per name, because a server is
initialized for one workspace root and answers against it. An entry with a `tcp`
address is different: nothing is started and nothing is stopped, because the
server was already running and other sessions may be on it. See
[tools.md#lsp](tools.md#lsp).

An entry that carries `lint_output` is a command-line linter rather than a
server. It has no lifetime: it is run over one file, its report is read, and it
exits. See [tools.md#lsp](tools.md#lsp).

**After a write.** With `lsp_diagnostics_on_write` on, a `Write`, `Edit` or
`BatchEdit` carries the language server's verdict on the file back with it, so
the model learns that its edit does not compile without running a build. Only
problems that were not reported for that file before are shown, and a problem
whose line number moved is not treated as new. The write waits at most 700 ms
for the answer, and a missing or slow server never turns a successful write into
a failed one. A `BatchEdit` spends that wait once for the whole batch, not once
per file.

A server that is slow, or busy with another file, answers after that wait is
over. Its answer is not thrown away: the next write reports it, under a heading
that says the file was written earlier. Only an answer published after the file
was last written counts, because an older one describes content that is gone.

`lsp_format_on_write` is off by default because it rewrites the file: a server
configured differently from the project's own formatter would reformat every
file the session touches. The [`formatter`](#tool-behaviour) setting runs the
project's own tool and is the safer choice. When on, the indent width and
whether tabs or spaces are used are read from the file itself, not from a
setting, so formatting one file does not re-indent it.

Both rows appear in the settings screen, as **Report problems after a write**
and **Format with the language server**.

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

Hooks may also live in a folder instead of this key: any `*.json`/`*.jsonc`
file in `~/.config/mikmik/hooks/` (user) or a project's `.mikmik/hooks/`
carries the same event-to-entries shape and merges into the same map. Project
folder hooks pass the same trust gate as project `settings.json` hooks; the
user's own folder runs directly. See [Hooks](hooks#where-settings-hooks-come-from).

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
| Local   | `<project-root>/.mikmik/rules/*.md` | The project's own rule files, loaded in alphabetical order. Usually [conditional rules](#conditional-rules). |

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
| `description` | One line saying what the file is about. Shown by `mikmik rules list`.            |
| `condition`   | Makes the file a [conditional rule](#conditional-rules).                          |
| `scope`, `globs`, `on_match`, `repeat` | Only read on a conditional rule. See below.             |

The frontmatter block itself is stripped before the file reaches the model.

A value may be quoted or bare, and a list may be written on one line or as a
block:

```yaml
condition: "Box::leak"
globs: *.rs, *.toml
scope:
  - text
  - thinking
```

A quoted value loses its quotes and its escapes, so `"\\.unwrap\\(\\)"` reaches
the regex engine as `\.unwrap\(\)`. A single-quoted value keeps its backslashes,
so `'runtime\.SetFinalizer'` needs no doubling.

### Conditional rules

A memory file with a `condition` is a **rule**. It leaves the system prompt and
waits. When the model writes something one of its regular expressions matches,
the rule is put in front of it, at that moment and only then. A rule you never
break costs nothing.

```markdown
---
description: Never use Box::leak, it intentionally leaks memory
condition: "Box::leak"
scope: "tool:Edit(*.rs), tool:Write(*.rs)"
---

Never use `Box::leak` to satisfy a lifetime. Use `Arc<T>` for shared data, or
`LazyLock<T>` for lazy global state.
```

| Field       | Meaning                                                                            | Default |
|-------------|------------------------------------------------------------------------------------|---------|
| `condition` | One regular expression, or a list of them. Any match wakes the rule.                | required |
| `scope`     | `text`, `thinking`, `tool`, `tool:Edit`, `tool:Edit(*.rs)`; comma-separated.        | `tool` |
| `globs`     | A second file-path gate, applied on top of `scope`.                                 | none |
| `on_match`  | `remind` runs the call and puts the rule on top of its result. `block` refuses the call and answers with the rule. | `remind` |
| `repeat`    | `once`, `always`, or a number of turns to wait before speaking again.               | `once` |

**What a rule reads.** Not the whole tool call, only the text it would
introduce. An `Edit` that **removes** a `.unwrap()` does not trip the rule that
forbids writing one, because only `new_string` is matched.

| Tool           | Read                                | Path used for `scope` and `globs` |
|----------------|-------------------------------------|-----------------------------------|
| `Write`        | `content`                           | `file_path`                       |
| `Edit`         | `new_string`                        | `file_path`                       |
| `BatchEdit`    | each edit's `new_string`            | each edit's `file_path`           |
| `NotebookEdit` | `new_source`                        | `notebook_path`                   |
| `ApplyPatch`   | the lines the patch adds            | the paths the patch writes        |
| `Bash`         | `command`                           | none                              |
| anything else  | every string argument, and only when a rule names that tool in its `scope` | none |

A tool name is matched without regard to case, so a rule file written for
another agent (`tool:edit`) still works. A pattern such as `*.rs` matches a
nested path: writing `**/*.rs` is not required. Brace groups (`*.{ts,tsx}`) are
expanded.

**Rules on what the model writes.** `scope: text` reads the answer and
`scope: thinking` reads the reasoning, as it arrives. A match there stops the
turn at that point: the half-written answer is thrown away, the rule is handed
to the model, and it writes the turn again. `on_match` does not apply, because
there is no tool result for the rule to ride on; `mikmik rules list` prints
**interrupt** for these so nothing is hidden.

```markdown
---
description: Say what the code does, not what it probably does
condition: "probably fine|should be okay"
scope: text
---

Do not hedge about behaviour you can check. Run it, then say what happened.
```

Three interruptions per query is the limit. A retried turn does not count
against `max_turns`, so without that limit a `repeat: always` rule could hold
the query open. `globs` has nothing to gate here and is ignored.

None of the rules that ship with the binary watch prose.

**Repeat.** A rule speaks once per session by default, because a rule that
repeats on every turn becomes noise and noise is ignored. `repeat: always` says
it every time, and `repeat: 10` says it again after ten turns.

Which rules spoke is written to the session's transcript, so resuming a session
does not repeat them about work that is already done. Rewinding the
conversation does not unsay them either: the rule reached the model once.

**Switches.**

| Key              | Type      | Default | Description                                       |
|------------------|-----------|---------|---------------------------------------------------|
| `rules_enabled`  | boolean   | true    | Whether conditional rules run at all.             |
| `rules_builtin`  | boolean   | true    | Whether the rules that ship with the binary run.  |
| `rules_disabled` | string[]  | []      | File stems of rules that must not run.            |

All three come from your own settings alone. A project may **add** a rule,
because a rule only restricts what the model writes; it cannot switch off or
drop a rule you set for yourself. The settings screen carries **Conditional
rules** and **Built-in rules** rows.

A condition that does not compile is reported in the log and skipped, and the
session starts. A rule left with no usable condition is dropped.

### The rules that ship with the binary

Sixty-one rules cover the mistakes a pattern can catch. They are on by
default. A rule of the same name in your own directories **replaces** the
built-in one, so disagreeing with one means rewriting it rather than only
switching it off.

| Group | Rules |
|-------|-------|
| Git   | `git-add-all`, `git-destructive`. Both **block**: they refuse the command rather than comment on it afterwards, because by then the work is gone. |
| Rust  | `rs-no-unwrap`, `rs-unsafe-safety`, `rs-box-leak`, `rs-lazylock`, `rs-parking-lot`, `rs-match-ergonomics`, `rs-future-prelude`, `rs-result-type` |
| Go    | `go-add-cleanup`, `go-bench-loop`, `go-exp-promoted`, `go-ioutil`, `go-join-hostport`, `go-new-expr`, `go-rand-v2`, `go-range-int` |
| TypeScript | `ts-no-any`, `ts-bare-catch`, `ts-import-type`, `ts-no-deprecated-leftovers`, `ts-no-dynamic-import`, `ts-no-inline-cast-access`, `ts-no-local-is-record`, `ts-no-return-type`, `ts-no-test-timers`, `ts-no-tiny-functions`, `ts-promise-with-resolvers`, `ts-redundant-clear-guard`, `ts-set-map` |
| Python | `py-bare-except`, `py-mutable-default`, `py-shell-injection`, `py-eval-exec`, `py-utcnow`, `py-typing-generics`, `py-star-import`, `py-yaml-load` |
| Shell  | `sh-curl-pipe-shell`, `sh-unquoted-expansion`, `sh-eval-variable`. These also watch the `Bash` tool, so they see a command the model runs as well as a script it writes. |
| Java, Kotlin | `java-printstacktrace`, `java-empty-catch`, `java-new-random`, `java-simpledateformat`, `java-runtime-exec` |
| C#     | `cs-async-void`, `cs-blocking-async`, `cs-empty-catch` |
| PHP    | `php-mysql-legacy`, `php-eval-system`, `php-unserialize`, `php-extract-import` |
| C, C++ | `c-unsafe-string`, `c-scanf-unbounded`, `c-system-call`, `cpp-raw-owning-new` |
| Any language | `no-secrets`, `sql-parameterize`, `web-no-localstorage` |

Twenty-seven of them are adapted from the `oh-my-pi` project under the MIT
License; the notice is in `crates/core/assets/rules/NOTICE.md`. Five of those
matched a syntax tree upstream. A regular expression cannot tie one placeholder
to another, so each of their conditions was rewritten to be narrow without
that, and each file says where its version is looser. The rest are this
project's own.

A condition is a regular expression and nothing more, so a rule can only state
what a pattern can see. It cannot say "call X without argument Y", because the
`regex` crate has no lookaround. Where that mattered, the rule is written the
other way round: `py-yaml-load` names the unsafe function rather than the
missing argument.

### Seeing what a rule would do

```bash
mikmik rules list                                    # every rule this directory loads
mikmik rules test Edit src/a.rs 'let x = y.unwrap();' # would anything fire?
mikmik rules test Bash '' 'git add -A'
mikmik rules test text 'that is probably fine'       # rules on prose take no file
mikmik rules test thinking 'I should be okay here'
```

`test` runs the real matcher, so what it prints is what a session would do. The
text may also come from stdin. A rule that never fires and a rule that fires on
everything look the same from the outside, and this is how to tell them apart
before shipping the file.

### Lifting a rule out of an AGENTS.md

Rules you already wrote as bullets in an `AGENTS.md` sit in the prompt on every
turn, whether or not they are about to be broken. `extract` proposes a rule file
for each bullet that a pattern can match:

```bash
mikmik rules extract                    # print the proposals, write nothing
mikmik rules extract --write            # write all of them
mikmik rules extract --write <name>...  # write the named ones
```

A bullet qualifies when it carries an inline code span, because that span is the
only part a regular expression can match on. Every span of three characters or
more becomes a condition, so a bullet naming two forbidden calls matches on
both. A bullet of pure prose is left where it is.

**The condition and the scope are guesses.** The command reads no code and
consults no model: it escapes the spans it found, and it names `tool:Bash` only
when a span starts with a command it recognises. Everything else gets
`scope: "tool"`, which watches every tool, and you narrow it. A bullet that
says "never" about a shell command is proposed as `on_match: block`, because a
command is already run by the time a reminder could arrive.

Read the proposals, then write the ones you want. A file that already exists is
skipped, never overwritten. A bullet from your own `AGENTS.md` proposes a file
under your config root, and a bullet from a repository's proposes one under
`.mikmik/rules/`, so a rule you set globally does not land in one project.

The bullet stays in the memory file. Remove it there once the rule is written,
or it costs context on every turn as well as speaking when it is broken.

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

| Key                 | Type    | Default | Description                                             |
|---------------------|---------|---------|---------------------------------------------------------|
| `autoMemoryEnabled` | boolean | false   | Keep the directory and show it to the model.            |
| `memoryModel`       | string  | unset   | Model the memory jobs run on. Unset uses the session's. |
| `memoryBackend`     | string  | unset   | Storage engine: unset/`"file"` for `.md` files, `"sqlite"` for a database. |

Both work at the top level and inside the `config` block:

```json
{ "version": 1, "autoMemoryEnabled": true }
```

```json
{ "version": 1, "config": { "autoMemoryEnabled": true } }
```

Inside `config`, `autoMemoryEnabled` is an alias for `auto_memory_enabled`,
which is what gets written back out. `/settings` writes both keys.

`memoryModel` covers both background jobs: the extraction that writes
`session-notes.md` at the end of a turn, and the consolidation sub-agent. Nobody
waits on either, so a cheaper model than the session's usually fits. It accepts
a bare model ID or `"provider/model"`, and the account comes with the model
rather than staying on the session's. Set it from `/settings` → **Memory
model**; a project's `settings.json` cannot set it, because it names a model
that runs on the user's account without being asked for.

`memoryBackend` picks the storage engine. Unset or `"file"` keeps the `.md`-file
store described below. `"sqlite"` keeps the memories in one `memory.db` inside
the same directory, an FTS5 database that `Learn`, `Retain`, `Memory` and the
consolidation dream all read and write through instead of files. The two engines
migrate both ways: the first time a project opens on sqlite it imports the
existing `.md` files, and switching back to files exports the database's lessons
and facts to `learned.md` and `facts.md`, so a project can move its memories in
either direction without losing them. Like `memoryModel`, a project's
`settings.json` cannot set it; it selects where the user's own memories live.

`/memories` reads, measures and clears the directory; see
[Commands](commands.md#memories).

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

`type` is one of `user`, `feedback`, `project` or `reference`. A search scores
`name` highest, then `description`, then the filename, then the body, so a file
without frontmatter is still findable through its text but ranks below one that
names the topic.

Session extraction writes `session-notes.md` there on its own, the
[`Learn`](tools.md#learn) tool writes `learned.md`, and the
[`Retain`](tools.md#retain) tool writes `facts.md`: one durable lesson or fact
per entry, newest first, deduplicated, 100 entries at most. On the sqlite engine
the same writes become rows in `memory.db` instead.

### What the model sees

While the feature is on, the system prompt carries a `<memory>` block naming
the directory, the `MEMORY.md` index (capped at 200 lines and 25 KB), and a
one-line manifest entry per file with its type, description and age. Bodies are
not loaded; the model reads one with the [`Memory`](tools.md#memory) tool. The
block also tells the model to record a single lesson with
[`Learn`](tools.md#learn), a single fact with [`Retain`](tools.md#retain), and a
whole document with `Write`. These tools are offered only while the feature is
on.

Each body the tool returns is prefixed with a staleness note when the file is
more than a day old, because a memory is a point-in-time observation and a
file:line citation in it may no longer hold.

### Credentials never reach a memory file

A memory file is read back into the system prompt of every later session in the
same project. A credential stored in one is not written once; it is re-sent on
every request, to whichever provider that session uses, until somebody opens the
file. Two checks stop that, and neither has a setting to turn it off.

Session extraction masks what it writes. Anything the extractor produces that
looks like a credential is replaced with `[REDACTED]` before `session-notes.md`
is written, and the run logs which class fired at `warn` level. The sentence
around the value survives, so "the deploy token is `[REDACTED]`" still records
that a deploy token exists.

`Write`, `Edit` and `BatchEdit` refuse instead. A write into the memory
directory whose new content carries a credential is rejected with a message
naming the class, and no bytes are written. `BatchEdit` aborts the whole batch,
including its clean edits. The check looks only at what the call adds, so a
memory file that already holds a credential stays editable and the edit that
removes it goes through. Everywhere else on disk these tools are unchanged.

Recognised classes: Anthropic, OpenAI, GitHub, GitLab, npm, Slack, Google, AWS,
Hugging Face, JWTs, PEM private-key blocks, and any `token: <long value>`-style
assignment. Every rule anchors on a vendor prefix or on an assignment, so an
ordinary identifier such as `keyboard_shortcuts_v2` is left alone.

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
| `MIKMIK_GOALS`        | Set to `0` to disable the goal system (`/goal`, `/guided-goal` and the `Goal` tool). |
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
  "max_concurrent_executors": 3,
  "executor_isolation": true,
  "total_budget_usd": 5.00,
  "preset_name": "anthropic-tiered"
}
```

| Key                        | Type            | Description                                                                        |
|----------------------------|-----------------|------------------------------------------------------------------------------------|
| `enabled`                  | boolean         | Whether managed agents are on.                                                     |
| `manager_model`            | string          | Model the session itself runs on, in `provider/model` form.                        |
| `executor_model`           | string          | Model every sub-agent runs on.                                                     |
| `executor_max_turns`       | integer         | Turn limit for one executor. Default: 10.                                          |
| `max_concurrent_executors` | integer         | How many executors may run at once. Default: 4.                                    |
| `executor_isolation`       | boolean         | Give each executor its own git worktree. Default: false.                           |
| `total_budget_usd`         | number \| null  | One pool the manager and its executors draw from. Null means no cap.               |
| `preset_name`              | string \| null  | Name of the preset the configuration came from, for display only.                  |

`total_budget_usd` is a single pool, not a split: the manager and every executor
draw from the same figure, and the session stops when it is spent. The
`--max-budget-usd` flag overrides it for one run.

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
