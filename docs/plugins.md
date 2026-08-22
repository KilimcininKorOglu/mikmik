# Plugins

MikMik's plugin system lets you extend the agent with additional slash commands, agents, skills, MCP servers, LSP servers, and lifecycle hooks — all packaged in a single directory.

---

## Plugin Discovery

Plugins are loaded from two directories: `~/.config/mikmik/plugins/` for every project, and `<project>/.mikmik/plugins/` for one. Each subdirectory that carries a valid manifest is treated as a plugin. The manifest is looked for at three paths, in order:

| Path                          | Notes                                                        |
|-------------------------------|--------------------------------------------------------------|
| `plugin.json`                 | Checked first, so it wins when a plugin carries more than one |
| `plugin.toml`                 | Same fields, TOML syntax                                      |
| `.claude-plugin/plugin.json`  | Where a plugin written for Claude Code keeps it               |

The plugin's root stays the directory itself in every case: `commands/`, `agents/`, `skills/`, `hooks/` and `output-styles/` are resolved against it, not against `.claude-plugin/`.

```
~/.config/mikmik/plugins/
├── my-plugin/
│   ├── plugin.toml          <- manifest
│   ├── commands/            <- *.md slash command definitions
│   ├── agents/              <- *.md agent definitions
│   ├── skills/              <- subdirectories with SKILL.md
│   ├── hooks/               <- hooks.json (optional)
│   └── output-styles/       <- *.md or *.json style definitions
└── another-plugin/
    └── plugin.json
```

Both `plugin.toml` (TOML format) and `plugin.json` (JSON format) are supported. The loader normalises camelCase and snake_case field names, so manifests written in either convention are accepted.

---

## Plugin Manifest Format

### plugin.toml

```toml
name        = "my-plugin"
version     = "1.0.0"
description = "Adds custom commands and hooks for my workflow"
license     = "MIT"
keywords    = ["formatting", "git"]

[author]
name  = "Your Name"
email = "you@example.com"
url   = "https://example.com"

homepage   = "https://example.com/my-plugin"
repository = "https://github.com/you/my-plugin"

# Extra command files beyond the commands/ directory
commands = ["./extra/review.md"]

# Extra agent markdown files beyond the agents/ directory
agents = ["./agents/reviewer.md"]

# Extra skill directories beyond the skills/ directory
skills = ["./extra-skills/"]

# Inline MCP server definitions
[[mcp_servers]]
name    = "my-tool-server"
command = "npx"
args    = ["-y", "my-mcp-server"]
type    = "stdio"

[mcp_servers.env]
API_TOKEN = "${MY_SERVICE_TOKEN}"

# Inline LSP server definitions
[[lsp_servers]]
name    = "pyright"
command = "pyright-langserver"
args    = ["--stdio"]
transport = "stdio"
restart_on_crash = true

[lsp_servers.extension_to_language]
".py" = "python"

# User-configurable options (edited in /settings)
[user_config.api_token]
type        = "string"
title       = "API Token"
description = "Token for the upstream service"
required    = true
sensitive   = true

[user_config.max_results]
type        = "number"
title       = "Max Results"
description = "Maximum items to return per query"
default     = 20

# Capability grants (omit to allow all)
capabilities = ["read_files", "network", "shell"]

# Marketplace identifier
marketplace_id = "you/my-plugin"
```

### plugin.json

```json
{
  "name": "my-plugin",
  "version": "1.0.0",
  "description": "Adds custom commands and hooks for my workflow",
  "author": {
    "name": "Your Name",
    "email": "you@example.com"
  },
  "homepage": "https://example.com/my-plugin",
  "repository": "https://github.com/you/my-plugin",
  "license": "MIT",
  "keywords": ["formatting", "git"],
  "commands": ["./extra/review.md"],
  "agents": ["./agents/reviewer.md"],
  "skills": ["./extra-skills/"],
  "mcpServers": {
    "my-tool-server": {
      "command": "npx",
      "args": ["-y", "my-mcp-server"],
      "type": "stdio",
      "env": {
        "API_TOKEN": "${MY_SERVICE_TOKEN}"
      }
    }
  },
  "userConfig": {
    "api_token": {
      "type": "string",
      "title": "API Token",
      "description": "Token for the upstream service",
      "required": true,
      "sensitive": true
    },
    "max_results": {
      "type": "number",
      "title": "Max Results",
      "description": "Maximum items to return per query",
      "default": 20
    }
  },
  "capabilities": ["read_files", "network", "shell"],
  "marketplaceId": "you/my-plugin"
}
```

---

## Manifest Fields Reference

### Required

| Field  | Type   | Description                                                            |
|--------|--------|------------------------------------------------------------------------|
| `name` | string | Plugin name. Must be non-empty and contain no spaces (use kebab-case). |

### Metadata (optional)

| Field            | Type             | Description                                                               |
|------------------|------------------|---------------------------------------------------------------------------|
| `version`        | string           | Plugin version string                                                     |
| `description`    | string           | Human-readable description                                                |
| `author`         | object           | `name`, `email` (optional), `url` (optional)                              |
| `homepage`       | string           | URL for the plugin's home page                                            |
| `repository`     | string           | URL for the source repository                                             |
| `license`        | string           | SPDX license identifier (e.g. `"MIT"`)                                    |
| `keywords`       | array of strings | Tags used in marketplace search                                           |
| `marketplace_id` | string           | Unique identifier in the plugin marketplace (e.g. `"author/plugin-name"`) |

### Content Declarations

| Field           | Type             | Description                                                                                                                  |
|-----------------|------------------|------------------------------------------------------------------------------------------------------------------------------|
| `commands`      | array of strings | Paths to extra slash command `.md` files or directories, relative to the plugin root. Supplements the `commands/` directory. |
| `agents`        | array of strings | Paths to extra agent `.md` files. Supplements the `agents/` directory.                                                       |
| `skills`        | array of strings | Paths to extra skill directories (each must contain a `SKILL.md`). Supplements the `skills/` directory.                      |
| `output_styles` | array of strings | Paths to extra output style definitions.                                                                                     |

### commands

Each `*.md` file under the plugin's `commands/` directory becomes a slash command named `<plugin>:<file stem>`, so `commands/greet.md` in the `toolkit` plugin runs as `/toolkit:greet`. The name is always namespaced, which is what keeps two plugins from claiming the same command.

The file is expanded like a skill: the YAML frontmatter is dropped, `$ARGUMENTS`, `$1` and `$2` take what the user typed, and a body with no placeholder gets the arguments appended on a trailing line. The command's description comes from the frontmatter's `description:` when it has one, and from the first line of the body otherwise.

Both the command and its description appear in the prompt typeahead, in the command palette and in the `?` help overlay.

### skills

Each subdirectory of the plugin's `skills/` directory that holds a `SKILL.md` becomes a skill named after the directory, unless the file's frontmatter sets `name:`. The session adds these directories to skill discovery at startup, so `/skills` lists them and the skill runs as a slash command under its own name.

A skill is not namespaced, so a name can collide. The slash router answers with a built-in command first and a command from `settings.json` second, which means a skill that shares either name never runs; `/skills` marks such an entry with the winner. Pick a name no built-in uses.

Skill directories named in the manifest's `skills` array are treated the same way.

### agents

Each `*.md` file in the plugin's `agents/` directory becomes an agent definition. `/agents` lists it with `plugin:<name>` as its source, and it is appended to a sub-agent's system prompt as a `## Agent: <file stem>` section, so a delegated task knows which specialised agents the installed plugins describe.

A definition whose name is already taken by a project or user agent is listed with the source that shadows it.

### mcp_servers

An array of inline MCP server definitions. Each entry is identical to an `McpServerConfig` (see the MCP documentation). In `plugin.json` the field can also be written as `"mcpServers"` with an object mapping (the loader converts it to the array form automatically).

These servers connect at startup along with the ones declared in `settings.json`. A server carries the scope of the plugin that declared it: one from `~/.config/mikmik/plugins/` launches directly, while one from `<project>/.mikmik/plugins/` is project-scoped and waits for the same approval as a project-defined server, because it arrives with a cloned repository. Approve it in the TUI prompt, or pass `--trust-project-mcp` (or set `trustProjectMcpServers`) for headless runs.

### lsp_servers

An array of LSP server definitions for language-aware editing support. They join the servers from `settings.json` at startup, and the LSP tool routes a file to one of them by `file_patterns` and `extension_to_language`. The field meanings match the ones in [tools.md#lsp](tools.md#lsp).

The LSP manager speaks stdio and owns the server's lifecycle, so `transport`, `workspace_folder`, `shutdown_timeout`, `restart_on_crash` and `max_restarts` are read but not applied. A `transport` other than `"stdio"` is reported in the log and the server is started over stdio anyway.

| Field                    | Type   | Description                                                            |
|--------------------------|--------|------------------------------------------------------------------------|
| `name`                   | string | Server identifier                                                      |
| `command`                | string | Executable to launch                                                   |
| `args`                   | array  | Command-line arguments                                                 |
| `file_patterns`          | array  | `*.ext`, or a whole file name such as `Dockerfile`                     |
| `extension_to_language`  | object | Map of file extension → LSP language ID                                |
| `language_id`            | string | One language ID for every file this server handles                     |
| `root_markers`           | array  | Files or directories that mark a project this server serves            |
| `is_linter`              | bool   | The server only reports problems; it answers no navigation request     |
| `disabled`               | bool   | Switch the server off without removing the entry                       |
| `initialization_options` | object | Sent once in the `initialize` handshake                                |
| `settings`               | object | Sent with `workspace/didChangeConfiguration` after the handshake       |
| `transport`              | string | `"stdio"` (default)                                                    |
| `env`                    | object | Extra environment variables                                            |
| `workspace_folder`       | string | Optional workspace path                                                |
| `startup_timeout`        | number | Milliseconds allowed for the handshake                                 |
| `shutdown_timeout`       | number | Milliseconds to wait for clean shutdown                                |
| `restart_on_crash`       | bool   | Automatically restart on unexpected exit                               |
| `max_restarts`           | number | Maximum restart attempts                                               |

### output_styles (directory)

Each `*.md` or `*.json` file in the plugin's `output-styles/` directory is registered at startup under its `name`. Select one with `/output-style <name>`, and its prompt is injected into the system prompt exactly like a built-in or user style. `/output-style` lists them, and the settings screen shows them under **Available**.

### hooks

Either a path string pointing to a `hooks.json` file inside the plugin directory, or an inline hooks configuration object (see the Hooks section below).

### user_config

A map of option keys to `PluginUserConfigOption` objects. Each option becomes a row in `/settings`, listed as `<plugin>: <title>`:

| Field         | Type   | Description                                                              |
|---------------|--------|--------------------------------------------------------------------------|
| `type`        | enum   | Value type: `"string"`, `"number"`, `"boolean"`, `"directory"`, `"file"` |
| `title`       | string | Display label                                                            |
| `description` | string | Explanation of the option                                                |
| `required`    | bool   | Whether the user must supply a value                                     |
| `default`     | any    | Value shown until the user sets one (optional)                           |
| `sensitive`   | bool   | Adds a note that the value is stored in `settings.json` in the clear     |

A `boolean` option is a toggle; every other type opens an edit prompt. Confirming an empty value clears the option rather than storing a blank.

**Where the values go.** `settings.json`, under `pluginConfig`, keyed by plugin name:

```json
{
  "pluginConfig": {
    "my-plugin": { "apiToken": "…", "maxResults": 20, "verbose": true }
  }
}
```

**How the plugin reads them.** Every hook and shell command the plugin runs gets them in its environment:

| Variable                        | Value                                                            |
|---------------------------------|------------------------------------------------------------------|
| `CLAUDE_PLUGIN_CONFIG`          | The whole object as JSON                                          |
| `CLAUDE_PLUGIN_CONFIG_<OPTION>` | One option, upper-cased, non-alphanumerics replaced by `_`        |

A string arrives unquoted; every other type arrives in its JSON form, so a boolean reads as `true` rather than `"true"`. A plugin with nothing configured gets neither variable, so a shell script can test for the variable to detect that case.

The type is taken from what was typed, not from the manifest: `true`/`false` store a boolean, a number stores a number, anything else stores a string. A value set while the session is running applies to the next hook that runs, with no reload needed, because the environment is built per invocation.

Options are read from the plugins the session has loaded, so a plugin installed mid-session shows its options in `/settings` after `/plugin reload`.

### capabilities

An optional array of capability category strings. When present, the plugin is restricted to only those categories. Omit the field entirely to allow all capabilities (backwards compatibility behaviour). An empty array (`[]`) grants no capabilities.

Known categories: `"read_files"`, `"write_files"`, `"network"`, `"shell"`, `"browser"`, `"mcp"`.

The check runs when a plugin slash command executes. A plugin's MCP servers, hooks, skills, agents and output styles are not filtered by it, so treat the list as a guard on the plugin's own commands rather than a sandbox around everything it ships.

---

## Hook Events

Plugins can run shell commands in response to lifecycle events. Hooks receive a JSON payload on stdin describing the event.

### Available Events

Every event below is raised by the running code, with one exception:
`TeammateIdle` is accepted by the manifest parser and stored, but nothing raises
it yet. Declaring it is harmless and does nothing.

`PreToolUse` and `PostToolUse` run for every tool call, on every provider. A
`PreToolUse` hook marked `blocking` stops the call when it exits non-zero, and
the model is told the plugin blocked it.

| Event                | When it fires                                    |
|----------------------|--------------------------------------------------|
| `PreToolUse`         | Before any tool is executed                      |
| `PostToolUse`        | After a tool returns its result                  |
| `PostToolUseFailure` | After a tool call throws an error                |
| `PermissionDenied`   | When a permission request is rejected            |
| `PermissionRequest`  | When a permission is requested (before decision) |
| `Notification`       | General notification from the agent              |
| `UserPromptSubmit`   | When the user submits a prompt                   |
| `SessionStart`       | At the beginning of a session                    |
| `SessionEnd`         | At clean session shutdown                        |
| `Stop`               | When the model finishes its turn                 |
| `StopFailure`        | When the stop sequence fails                     |
| `SubagentStart`      | When a sub-agent is spawned                      |
| `SubagentStop`       | When a sub-agent finishes                        |
| `PreCompact`         | Before context compaction                        |
| `PostCompact`        | After context compaction                         |
| `Setup`              | During plugin setup phase                        |
| `TeammateIdle`       | When a teammate agent becomes idle               |
| `TaskCreated`        | When a task is created                           |
| `TaskCompleted`      | When a task finishes                             |
| `Elicitation`        | When the model requests clarification            |
| `ElicitationResult`  | When elicitation receives a response             |
| `ConfigChange`       | When configuration is modified                   |
| `WorktreeCreate`     | When a git worktree is created                   |
| `WorktreeRemove`     | When a git worktree is removed                   |
| `InstructionsLoaded` | After the session's instruction files are loaded |
| `CwdChanged`         | When the working directory changes               |
| `FileChanged`        | When a tool writes a file                        |

### HookEntry Fields

Each hook entry in a hooks configuration:

| Field      | Type   | Description                                                                                                                   |
|------------|--------|-------------------------------------------------------------------------------------------------------------------------------|
| `command`    | string | Shell command to run. Receives event JSON on stdin.                                                                           |
| `matcher`    | string | Optional tool-name filter. Supports the `*` wildcard (e.g. `"Write*"`). Only relevant for `PreToolUse` / `PostToolUse`.       |
| `blocking`   | bool   | If `true`, a non-zero exit code blocks the operation. Non-blocking hooks (default) only log a warning on failure.             |
| `timeout_ms` | number | How long the command may run before it is killed. Default 30000. `timeout` is accepted as an alias.                           |

A blocking hook that reaches its time limit blocks the operation: a hook that never answered cannot be read as approval.

### Hooks Configuration Format

Hooks can be defined inline in the manifest or in a separate `hooks/hooks.json` file. Both the flat form and the wrapped form are accepted:

**Flat form:**

```json
{
  "PreToolUse": [
    {
      "matcher": "Bash",
      "hooks": [
        {
          "command": "echo \"About to run Bash tool\" >&2",
          "blocking": false
        }
      ]
    }
  ],
  "Stop": [
    {
      "hooks": [
        {
          "command": "notify-send 'MikMik finished'",
          "blocking": false
        }
      ]
    }
  ]
}
```

**Wrapped form (with description):**

```json
{
  "description": "Audit and notification hooks",
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit",
        "hooks": [
          {
            "command": "python3 lint_check.py",
            "blocking": true
          }
        ]
      }
    ]
  }
}
```

When a blocking hook exits non-zero, MikMik denies the operation and reports the hook's stderr as the reason.

**Environment variables available to hook processes:**

| Variable                        | Value                                                     |
|---------------------------------|-----------------------------------------------------------|
| `CLAUDE_PLUGIN_ROOT`            | Absolute path to the plugin directory                     |
| `CLAUDE_PLUGIN_NAME`            | Plugin name from the manifest                             |
| `CLAUDE_PLUGIN_CONFIG`          | The plugin's `userConfig` values as JSON, when any are set |
| `CLAUDE_PLUGIN_CONFIG_<OPTION>` | One `userConfig` value (see [user_config](#user_config))  |

The tool's name, input and result are not in the environment. Read them from the
event JSON on stdin, which carries `tool_name`, `tool_input`, `tool_output` and
`is_error`.

---

## Managing Plugins with /plugin

The `/plugin` slash command manages plugins from within an interactive session:

```
/plugin                      — list all installed plugins
/plugin list                 — list all installed plugins with status
/plugin info <name>          — show detailed info about a plugin
/plugin enable <name>        — enable a plugin (persisted to settings)
/plugin disable <name>       — disable a plugin (persisted to settings)
/plugin install <source>     — install from a directory, an owner/repo, or a git URL
/plugin update <name>        — pull the latest commit for a plugin installed from git
/plugin remove <name>        — delete an installed plugin's directory
/plugin reload               — reread the plugin directories and apply what changed
```

See [Installing from a Repository](#installing-from-a-repository) for the sources `install` accepts.

`enable` and `disable` write to `settings.json`. Run `/plugin reload` afterwards to apply the change to the running session.

### /reload-plugins

```
/reload-plugins
```

Rereads the user and project plugin directories, re-reads every manifest, and applies the result to the running session:

| Contribution      | On reload                                                                 |
|-------------------|---------------------------------------------------------------------------|
| Hooks             | The hook registry is replaced. A hook a plugin added starts firing, one from a removed or disabled plugin stops. |
| Slash commands    | The typeahead, the command palette and the help overlay list the new set.  |
| Skills            | A plugin's `skills/` directory joins or leaves skill discovery.            |
| Agents            | Sub-agent prompts read the new `agents/` definitions.                      |
| Output styles     | A new style is registered. A style already registered under the same name stays as it was. |
| Language servers  | A server a plugin added joins the config; one it dropped leaves.           |
| MCP servers       | A server a plugin added joins the config; one it dropped leaves. The MCP runtime reconnects only when this set actually changed, and a newly added server passes the same trust prompt as one declared in `settings.json`. |

The command reports one line: how much is loaded now, plus which plugins were added, removed or updated relative to the set the session was running.

Two things a reload does not undo. An output style that was already registered under a name keeps its definition for the rest of the session, and a plugin's `settings.json`-independent side effects (anything its `Setup` hook already did) stay done.

---

## Installing from a Repository

`/plugin install <source>` reads a plugin from a directory on this machine or from a git repository:

```
/plugin install ./my-plugin                          local directory
/plugin install ~/work/my-plugin                     ~ is expanded
/plugin install acme/my-plugin                       github.com/acme/my-plugin
/plugin install acme/my-plugin@v1.2.0                a branch or tag
/plugin install https://gitlab.com/acme/my-plugin.git
/plugin install git@github.com:acme/my-plugin.git
/plugin install file:///srv/repos/my-plugin.git
```

A path that exists on disk wins over the `owner/repo` reading, so a local directory named like a repository still installs from disk.

**What the repository has to contain.** Either a manifest at its root (see [Plugin Discovery](#plugin-discovery)), in which case the repository is one plugin, or a `.claude-plugin/marketplace.json` listing several:

```json
{
  "name": "acme",
  "plugins": [
    { "name": "one", "source": "./plugins/one" },
    { "name": "two", "source": "./plugins/two" }
  ]
}
```

Every listed entry whose `source` is a path inside the repository is installed. An entry that names another repository is skipped rather than followed, and so is one whose path leaves the clone.

**Where it lands.** Each plugin is installed as `<mikmik home>/plugins/<name>`, taking the name from its manifest rather than from the repository. The install refuses rather than overwriting when that directory already exists, and a repository holding several plugins is checked in full before anything moves, so a collision leaves nothing half-installed. The clone keeps its `.git` directory, which is what `/plugin update` needs.

Run `/plugin reload` afterwards to use the plugin in the running session.

### Updating and removing

```
/plugin update <name>    pull the latest commit (git installs only)
/plugin remove <name>    delete the installed directory
```

`update` is a fast-forward pull and reports the commit range it moved through, or that the plugin was already current. A plugin installed from a local directory has no remote to pull from and says so.

### What is not installed

`http://`, `git://` and `ext::` sources are refused: the first two fetch code over a connection nobody authenticated, and `ext::` makes git run a command the URL chooses. There is no plugin registry service behind `/plugin install`; a name that is not a path, an `owner/repo` or a URL is an error rather than a registry lookup. The `marketplace_id` manifest field is metadata and nothing reads it.

---

## Example: A Complete Plugin

```toml
# ~/.config/mikmik/plugins/code-quality/plugin.toml

name        = "code-quality"
version     = "0.3.1"
description = "Runs linters and formatters as blocking pre-tool hooks"
license     = "MIT"
keywords    = ["lint", "format", "quality"]

[author]
name = "Dev Team"

capabilities = ["shell", "read_files"]

[user_config.fail_on_warning]
type        = "boolean"
title       = "Fail on Warnings"
description = "Treat linter warnings as errors"
default     = false
```

```json
// ~/.config/mikmik/plugins/code-quality/hooks/hooks.json
{
  "description": "Lint and format on file edits",
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit",
        "hooks": [
          {
            "command": "jq -r .tool_input.file_path | xargs -r eslint --fix 2>&1 || true",
            "blocking": false
          }
        ]
      },
      {
        "matcher": "Write",
        "hooks": [
          {
            "command": "jq -r .tool_input.file_path | xargs -r prettier --write 2>&1 || true",
            "blocking": false
          }
        ]
      }
    ]
  }
}
```
