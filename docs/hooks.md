# Hooks

Hooks let you run a shell command in response to events that happen inside a MikMik session. They are the way to extend and automate MikMik without modifying the agent itself.

---

## What a hook is

A hook is a shell command MikMik runs at a specific lifecycle event. When the event fires, MikMik:

1. Serialises a JSON payload describing the event.
2. Passes that JSON to the hook's stdin.
3. Waits for the hook to exit, or kills it when its time limit passes.
4. Reads the exit code according to that event's rules.

The command runs through `sh -c` (`cmd /C` on Windows), so a hook can be written in any language that reads stdin and writes to stdout or stderr.

---

## Two hook systems

MikMik has two independent hook systems. They share the idea but not the events, the file format, or the fields.

| | Settings hooks | Plugin hooks |
|---|---|---|
| Declared in | the `hooks` key of `settings.json` | `plugin.json`, or `hooks/hooks.json` in the plugin |
| Events | six | twenty-seven |
| Shape | event name to a flat array of hooks | event name to an array of matcher objects |
| Filter field | `tool_filter` | `matcher` |

Both fire for `PreToolUse` and `PostToolUse`: the settings hooks run first, then the plugin hooks.

---

## Settings hooks

### Shape

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "command": "python3 ~/.config/mikmik/hooks/check_bash.py",
        "tool_filter": "Bash",
        "blocking": true,
        "timeout_ms": 5000
      }
    ]
  }
}
```

| Field         | Required | Description                                                                 |
|---------------|----------|-----------------------------------------------------------------------------|
| `command`     | yes      | The shell command. Receives the event JSON on stdin                         |
| `tool_filter` | no       | Runs only for this tool name. `*` matches every tool                        |
| `blocking`    | no       | `true` makes a non-zero exit block the operation. Default `false`           |
| `timeout_ms`  | no       | Time limit in milliseconds. Default 30000                                   |

Hooks in an array run in order. The command runs in the session's working directory.

### Exit codes and output

For `PreToolUse` and `PostToolUse`:

| Result                          | Effect                                                      |
|---------------------------------|-------------------------------------------------------------|
| Exit 0, no stdout               | Continue                                                    |
| Exit 0, stdout                  | The trimmed stdout replaces the input                       |
| Non-zero, `blocking: false`     | Ignored                                                     |
| Non-zero, `blocking: true`      | Blocks. stderr, or stdout when stderr is empty, says why    |
| Time limit passed, blocking     | Blocks. A hook that never answered is not read as approval  |
| Time limit passed, non-blocking | Skipped                                                     |

A blocked tool call returns `Blocked by hook: <reason>` to the model in place of the tool result.

### Payload

The JSON written to the hook's stdin:

```json
{
  "event": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "rm -rf /tmp/foo" },
  "session_id": "01hq..."
}
```

| Field         | Present on                                    |
|---------------|-----------------------------------------------|
| `event`       | always                                        |
| `tool_name`   | tool events                                   |
| `tool_input`  | tool events                                   |
| `tool_output` | `PostToolUse`                                 |
| `is_error`    | `PostToolUse`                                 |
| `session_id`  | tool events                                   |

A field with no value is left out rather than sent as `null`.

### Events

#### `PreToolUse`

Fires before a tool executes. A blocking hook that exits non-zero stops the call.

#### `PostToolUse`

Fires after a tool returns, whether it succeeded or failed. `is_error` says which.

#### `PostModelTurn`

Fires after the model samples a response, before the tools in it run. This one does not read stdin.

| Exit code | Effect                                                              |
|-----------|---------------------------------------------------------------------|
| `0`       | Continue                                                            |
| `1`       | stderr (or stdout) is injected as a user message; the loop continues|
| `> 1`     | Same, and the query loop stops                                      |

#### `Stop`

Fires when the model finishes a turn. It runs twice: once inline, which the turn waits for, and once in the background. Either way the output is discarded and the exit code is ignored, so a `Stop` hook cannot change what happens next. The background run passes the turn's text in the `CLAUDE_HOOK_OUTPUT` environment variable rather than on stdin.

Keep it short. The inline run holds the turn open for as long as the command takes, up to the hook timeout.

#### `UserPromptSubmit`

Fires when the user submits a prompt.

#### `Notification`

Fires when MikMik raises a notification.

### Where settings hooks come from

A hook declared in a repository's `.mikmik/settings.json` runs commands from that repository. It is gated behind project trust: opening a cloned repo does not run its hooks until you approve them.

`--bare` clears the hook map entirely, so nothing runs.

---

## Plugin hooks

A plugin ships hooks in `hooks/hooks.json`, or inline in the `hooks` field of `plugin.json`. The file takes priority over the manifest field. They are registered at startup and appear in `/hooks` with `plugin:<name>` as their source.

### Shape

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write",
        "hooks": [
          {
            "command": "node $CLAUDE_PLUGIN_ROOT/format.js",
            "blocking": false,
            "timeout_ms": 10000
          }
        ]
      }
    ]
  }
}
```

The outer `{"hooks": …}` wrapper is optional; the events map alone is accepted. A `description` beside it is kept for display.

| Field        | Level   | Description                                                       |
|--------------|---------|-------------------------------------------------------------------|
| `matcher`    | matcher | Pattern compared against the event's matchable field              |
| `matcher`    | hook    | Same, per hook                                                    |
| `command`    | hook    | The shell command. Receives the event JSON on stdin               |
| `blocking`   | hook    | `true` makes a non-zero exit deny the operation. Default `false`  |
| `timeout_ms` | hook    | Time limit in milliseconds. Default 30000. `timeout` is an alias  |

### Environment

| Variable              | Value                          |
|-----------------------|--------------------------------|
| `CLAUDE_PLUGIN_ROOT`  | The plugin's directory         |
| `CLAUDE_PLUGIN_NAME`  | The plugin's name              |

The plugin's own configuration is exported as environment variables too.

### Payload

The event's fields, plus `event` naming the event. For `PreToolUse`:

```json
{
  "event": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "rm -rf /tmp/foo" }
}
```

### Events

Every event below is raised by the running code, with one exception: `TeammateIdle` is accepted and stored, but nothing raises it yet, so a hook attached to it never runs.

#### `PreToolUse`

Fires before any tool executes. The matcher is compared against `tool_name`.

**Payload:** `tool_name`, `tool_input`.

#### `PostToolUse`

Fires after a tool completes successfully. The matcher is compared against `tool_name`.

**Payload:** `tool_name`, `tool_input`, `tool_output`, `is_error`.

#### `PostToolUseFailure`

Fires after a tool errors. The matcher is compared against `tool_name`.

**Payload:** the same four fields; `tool_output` holds the error text.

#### `Stop`

Fires right before the model concludes its response for a turn.

#### `StopFailure`

Fires when the turn ends on an API error instead of a normal stop.

#### `UserPromptSubmit`

Fires when the user submits input.

#### `Notification`

Fires when MikMik sends a notification. The matcher is compared against the notification type.

#### `Setup`

Fires once at startup, before `SessionStart`, so a plugin can prepare itself before a session is under way.

**Payload:** `working_dir`.

#### `SessionStart`

Fires when a session begins.

**Payload:** `working_dir`, `session_id`.

#### `SessionEnd`

Fires when a session is ending. The matcher is compared against the reason.

#### `SubagentStart`

Fires when an agent tool call starts a subagent. The matcher is compared against the agent type.

#### `SubagentStop`

Fires right before a subagent concludes its response.

#### `PreCompact`

Fires before a compaction. Exit code 2 blocks it.

#### `PostCompact`

Fires after compaction completes.

#### `PermissionRequest`

Fires when a permission dialog is shown. The matcher is compared against `tool_name`.

#### `PermissionDenied`

Fires when the user denies a permission request. The matcher is compared against `tool_name`.

**Payload:** `tool_name`.

#### `TaskCreated`

Fires when a task is created.

#### `TaskCompleted`

Fires when a task is marked complete.

#### `Elicitation`

Fires when an MCP server requests user input. The matcher is compared against the server name.

#### `ElicitationResult`

Fires after the user responds to an MCP elicitation.

#### `ConfigChange`

Fires when a configuration file changes during a session.

#### `WorktreeCreate`

Fires when a git worktree is being created. This is the hook that lets a worktree be a Docker container, a virtual machine, or any other directory-backed isolation.

**Payload:** `name`, the suggested worktree slug.

**Output:** stdout must be the absolute path of the created directory.

#### `WorktreeRemove`

Fires when a worktree created that way is removed.

**Payload:** `worktree_path`.

#### `InstructionsLoaded`

Fires after the session's instruction files are loaded.

**Payload:** `working_dir`, `has_instructions`.

#### `CwdChanged`

Fires after the working directory changes.

**Payload:** `working_dir`, the new directory.

#### `FileChanged`

Fires when a tool writes a file. The matcher is compared against the path.

**Payload:** `file_path` and `change`. `change` is `created` or `written` for `Write`, and `edited` for `Edit`, which also sends `replacements`.

#### `TeammateIdle`

Declared and accepted, but nothing raises it yet.

### Disabling

A plugin hook runs whenever its plugin is enabled. There is no setting that suppresses hooks by source, so disable the plugin (`/plugin disable <name>`) when you do not want its hooks.

---

## /hooks command

Run `/hooks` inside an active session to open the interactive hooks configuration menu.

The menu displays all registered hooks grouped by event, showing the source (user settings, project settings, local settings, or plugin) for each.

From this menu you can:
- View which hooks are active for each event.
- Add, edit, or remove hooks from editable settings sources.
- Inspect the metadata for any event.

Changes made through `/hooks` are written immediately to the appropriate settings file.

---

## Example hooks

### Log every tool call to a file

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "command": "jq -c '{ts: now | todate, event: .event, tool: .tool_name, input: .tool_input}' >> ~/.config/mikmik/tool.log"
      }
    ]
  }
}
```

The hook is not blocking and writes nothing to stdout, so it observes without changing anything.

### Block dangerous shell patterns

Create `~/.config/mikmik/hooks/guard.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // ""')

DANGEROUS_PATTERNS=(
  'rm -rf /'
  'dd if=.*of=/dev/'
  'mkfs\.'
  ':(){:|:&};:'
)

for pattern in "${DANGEROUS_PATTERNS[@]}"; do
  if echo "$CMD" | grep -qP "$pattern"; then
    echo "Blocked: command matches dangerous pattern '$pattern'" >&2
    exit 2
  fi
done
```

Register it:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "command": "bash ~/.config/mikmik/hooks/guard.sh",
        "tool_filter": "Bash",
        "blocking": true
      }
    ]
  }
}
```

`blocking` is what makes the non-zero exit stop the call. Without it the exit code is ignored. The stderr message reaches the model, which typically reconsiders.

### Auto-format on file write

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "command": "bash -c 'FILE=$(jq -r .tool_input.file_path); case \"$FILE\" in *.ts|*.tsx|*.js|*.jsx|*.json|*.css|*.md) prettier --write \"$FILE\" 2>/dev/null ;; *.py) ruff format \"$FILE\" 2>/dev/null ;; *.rs) rustfmt \"$FILE\" 2>/dev/null ;; esac'",
        "tool_filter": "Write"
      }
    ]
  }
}
```

Suppress the formatter's own output: any stdout would be read as a replacement for the tool input.

MikMik also runs a configured formatter after every write on its own, without a hook. See [Configuration](configuration).

### Notify when a turn ends

```bash
#!/usr/bin/env bash
# ~/.config/mikmik/hooks/notify.sh
curl -s -X POST "$SLACK_WEBHOOK_URL" \
  -H 'Content-Type: application/json' \
  -d "{\"text\": \"MikMik finished: ${CLAUDE_HOOK_OUTPUT:0:200}\"}"
```

```json
{
  "hooks": {
    "Stop": [
      { "command": "bash ~/.config/mikmik/hooks/notify.sh" }
    ]
  }
}
```

`Stop` hooks read the turn's text from `CLAUDE_HOOK_OUTPUT`, not from stdin.

---

## Testing hooks

The simplest test writes the incoming JSON to a file:

```bash
#!/usr/bin/env bash
cat > /tmp/last-hook-input.json
```

Register it as a `PreToolUse` hook for the tool you want to observe. After the next tool call, read `/tmp/last-hook-input.json` to confirm the payload shape.

To test a blocking hook without a live session, pipe a sample payload in:

```bash
echo '{"event":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}' \
  | bash ~/.config/mikmik/hooks/guard.sh
echo "Exit: $?"
```

Exit 0 means the hook would allow the call, non-zero that a `blocking` hook would stop it.
