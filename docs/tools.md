# MikMik Tools Reference

This document is the complete reference for every tool available to the MikMik agent. Tools are the mechanism by which the model interacts with the outside world — reading files, running commands, searching the web, and coordinating sub-agents.

---

## Table of Contents

1. [Tool System Overview](#tool-system-overview)
2. [Permission System](#permission-system)
3. [File Tools](#file-tools)
4. [Shell Execution Tools](#shell-execution-tools)
5. [Search Tools](#search-tools)
6. [Web Tools](#web-tools)
7. [Task Management Tools](#task-management-tools)
8. [MCP Integration Tools](#mcp-integration-tools)
9. [Agent Tools](#agent-tools)
10. [Notebook Tools](#notebook-tools)
11. [Planning Tools](#planning-tools)
12. [Worktree Tools](#worktree-tools)
13. [Utility Tools](#utility-tools)
14. [Cron Tools](#cron-tools)
15. [Code Intelligence Tools](#code-intelligence-tools)
16. [Advanced Tools](#advanced-tools)
17. [Tool Framework Internals](#tool-framework-internals)

---

## Tool System Overview

Every tool in MikMik implements the `Tool` trait. It defines:

| Member               | Purpose                                                                   |
|----------------------|---------------------------------------------------------------------------|
| `name()`             | The name the model calls                                                  |
| `description()`      | The one-line description the model reads                                  |
| `permission_level()` | The level the tool needs; see the table below                             |
| `input_schema()`     | A JSON Schema describing the parameters                                   |
| `execute()`          | Performs the operation and returns a `ToolResult`                         |
| `self_gates()`       | `true` when the tool prompts for permission itself, so the central gate does not prompt twice |
| `advanced()`         | Marks a rarely used tool as a candidate for on-demand disclosure          |

A `ToolResult` carries the text sent back to the model, an error flag, and optional structured metadata the TUI uses to render diffs.

Tools are loaded at session start. The model receives the names, descriptions and schemas, and chooses which to call. Every call goes through permission resolution first: a tool that forgets to check permission is still caught by the central backstop whenever its level is a gated one.

### Workspace-root paths

Every path-based tool accepts three forms: an absolute path, a path relative to the working directory, and a named workspace-root path.

The working directory is always `&main`. Directories added with `--add-dir`, `additional_dirs` or `workspace_paths` take a name from their last path component (`&docs`, `&_ai-engine`, `&my-project-api`), with a counter appended when two of them share a name. Write `&<root-name>/<relative-path>` for a file under that root, or `&<root-name>` on its own for the root directory. A root name that does not exist is an error listing the known roots, not a path.

`Glob` and `Grep` search the working directory when `path` is omitted; pass `path=&<root-name>` to search a different root. See [`--add-dir`](advanced.md#--add-dir) for how the names are derived.

---

## Permission System

### Permission Levels

Each tool declares one permission level:

| Level         | Description                            | Examples                            |
|---------------|----------------------------------------|-------------------------------------|
| **None**      | No external effects; purely passive    | `Sleep`                             |
| **ReadOnly**  | Reads data; no writes or execution     | `Read`, `Glob`, `WebFetch`          |
| **Write**     | Creates or modifies data               | `Write`, `Edit`, `Config`           |
| **Execute**   | Runs code or spawns processes          | `Bash`, `TaskCreate`, `SendMessage` |
| **Dangerous** | Broad system access; high blast radius | `computer`                          |
| **Forbidden** | Never executed, in any mode            | A `Critical`-risk bash command      |

`None` and `ReadOnly` are not gated at all: they never reach the permission manager, so no rule applies to them.

### Permission Modes

| Mode                | Behavior                                                        |
|---------------------|-----------------------------------------------------------------|
| `default`           | Prompts the user for any tool that isn't pre-approved           |
| `plan`              | All write/execute tools are blocked; read-only tools run freely |
| `acceptEdits`       | File edits are auto-approved; shell execution still prompts     |
| `bypassPermissions` | All tools run without prompting (headless/CI use)               |

`--dangerously-skip-permissions` selects `bypassPermissions`. It opens a warning gate first, in the terminal and in the web client alike. Use it only in trusted, sandboxed environments.

### Permission Rules

Rules are stored per-project and per-user. A rule specifies:

- **Tool name** (or glob pattern matching tool names)
- **Path pattern** (optional, for file tools)
- **Decision**: `allow` or `deny`

Rules are evaluated in order; the first match wins. Manage rules with `/permissions`.

### Change recording

Every write records the previous content before overwriting it, which is what `/undo` and `/rewind` replay. A configured formatter runs afterwards, and the `FileChanged` hook fires with the path and whether the file was created or overwritten.

---

## File Tools

### Read

**Permission level:** ReadOnly

Read the contents of a file from the local filesystem. Returns file contents as a string. Supports optional line range to read a subset of a large file.

| Parameter   | Type    | Required | Description                     |
|-------------|---------|----------|---------------------------------|
| `file_path` | string  | yes      | Absolute, working-directory-relative, or `&root-name/relative` path|
| `offset`    | integer | no       | First line to read (1-indexed)  |
| `limit`     | integer | no       | Maximum number of lines to read |

Supports reading: text files, images (PNG, JPG, GIF, WEBP — returned as base64), PDF files (text extraction), and Jupyter notebooks.

---

### Write

**Permission level:** Write

Write content to a file. Creates the file and any missing parent directories. Overwrites existing files entirely.

| Parameter   | Type   | Required | Description            |
|-------------|--------|----------|------------------------|
| `file_path` | string | yes      | Absolute, working-directory-relative, or `&root-name/relative` path |
| `content`   | string | yes      | Full file content      |

The previous content is stored, so `/undo` can put it back.

When a language server serves the file, the result also carries the problems the
write introduced, and the file can be formatted by that server first. Both are
settings; see [configuration.md#language-servers](configuration.md#language-servers).

---

### Edit

**Permission level:** Write

Perform an exact string replacement within an existing file. Fails if `old_string` is not found or is not unique. Prefer this tool over `Write` when making targeted edits, as it only transmits the diff rather than the entire file.

| Parameter     | Type    | Required | Description                              |
|---------------|---------|----------|------------------------------------------|
| `file_path`   | string  | yes      | Absolute, working-directory-relative, or `&root-name/relative` path|
| `old_string`  | string  | yes      | Exact text to replace                    |
| `new_string`  | string  | yes      | Replacement text                         |
| `replace_all` | boolean | no       | Replace all occurrences (default: false) |

Whitespace and indentation must match exactly.

When a language server serves the file, the result also carries the problems the
write introduced, and the file can be formatted by that server first. Both are
settings; see [configuration.md#language-servers](configuration.md#language-servers).

---

### BatchEdit

**Permission level:** Write

Apply multiple `Edit`-style edits in a single tool call. More efficient than calling `Edit` repeatedly when making many changes to the same file or across multiple files.

| Parameter | Type  | Required | Description                                                         |
|-----------|-------|----------|---------------------------------------------------------------------|
| `edits`   | array | yes      | Array of `{file_path, old_string, new_string, replace_all}` objects |

Edits within the same file are applied in order. If any individual edit fails (string not found, not unique), the batch is aborted and no changes are written.

---

### ApplyPatch

**Permission level:** Write

Apply a unified diff patch to one or more files. Accepts standard `diff -u` / `git diff` format patches.

| Parameter | Type   | Required | Description             |
|-----------|--------|----------|-------------------------|
| `patch`   | string | yes      | Unified diff patch text |

Useful when the model needs to express changes in diff format rather than as string replacements.

---

## Shell Execution Tools

### Bash

**Permission level:** Execute

Execute a shell command in a real terminal (PTY). One shell serves the whole session, so `cd` and `export` outlive the call and the working directory persists between commands. The PTY also lets interactive programs and terminal-aware tools (npm, cargo, git, pytest) behave as they would at a keyboard; colour codes are stripped for readability.

| Parameter           | Type    | Required | Description                                              |
|---------------------|---------|----------|----------------------------------------------------------|
| `command`           | string  | yes      | Shell command to execute                                 |
| `description`       | string  | no       | Description shown in the TUI and in the permission prompt|
| `timeout`           | integer | no       | Timeout in milliseconds (default 120000, max 600000)     |
| `run_in_background` | boolean | no       | Run asynchronously; result delivered via notification    |

Output (stdout + stderr) is returned as a string, and long output is truncated.

A command the risk classifier rates `Critical` (`rm -rf /`, a fork bomb, `dd if=`) is `Forbidden`: no permission mode approves it.

A command that destroys data (`rm`, `shred`, `dd`, `truncate`, `mkfs`, `mv -f`, `git clean -f`) prompts even when an allow rule or a prefix allowlist entry already covers `Bash`, because that approval was granted for a tool rather than for a deletion. `bypassPermissions` still allows it. See [Configuration](configuration.md#commands-that-destroy-data-always-ask).

When `run_in_background` is `true`, the task ID is returned immediately. Use `monitor` to check status, retrieve output, or cancel the task.

---

### monitor

**Permission level:** ReadOnly

Monitor background tasks started with `Bash`'s `run_in_background=true`. Supports listing all tasks, checking the status or output of a specific task, and cancelling a running task.

| Parameter | Type   | Required | Description                                                             |
|-----------|--------|----------|-------------------------------------------------------------------------|
| `action`  | string | no       | `list` (default), `status`, `output`, or `cancel`                       |
| `task_id` | string | no       | Task ID to inspect or cancel. Required for `status`, `output`, `cancel` |

**Actions:**

| Action   | Effect                                                       |
|----------|--------------------------------------------------------------|
| `list`   | Lists all background tasks with their IDs, status, and names |
| `status` | Shows the status and metadata for a specific task            |
| `output` | Retrieves the stdout/stderr output collected so far          |
| `cancel` | Sends a termination signal to a running task                 |

Task statuses: `running`, `completed`, `failed: <reason>`, `cancelled`.

---

### PowerShell

**Permission level:** Execute

Execute a PowerShell command on Windows hosts. Equivalent to `Bash` but uses `pwsh` (PowerShell Core) or `powershell.exe` as the shell.

| Parameter | Type    | Required | Description                   |
|-----------|---------|----------|-------------------------------|
| `command` | string  | yes      | PowerShell command to execute |
| `timeout` | integer | no       | Timeout in milliseconds       |

Available only when running on Windows.

---

### REPL

**Permission level:** Execute

Maintain a persistent REPL session for a supported language (Python, Node.js, Ruby, etc.). State accumulates between calls — variables, imports, and definitions persist for the duration of the session.

| Parameter  | Type   | Required | Description                                      |
|------------|--------|----------|--------------------------------------------------|
| `language` | string | yes      | Language runtime (`python`, `node`, `ruby`, ...) |
| `code`     | string | yes      | Code to evaluate                                 |

Useful for iterative data exploration or multi-step computations where re-running from scratch each time would be expensive.

---

## Search Tools

### Glob

**Permission level:** ReadOnly

Find files matching a glob pattern. Searches from a specified directory (defaults to the current working directory). Returns matching file paths sorted by modification time.

| Parameter | Type   | Required | Description                                   |
|-----------|--------|----------|-----------------------------------------------|
| `pattern` | string | yes      | Glob pattern (e.g., `**/*.rs`, `src/**/*.ts`) |
| `path`    | string | no       | Directory to search from; `&root-name` targets another workspace root|

---

### Grep

**Permission level:** ReadOnly

Search file contents using regular expressions, powered by ripgrep. Supports multiple output modes: matching lines with context, file paths only, or match counts.

| Parameter     | Type    | Required | Description                                 |
|---------------|---------|----------|---------------------------------------------|
| `pattern`     | string  | yes      | Regular expression pattern                  |
| `path`        | string  | no       | Directory or file to search; `&root-name` targets another workspace root|
| `glob`        | string  | no       | File glob filter (e.g., `*.rs`)             |
| `type`        | string  | no       | File type filter (e.g., `rust`, `py`, `js`) |
| `output_mode` | string  | no       | `content`, `files_with_matches`, or `count` |
| `-i`          | boolean | no       | Case-insensitive search                     |
| `-n`          | boolean | no       | Show line numbers                           |
| `context`     | integer | no       | Lines of context around each match          |
| `multiline`   | boolean | no       | Enable multiline matching                   |
| `head_limit`  | integer | no       | Limit output lines (default 250)            |

---

### ToolSearch

**Permission level:** ReadOnly

Search available tools by name or keyword to retrieve their full parameter schemas. Used internally by the model to discover deferred tools before calling them.

| Parameter     | Type    | Required | Description                 |
|---------------|---------|----------|-----------------------------|
| `query`       | string  | yes      | Tool name or keyword search |
| `max_results` | integer | no       | Maximum results (default 5) |

---

## Web Tools

### WebFetch

**Permission level:** ReadOnly

Fetch the content of a URL. Returns the page content, typically converted to Markdown for readability. Supports HTML pages, plain text, JSON, and PDF documents.

| Parameter | Type   | Required | Description                                 |
|-----------|--------|----------|---------------------------------------------|
| `url`     | string | yes      | URL to fetch                                |
| `prompt`  | string | no       | Optional extraction prompt to focus content |

Network requests are subject to the host's firewall and proxy settings.

---

### WebSearch

**Permission level:** ReadOnly

Perform a web search and return a list of results with titles, URLs, and snippets.

The backend is selected by environment, in priority order:

1. **SearXNG** — a self-hosted instance's base URL, from the `searxngUrl` setting or, failing that, the `SEARXNG_URL` environment variable. The instance's `settings.yml` must have the JSON `format` enabled.
2. **Brave Search** — set `BRAVE_SEARCH_API_KEY`.
3. **DuckDuckGo** — no-config fallback used when neither of the above is set.

The easiest way to configure SearXNG is `/settings`: turn on **SearXNG** and it
asks for the address, seeded with `http://localhost:8080`, which is the port
SearXNG binds by default. Turning it off clears the address.

A backend is only tried when it has been configured. No address is guessed,
because whatever answers a guessed port would receive the search query.

SearXNG results carry the upstream engines that surfaced them, rendered as
`[engines: google, duckduckgo]`. The instance already returns them ranked.

| Parameter     | Type    | Required | Description                                    |
|---------------|---------|----------|--------------------------------------------------|
| `query`       | string  | yes      | Search query                                   |
| `num_results` | integer | no       | Number of results to return. Default 5, max 20 |

#### When SearXNG is unreachable

By default the tool reports the failure and stops. It does not move on to Brave
or DuckDuckGo, so a query aimed at a private instance never reaches a third
party by surprise.

Set `"webSearchFallback": true` in the `config` block of `settings.json`, or
turn on **Web search fallback** in `/settings`, to let the tool continue with
Brave (when `BRAVE_SEARCH_API_KEY` is set) or DuckDuckGo. The result then opens
with the name of the backend that took over.

---

## Task Management Tools

The task system allows the model to create and track long-running background work.

### TaskCreate

**Permission level:** Execute

Create a new background task. The task runs asynchronously; use `TaskGet` or `TaskOutput` to poll for completion.

| Parameter     | Type    | Required | Description                        |
|---------------|---------|----------|------------------------------------|
| `description` | string  | yes      | Human-readable task description    |
| `command`     | string  | yes      | Shell command or prompt to execute |
| `timeout`     | integer | no       | Maximum runtime in milliseconds    |

Returns a `task_id` for use with other task tools.

---

### TaskGet

**Permission level:** ReadOnly

Get the current state of a task by ID.

| Parameter | Type   | Required | Description     |
|-----------|--------|----------|-----------------|
| `task_id` | string | yes      | Task identifier |

Returns status (`pending`, `running`, `completed`, `failed`), progress, and partial output.

---

### TaskList

**Permission level:** ReadOnly

List all tasks in the current session with their statuses.

| Parameter | Type   | Required | Description                                               |
|-----------|--------|----------|-----------------------------------------------------------|
| `filter`  | string | no       | Filter by status: `all`, `running`, `completed`, `failed` |

---

### TaskUpdate

**Permission level:** Execute

Update the parameters of a running or pending task.

| Parameter     | Type   | Required | Description     |
|---------------|--------|----------|-----------------|
| `task_id`     | string | yes      | Task identifier |
| `description` | string | no       | New description |

---

### TaskStop

**Permission level:** Execute

Terminate a running task.

| Parameter | Type   | Required | Description             |
|-----------|--------|----------|-------------------------|
| `task_id` | string | yes      | Task identifier to stop |

Sends SIGTERM to the task process. If it does not exit within a grace period, SIGKILL is sent.

---

### TaskOutput

**Permission level:** ReadOnly

Retrieve the accumulated stdout/stderr output from a task.

| Parameter | Type    | Required | Description                              |
|-----------|---------|----------|------------------------------------------|
| `task_id` | string  | yes      | Task identifier                          |
| `offset`  | integer | no       | Byte offset to read from (for streaming) |

---

### TodoWrite

**Permission level:** Write

Write or update the session TODO list. The TODO list is a structured set of tasks tracked across the session and displayed in the TUI sidebar.

| Parameter | Type  | Required | Description                                        |
|-----------|-------|----------|----------------------------------------------------|
| `todos`   | array | yes      | Array of `{id, content, status, priority}` objects |

Status values: `pending`, `in_progress`, `completed`. Priority values: `low`, `medium`, `high`.

---

## MCP Integration Tools

Model Context Protocol (MCP) tools bridge MikMik to external MCP servers.

### ListMcpResources

**Permission level:** ReadOnly

List resources exposed by a connected MCP server.

| Parameter     | Type   | Required | Description                   |
|---------------|--------|----------|-------------------------------|
| `server_name` | string | yes      | MCP server name as configured |

Returns a list of resource URIs with descriptions.

---

### ReadMcpResource

**Permission level:** ReadOnly

Read the content of a specific resource from an MCP server.

| Parameter     | Type   | Required | Description          |
|---------------|--------|----------|----------------------|
| `server_name` | string | yes      | MCP server name      |
| `uri`         | string | yes      | Resource URI to read |

---

### mcp__auth

**Permission level:** Execute

Authenticate with an MCP server that requires credentials. Triggers the server's authentication flow and stores the resulting tokens.

| Parameter     | Type   | Required | Description     |
|---------------|--------|----------|-----------------|
| `server_name` | string | yes      | MCP server name |

---

## Agent Tools

Agent tools enable multi-agent coordination: spawning sub-agents, forming teams, and passing messages between them.

### Agent

**Permission level:** Execute

Run a sub-agent on a task of its own. The sub-agent gets a fresh context and reports back a single result, so a long search or a bounded piece of work does not fill the caller's context.

| Parameter           | Type    | Required | Description                                                          |
|---------------------|---------|----------|----------------------------------------------------------------------|
| `description`       | string  | yes      | Three to five words naming the task                                  |
| `prompt`            | string  | yes      | The complete task for the agent                                      |
| `tools`             | array   | no       | Tool names the agent may use. Defaults to all of them                |
| `system_prompt`     | string  | no       | Replaces the sub-agent's system prompt                               |
| `max_turns`         | number  | no       | Turn limit for the sub-agent (default 10)                            |
| `model`             | string  | no       | A different model for this agent                                     |
| `isolation`         | string  | no       | `worktree` runs the agent in its own git worktree                    |
| `run_in_background` | boolean | no       | Return an `agent_id` at once instead of waiting                      |

An agent never receives the `Agent` tool, so it cannot spawn further agents. `isolation: worktree` is what keeps parallel agents from writing over each other. A background agent is polled through `monitor` with `action=status` or `action=output` and `task_id` set to the returned `agent_id`.

---

### Memory

**Permission level:** ReadOnly

Load the full text of memory files about a topic. The system prompt lists which memory files exist; this tool reads the ones that look relevant.

| Parameter   | Type    | Required | Description                                    |
|-------------|---------|----------|------------------------------------------------|
| `query`     | string  | yes      | The topic to search for                        |
| `max_files` | integer | no       | How many files to return                       |

The query is scored against each file's name, description and filename, so search by topic rather than by exact wording.

---

### SendMessage

**Permission level:** Execute

Send a message to another agent (sub-agent or coordinator). Used for inter-agent communication in multi-agent workflows.

| Parameter | Type   | Required | Description                                       |
|-----------|--------|----------|---------------------------------------------------|
| `to`      | string | yes      | Target agent name, or `main` for the main session |
| `message` | string | yes      | Message content                                   |
| `summary` | string | no       | A short preview line shown in the UI              |

---

### TeamCreate

**Permission level:** Execute

Create a team of sub-agents to work in parallel on a set of tasks. Each agent in the team receives its own context and toolset.

| Parameter     | Type    | Required | Description                                                     |
|---------------|---------|----------|-----------------------------------------------------------------|
| `team_name`   | string  | yes      | Identifier for the team                                         |
| `task`        | string  | yes      | The shared task every agent works on                            |
| `agents`      | array   | no       | Agent specs: `{name, role, tools, task}`; `name` is required    |
| `parallel`    | boolean | no       | Run the agents at the same time (default `true`)                |
| `description` | string  | no       | Team description stored in the configuration                    |

An agent's `tools` limits it to the named tools; omitting the field gives it all of them. Its `task` overrides the shared one.

---

### TeamDelete

**Permission level:** Execute

Dissolve a team and terminate all its member agents.

| Parameter   | Type   | Required | Description      |
|-------------|--------|----------|------------------|
| `team_name` | string | yes      | Team to dissolve |

---

### AcpAgent

**Permission level:** Execute

Delegate a task to an external agent that speaks the [Agent Client Protocol](https://agentclientprotocol.com/). The agent runs as a subprocess in the session's working directory and is driven over stdio.

| Parameter | Type   | Required | Description                                             |
|-----------|--------|----------|---------------------------------------------------------|
| `agent`   | string | yes      | Name of a configured agent, from `acpAgents` in settings |
| `prompt`  | string | yes      | The task to delegate                                     |

The tool is only offered when at least one agent is configured. Define them in `~/.config/mikmik/settings.json`:

```json
{
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
}
```

`env` values go through `{env:VARNAME}` substitution, so a token can be named rather than written into the settings file.

Every action the sub-agent asks to take arrives as a `session/request_permission` request and is answered through the same permission prompt as a local tool. A denial is sent back as a rejection; if the agent offers no option matching the decision, the request is cancelled rather than answered with an unrelated choice.

The turn is bounded: cancelling the session, or ten minutes elapsing, kills the subprocess and reports the tail of its stderr.

**Security:** an agent definition names an executable that the model can invoke, so `acpAgents` is read only from your own global settings. A project's `.mikmik/settings.json` cannot add one.

---

### RemoteTrigger

**Permission level:** Execute

Send an event to another session, so work finishing here can wake something waiting there.

| Parameter    | Type   | Required | Description                                              |
|--------------|--------|----------|----------------------------------------------------------|
| `session_id` | string | yes      | The session to trigger                                   |
| `event_name` | string | yes      | The event name, such as `task_complete` or `result_ready`|
| `payload`    | object | no       | JSON delivered with the event                            |

---

### Skill

**Permission level:** Execute

Invoke a named skill (bundled prompt-command) programmatically from within a tool call chain.

| Parameter | Type   | Required | Description                    |
|-----------|--------|----------|--------------------------------|
| `skill`   | string | yes      | Skill name                     |
| `args`    | string | no       | Arguments to pass to the skill |

---

### GoalComplete

**Permission level:** None

Mark the active goal as complete. This tool is surfaced to the model when a `/goal` is active. The model calls it only after performing a genuine completion audit — verifying the goal has been fully met rather than partially addressed.

| Parameter       | Type   | Required | Description                                                                                                |
|-----------------|--------|----------|------------------------------------------------------------------------------------------------------------|
| `audit_summary` | string | yes      | Concise summary of the goal-completion audit                                                               |
| `evidence`      | string | yes      | Specific evidence demonstrating the goal was achieved (files changed, tests passed, output produced, etc.) |

Calling this tool triggers the goal system to mark the goal as `Completed` and surfaces the audit results to the user. The model is expected to verify the goal thoroughly before calling — calling without genuine evidence is treated as an error.

See also: `/goal complete` command.

---

### Advisor

**Permission level:** None

Ask a second, independent model to review a decision before acting on it. The model calls this itself when a change is hard to reverse, when two designs are genuinely close, or when it doubts its own answer.

| Parameter  | Type   | Required | Description                                                                        |
|------------|--------|----------|--------------------------------------------------------------------------------------|
| `question` | string | yes      | The specific decision, claim, or trade-off to review                               |
| `context`  | string | no       | The material to review: a diff, a plan, or a code snippet                          |

The advisor has no access to the conversation, so the caller must include everything it needs to judge. Its reply comes back as an ordinary tool result and the transcript shows a single status line rather than repeating the question.

The tool is only registered when `advisorModel` is configured (see [`/advisor`](commands.md#advisor)), so a session without an advisor pays neither the schema cost nor the system-prompt guideline. Calls are capped at two per turn; beyond that the tool returns an error telling the model to decide with what it has.

Which credentials the call uses follows `advisorModel`. By default it is the same provider and account as the session. When the setting names an account (`anthropic:personal/sonnet`), the advisor authenticates as that stored login instead, leaving the session on its own.

Advisor tokens are added to the session cost. `CostTracker` prices every token at the session model's rate, so the figure drifts when the advisor model is priced differently.

---

## Notebook Tools

### NotebookEdit

**Permission level:** Write

Edit a Jupyter notebook (`.ipynb`) by modifying, inserting, or deleting cells. Operates on the notebook's JSON structure directly.

| Parameter       | Type   | Required | Description                                                         |
|-----------------|--------|----------|---------------------------------------------------------------------|
| `notebook_path` | string | yes      | Absolute, working-directory-relative, or `&root-name/relative` path |
| `cell_id`       | string | no       | Cell ID, a UUID or `cell-N`. Required for `replace` and `delete`    |
| `new_source`    | string | no       | New cell content. Required for `replace` and `insert`               |
| `cell_type`     | string | no       | `code` or `markdown`, for `insert` (default `code`)                 |
| `edit_mode`     | string | no       | `replace` (default), `insert`, or `delete`                          |

---

## Planning Tools

### EnterPlanMode

**Permission level:** None

Switch the agent into plan mode. In plan mode, all write and execute tools are blocked. The agent can only read files, search, and reason. Used to draft an approach before taking action.

| Parameter | Type   | Required | Description                    |
|-----------|--------|----------|--------------------------------|
| `reason`  | string | no       | Why plan mode is being entered |

The switch is real and takes effect immediately: the permission mode becomes `plan` and the tool list is rebuilt without the write and execute tools, exactly as `/plan` and `Tab` do. The new mode reaches the turn that is already running, so a write attempted straight after the switch is refused rather than allowed until the next key press. No approval is asked for, because the tool can only narrow what the agent may do. The mode in force beforehand is remembered, so leaving plan mode restores it.

When the model reaches for this tool is decided by the base system prompt, which lists what counts as significant work and what does not, and asks the model to use `AskUserQuestion` for anything the request leaves open. Replacing that prompt with `customSystemPrompt` removes the guidance; the tool description repeats the essentials, but the choice then rests entirely on it.

Only the interactive TUI can switch modes. In headless runs (`--print`) and over ACP there is nowhere to apply the switch, so the tool returns an error saying the mode did not change.

Exits when `/plan` is invoked again, when `Tab` switches back to build mode, or when `ExitPlanMode` is called and the plan is approved.

---

### ExitPlanMode

**Permission level:** None

Write the plan to the next numbered file under `<config dir>/plans/<session id>/`, put it in front of the user and wait for their decision. The turn is blocked until they answer, and the answer decides the permission mode the session lands in. A plan edited in the dialog is read back from the file and returned in place of the one that was proposed. See [Plan mode](configuration.md#plan) for the four answers.

| Parameter | Type   | Required | Description                  |
|-----------|--------|----------|------------------------------|
| `summary` | string | no       | The plan, as written for the user |

Headless (`--print`) has no dialog to ask through: there the tool writes the plan file, reports the plan and leaves plan mode without blocking.

Pass the whole plan as `summary`, not a one-line description. The file is what the user reads and edits, and it is the only lasting record of the proposal once the transcript scrolls away. The base system prompt and the `plan` agent prompt both say so.

---

## Worktree Tools

Worktree tools manage git worktrees, enabling the agent to work on multiple branches simultaneously in isolated directories.

### EnterWorktree

**Permission level:** Execute

Create a git worktree on a new branch and switch the agent's working directory to it.

| Parameter             | Type   | Required | Description                                                    |
|-----------------------|--------|----------|----------------------------------------------------------------|
| `branch`              | string | no       | Branch to create. Defaults to a timestamped name               |
| `path`                | string | no       | Worktree directory. Defaults to `.worktrees/<branch>`          |
| `post_create_command` | string | no       | Command to run inside the new worktree, such as `npm install`  |

---

### ExitWorktree

**Permission level:** Execute

Leave the worktree and return to the original working directory.

| Parameter         | Type    | Required | Description                                                       |
|-------------------|---------|----------|-------------------------------------------------------------------|
| `action`          | string  | yes      | `keep` leaves the worktree on disk; `remove` deletes it and its branch |
| `discard_changes` | boolean | no       | Required `true` to remove a worktree holding uncommitted or unmerged work |

Without `discard_changes`, `remove` refuses rather than discarding work.

---

## Utility Tools

### AskUserQuestion

**Permission level:** Execute

Pause execution and ask the user a question via an interactive prompt in the TUI. Returns the user's typed response. Use sparingly — prefer acting with best judgment and asking only when the choice is genuinely ambiguous.

| Parameter  | Type   | Required | Description                                 |
|------------|--------|----------|---------------------------------------------|
| `question` | string | yes      | Question text to display                    |
| `options`  | array  | no       | Strings offered as choices. The user picks one or types their own answer |

---

### Brief

**Permission level:** ReadOnly

Emit a short status message to the session output without triggering a full model response. Used in automated pipelines to surface progress updates.

| Parameter     | Type   | Required | Description                                                    |
|---------------|--------|----------|----------------------------------------------------------------|
| `message`     | string | yes      | The message, in Markdown                                       |
| `status`      | string | yes      | `proactive` for an unsolicited update, `normal` for a reply    |
| `attachments` | array  | no       | File paths to attach: images, diffs, logs                      |

---

### Sleep

**Permission level:** None

Pause execution for a specified duration. Useful in polling loops or when waiting for external processes.

| Parameter | Type    | Required | Description           |
|-----------|---------|----------|-----------------------|
| `ms`      | integer | yes      | Milliseconds to sleep |

The maximum is 300000 ms (5 minutes) per call.

---

### Config

**Permission level:** Write

Read or write a MikMik setting. Omitting `value` reads the current one; supplying it writes to `settings.json`, where the next session picks it up.

`permission_mode` is the one exception: it can be read but not written. A turn that set `bypass_permissions` would be switching off the checks that gate its own next tool call, in this session and every later one. Only the user changes it, through `/permissions set`, `/yolo`, or `--permission-mode`.

| Parameter | Type   | Required | Description                                              |
|-----------|--------|----------|----------------------------------------------------------|
| `setting` | string | yes      | Setting key, or `list` to see every key the tool accepts |
| `value`   | any    | no       | Value to write. Omit to read instead.                    |

Accepted keys:

| Key               | Type    | Description                                                                              |
|-------------------|---------|--------------------------------------------------------------------------------------------|
| `model`           | string  | Model ID to use                                                                             |
| `provider`        | string  | Account the turn is routed to                                                               |
| `effort`          | string  | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or `ultracode`                  |
| `max_tokens`      | integer | Maximum output tokens per response                                                          |
| `verbose`         | boolean | Verbose logging                                                                             |
| `permission_mode` | string  | `default`, `accept_edits`, `bypass_permissions`, or `plan`. Read-only                       |
| `auto_compact`    | boolean | Auto-compact the conversation when the context fills                                        |

---

## Cron Tools

Cron tools manage scheduled agent triggers.

### CronCreate

**Permission level:** Write

Create a new scheduled trigger. The trigger fires at the specified cron schedule and executes the configured agent or command.

| Parameter   | Type    | Required | Description                                                           |
|-------------|---------|----------|-----------------------------------------------------------------------|
| `cron`      | string  | yes      | Five-field cron expression: `M H DoM Mon DoW`                         |
| `prompt`    | string  | yes      | The prompt to run at each scheduled time                              |
| `recurring` | boolean | no       | `true` (default) repeats; `false` fires once, then deletes the task    |
| `durable`   | boolean | no       | `true` persists to `.mikmik/scheduled_tasks.json`; `false` (default) lasts for the session |

Times are read in the local timezone, so `0 9 * * 1-5` means 9am local on weekdays.

---

### CronDelete

**Permission level:** Write

Delete a scheduled trigger by name.

| Parameter | Type   | Required | Description                    |
|-----------|--------|----------|--------------------------------|
| `id`      | string | yes      | The task ID `CronCreate` returned |

---

### CronList

**Permission level:** ReadOnly

List all scheduled triggers with their schedules and enabled states.

No parameters.

---

## Code Intelligence Tools

Code intelligence tools query language servers for semantic information about source code.

### LSP

**Permission level:** ReadOnly

Query a language server for code intelligence. Servers for common projects are
detected automatically; more can be declared in `settings.json` under
`lsp_servers`.

| Parameter  | Type    | Required | Description                                                                                                     |
|------------|---------|----------|-------------------------------------------------------------------------------------------------------------------|
| `action`   | string  | yes      | See the table below.                                                                                            |
| `file`     | string  | no       | Absolute or working-directory-relative path. `"*"` or omitted means the workspace, for `symbols`, `reload`, `capabilities` and `request`. |
| `line`     | integer | no       | 1-based line number, for the position-based actions.                                                            |
| `column`   | integer | no       | 1-based column number. Prefer `symbol`.                                                                         |
| `symbol`   | string  | no       | The symbol on `line` to point at. `name#2` selects the second occurrence on that line.                          |
| `query`    | string  | no       | Workspace symbol search text, code-action selector, or the method name for `request`.                           |
| `new_name` | string  | no       | The new name for `rename`, or the destination path for `rename_file`.                                           |
| `apply`    | boolean | no       | `rename` and `rename_file` apply by default; `false` previews. `code_actions` lists by default; `true` applies.  |
| `payload`  | string  | no       | JSON parameters for `request`.                                                                                  |

**Actions:**

| Action            | Writes | Description                                                                 |
|-------------------|--------|-----------------------------------------------------------------------------|
| `hover`           | no     | Documentation and type of the symbol at the position                        |
| `definition`      | no     | Where the symbol is defined                                                 |
| `type_definition` | no     | Where the symbol's type is defined                                          |
| `implementation`  | no     | What implements the interface or trait at the position                      |
| `references`      | no     | Every reference to the symbol at the position                               |
| `symbols`         | no     | One file's symbols, or the workspace's with `file: "*"` and a `query`       |
| `diagnostics`     | no     | Errors and warnings for the file                                            |
| `status`          | no     | Which servers are configured, running, or missing their binary              |
| `capabilities`    | no     | What a server says it supports                                              |
| `rename`          | yes    | Rename the symbol everywhere it is used                                     |
| `rename_file`     | yes    | Move a file or directory and update every reference to it                   |
| `code_actions`    | yes    | List the fixes and refactorings offered, and apply one                      |
| `reload`          | yes    | Re-read the configuration and push it to the servers again                  |
| `request`         | yes    | Send a raw LSP request, for anything the actions above do not cover         |

**Naming a position.** `symbol` is the reliable way to point at a token:
counting columns by hand is the commonest way a request lands on the wrong one
and answers nothing. With neither `column` nor `symbol`, the first
non-whitespace column of the line is used, and `definition`, `references` and
`rename` refuse rather than use it: their wrong answer is an empty list, which
reads as "nothing found" and is acted on.

`references` asks again, twice, when the only answer is the declaration it was
given. That is usually a server that has not finished indexing rather than a
symbol nothing uses.

A symbol list carries the line of each entry, and a symbol the server marks as
deprecated says so.

**Rename.** `rename` applies the edits unless `apply` is `false`. A create,
rename or delete of a whole file inside the server's answer is reported and not
performed; use `rename_file` for that, which asks every server for the edits
the move needs, applies them, moves the path, and tells the servers it moved.
A directory move is limited to 1000 files.

**Code actions.** Listing shows an index and a title for each. Applying takes
`apply: true` and a `query` that is either the index or part of the title. The
action's edit is applied first, then its command is run, which is the order the
protocol specifies.

`diagnostics` asks every server that handles the file, linters included, and
waits up to three seconds for a fresh answer rather than reading whatever the
cache holds. A server that answers on request rather than publishing is asked
directly, because waiting for a notification from one of those waits forever.
The same problem reported by two servers is shown once, errors come before
warnings and hints, and at most 50 messages are returned.

`file` may also be a glob, which reports on up to 20 matching files with a
shorter wait each, or `"*"`, which runs the project's own check instead of
asking the servers: `cargo check`, `tsc --noEmit`, `go build ./...` or
`pyright`, chosen by the marker files in the working directory. That covers
what a language server cannot: a change that breaks a file nothing has opened.
It runs a build, so it asks for the same permission a write does, and it is cut
off after two minutes.

The other actions go to the first matching server that is not a linter, and
wait while that server reports work in progress, because a server that is still
indexing answers "nothing found" rather than "not ready".

**Configuration** (`settings.json`):

```json
{
  "lsp_servers": [
    {
      "name": "rust-analyzer",
      "command": "rust-analyzer",
      "args": [],
      "file_patterns": ["*.rs"],
      "root_markers": ["Cargo.toml"]
    },
    {
      "name": "typescript-language-server",
      "command": "typescript-language-server",
      "args": ["--stdio"],
      "file_patterns": ["*.ts", "*.tsx", "*.js", "*.jsx"],
      "root_markers": ["package.json", "tsconfig.json"]
    }
  ]
}
```

| Field                     | Type            | Required | Description                                                                                              |
|---------------------------|-----------------|----------|----------------------------------------------------------------------------------------------------------|
| `name`                    | string          | yes      | The server's identity. A later entry of the same name replaces the earlier one.                          |
| `command`                 | string          | yes      | The binary to run, by name or by absolute path.                                                          |
| `args`                    | string[]        | yes      | Arguments passed to the binary.                                                                          |
| `file_patterns`           | string[]        | yes      | `*.ext` selects an extension. A pattern without `*.` matches a whole file name, such as `Dockerfile`.    |
| `root_markers`            | string[]        | no       | Files or directories that mark a project this server serves.                                             |
| `disabled`                | boolean         | no       | Switch the server off without deleting the entry. Default `false`.                                       |
| `is_linter`               | boolean         | no       | The server only reports problems. It answers diagnostics and never navigation. Default `false`.          |
| `language_id`             | string          | no       | One language id for every file. Overrides `extension_to_language` and the built-in table.                |
| `extension_to_language`   | object          | no       | Per-extension language id, e.g. `{".rs": "rust"}`.                                                       |
| `initialization_options`  | object          | no       | Sent once in the `initialize` handshake.                                                                 |
| `settings`                | object          | no       | Sent with `workspace/didChangeConfiguration` after the handshake. Unlike the line above, it can change.  |
| `env`                     | object          | no       | Extra environment variables for the server process.                                                      |
| `warmup_timeout_ms`       | number          | no       | Budget for the handshake. Default 5000.                                                                  |
| `request_timeout_ms`      | number          | no       | Budget for one request. Default 30000.                                                                   |
| `capabilities`            | object          | no       | Opt-in non-standard features: `flycheck`, `ssr`, `expand_macro`, `runnables`, `related_tests`.           |
| `workspace_ready_timings` | object          | no       | Project-load wait overrides: `timeout_ms`, `poll_ms`, `settle_ms`, `status_request_timeout_ms`.          |

**The language id.** `language_id` wins, then `extension_to_language`, then a
built-in table that covers the common extensions. Only an extension none of
them knows falls back to `plaintext`, which most servers ignore. So a server
that serves one language needs no extension map.

**Routing.** Every enabled server whose `file_patterns` match the file answers
`diagnostics`. Navigation actions go to the first matching server that is not a
linter.

**Precedence.** A project's `settings.json` entry replaces the user's entry of
the same name rather than joining it, because two entries of one name would
both match and the winner would depend on their order. A project-supplied
server names a binary to run, so it is only taken after you approve the
project's settings.

**Detection.** Nothing has to be configured for a common project. The binary
ships with a catalogue of language servers, and a catalogue server is used when
the working directory carries one of its root markers and its binary resolves.
Switch the catalogue off with `"lsp_auto_detect": false`, or from the
**Detect language servers** row in the settings screen. See
[configuration.md#language-servers](configuration.md#language-servers) for where
the binary is looked for.

When no server answers, the tool names the reason for each server that could
have served the file: the marker is missing, the binary is not installed, or
the entry is switched off.

**The catalogue.**

| Server                        | Files                          | Binary                            |
|-------------------------------|--------------------------------|-----------------------------------|
| `rust-analyzer`               | Rust                           | `rust-analyzer`                   |
| `clangd`                      | C, C++, Objective-C            | `clangd`                          |
| `zls`                         | Zig                            | `zls`                             |
| `gopls`                       | Go                             | `gopls`                           |
| `typescript-language-server`  | TypeScript, JavaScript         | `typescript-language-server`      |
| `denols`                      | TypeScript, JavaScript (Deno)  | `deno`                            |
| `biome`                       | TS/JS/JSON/CSS (linter)        | `biome`                           |
| `eslint`                      | TS/JS/Vue/Svelte (linter)      | `vscode-eslint-language-server`   |
| `vscode-html-language-server` | HTML                           | `vscode-html-language-server`     |
| `vscode-css-language-server`  | CSS, SCSS, Sass, Less          | `vscode-css-language-server`      |
| `vscode-json-language-server` | JSON                           | `vscode-json-language-server`     |
| `tailwindcss`                 | HTML, CSS, TS/JS, Vue, Svelte  | `tailwindcss-language-server`     |
| `svelte`                      | Svelte                         | `svelteserver`                    |
| `vue-language-server`         | Vue                            | `vue-language-server`             |
| `astro`                       | Astro                          | `astro-ls`                        |
| `pyright`                     | Python                         | `pyright-langserver`              |
| `basedpyright`                | Python                         | `basedpyright-langserver`         |
| `pylsp`                       | Python                         | `pylsp`                           |
| `ty`                          | Python                         | `ty`                              |
| `ruff`                        | Python (linter)                | `ruff`                            |
| `jdtls`                       | Java                           | `jdtls`                           |
| `kotlin-lsp`                  | Kotlin                         | `kotlin-lsp`                      |
| `metals`                      | Scala                          | `metals`                          |
| `hls`                         | Haskell                        | `haskell-language-server-wrapper` |
| `ocamllsp`                    | OCaml                          | `ocamllsp`                        |
| `elixirls`                    | Elixir                         | `elixir-ls`                       |
| `expert`                      | Elixir                         | `expert`                          |
| `erlangls`                    | Erlang                         | `erlang_ls`                       |
| `gleam`                       | Gleam                          | `gleam`                           |
| `solargraph`                  | Ruby                           | `solargraph`                      |
| `ruby-lsp`                    | Ruby                           | `ruby-lsp`                        |
| `rubocop`                     | Ruby (linter)                  | `rubocop`                         |
| `bashls`                      | Bash, Zsh                      | `bash-language-server`            |
| `lua-language-server`         | Lua                            | `lua-language-server`             |
| `intelephense`                | PHP                            | `intelephense`                    |
| `phpactor`                    | PHP                            | `phpactor`                        |
| `omnisharp`                   | C#                             | `omnisharp`                       |
| `yamlls`                      | YAML                           | `yaml-language-server`            |
| `terraformls`                 | Terraform                      | `terraform-ls`                    |
| `dockerls`                    | Dockerfile                     | `docker-langserver`               |
| `helm-ls`                     | Helm templates                 | `helm_ls`                         |
| `nixd`                        | Nix                            | `nixd`                            |
| `nil`                         | Nix                            | `nil`                             |
| `ols`                         | Odin                           | `ols`                             |
| `dartls`                      | Dart                           | `dart`                            |
| `marksman`                    | Markdown                       | `marksman`                        |
| `texlab`                      | LaTeX, BibTeX                  | `texlab`                          |
| `graphql`                     | GraphQL                        | `graphql-lsp`                     |
| `prismals`                    | Prisma                         | `prisma-language-server`          |
| `vimls`                       | Vim script                     | `vim-language-server`             |
| `emmet-language-server`       | HTML, CSS, JSX, Vue, Svelte    | `emmet-language-server`           |
| `sourcekit-lsp`               | Swift                          | `sourcekit-lsp`                   |
| `swiftlint`                   | Swift (linter)                 | `swiftlint`                       |
| `tlaplus`                     | TLA+                           | `tlapm_lsp`                       |

**Protocol behaviour.** Points worth knowing when a server misbehaves:

- The server's `initialize` answer is kept, so a request the server does not
  advertise is not sent at all.
- A request that reaches its timeout is cancelled with `$/cancelRequest`, so the
  server stops working on an answer nobody will read.
- Requests the server sends are answered: its configuration
  (`workspace/configuration`), the workspace folders, capability registration,
  progress creation, and message or document requests. Anything else is refused
  with "method not found" rather than ignored, because an unanswered request can
  stall a server.
- A server-initiated `workspace/applyEdit` is applied to the files on disk. A
  create, rename or delete inside one is reported and not performed.
- `settings` is pushed after the handshake with
  `workspace/didChangeConfiguration`.
- Before a navigation request, the tool waits while the server reports work in
  progress, because a server that is still indexing answers "nothing found"
  rather than "not ready". The wait is bounded by `workspace_ready_timings`.
- The handshake has its own budget, `warmup_timeout_ms`, separate from the
  per-request `request_timeout_ms`.
- When a server exits, every request still waiting fails with the reason and the
  last lines the server wrote to its standard error.

The tool resolves relative paths against the current working directory.

---

## Advanced Tools

### computer

**Permission level:** Dangerous

Control the desktop GUI — move the mouse, click, type, take screenshots, and interact with applications. Enables the agent to operate software that has no API or CLI interface.

| Parameter          | Type    | Required | Description                                                        |
|--------------------|---------|----------|--------------------------------------------------------------------|
| `action`           | string  | yes      | See the list below                                                 |
| `coordinate`       | array   | no       | `[x, y]` pixel coordinate for mouse actions                        |
| `start_coordinate` | array   | no       | Start `[x, y]` for `left_click_drag`                               |
| `end_coordinate`   | array   | no       | End `[x, y]` for `left_click_drag`                                 |
| `text`             | string  | no       | Text to type, or the key sequence to press, such as `ctrl+c`       |
| `direction`        | string  | no       | Scroll direction: `up`, `down`, `left`, or `right`                 |
| `amount`           | integer | no       | Number of scroll notches                                           |

Actions: `screenshot`, `mouse_move`, `left_click`, `right_click`, `double_click`, `left_click_drag`, `type_text`, `key`, `scroll`, `get_cursor_position`.

This tool has the highest blast radius of any tool in MikMik. It requires explicit permission and should only be enabled in controlled environments. All actions are logged in detail.

Requires a display server (X11, Wayland, or Windows Desktop). Not available in headless environments.

---

### StructuredOutput

**Permission level:** None

Return structured JSON output as the agent's final response. This tool is surfaced only in non-interactive (SDK/headless) sessions and in hook handlers. The model must call it exactly once at the end of its response to deliver structured data to the caller.

The input schema is open — it accepts any JSON object. The specific expected schema is communicated via the system prompt for each session type.

**Example usage in a hook handler:**

```json
{
  "ok": true,
  "reason": "All tests passed."
}
```

**Example in an SDK session returning structured analysis:**

```json
{
  "summary": "Three security issues found.",
  "issues": [
    { "severity": "high", "description": "SQL injection in login handler" }
  ]
}
```

Calling this tool in an interactive session has no effect; the confirmation string is returned but the structured output is not surfaced to the TUI.

---

## Tool Framework Internals

### ToolContext

Every tool receives a `ToolContext` at call time. The fields that shape what a tool may do:

| Field                 | Description                                                                 |
|-----------------------|-----------------------------------------------------------------------------|
| `working_dir`         | The directory paths resolve against                                         |
| `permission_mode`     | The mode captured when the turn started                                     |
| `permission_handler`  | Where a permission question is asked                                        |
| `permission_manager`  | The live rule set, shared with the TUI, so a mode change reaches this turn   |
| `config`              | The session configuration                                                   |
| `session_id`          | The session the call belongs to                                             |
| `non_interactive`     | True in headless runs, where no dialog can be opened                        |
| `cancel_token`        | Cancelled when the turn is interrupted; long operations must check it       |
| `current_call`        | The tool call in flight, which is what streams live output                  |
| `file_history`        | The record `/undo` and `/rewind` replay                                     |
| `cost_tracker`        | Token and cost accounting for the session                                   |
| `mcp_manager`         | The connected MCP servers, when any                                         |
| `editor`              | The editor hosting the session, when one offered to host reads and writes   |

Four side channels carry a tool's request back to the TUI without a round trip through the model:

| Channel            | Carries                                                     |
|--------------------|-------------------------------------------------------------|
| `user_question_tx` | An `AskUserQuestion` prompt and its answer                   |
| `plan_approval_tx` | An `ExitPlanMode` plan and the user's decision               |
| `plan_mode_tx`     | An `EnterPlanMode` switch                                    |
| `tool_output_tx`   | Live output chunks from a running command                    |

Each is an `Option`. A session with no TUI leaves it `None`, and the tool says so rather than reporting a change it could not make.
