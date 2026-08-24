# Agents and Multi-Agent Features

MikMik has a named-agent system that lets you select a pre-configured persona with its own tool permissions, model, system prompt, and turn budget. For larger tasks the model spawns sub-agents that work in parallel, and managed agents put a manager and its executors on separate models and budgets.

---

## Built-in Named Agents

Three agents ship by default. Their definitions can be overridden per-user in `~/.config/mikmik/settings.json`.

### build

Full tool access. Intended for implementing features and fixing bugs.

| Property      | Value                               |
|---------------|-------------------------------------|
| Access        | `full` — all tools available        |
| Max turns     | Unlimited (uses the global default) |
| Display color | Cyan                                |

Default system prompt prefix:

> You are the build agent. You have full access to read, write, and execute. Focus on implementing the requested changes completely and correctly.

### plan

Read-only analysis. Cannot write files or execute commands. Intended for understanding codebases and planning changes before committing to implementation.

| Property      | Value                                                  |
|---------------|--------------------------------------------------------|
| Access        | `read-only` — file reads, no writes or shell execution |
| Max turns     | 20                                                     |
| Display color | Yellow                                                 |

Default system prompt prefix:

> You are the plan agent. You can read and search but cannot write files or run commands. Read the code before you describe a change to it, and never plan from a guess. Use AskUserQuestion whenever the request leaves a choice open, and ask before you write the plan. State every assumption you could not resolve. When the plan is ready, call ExitPlanMode with the whole plan as the summary, and wait for the user's answer before starting any work.

### explore

Fast search-only exploration. Intended for quickly locating relevant code and answering questions about structure.

| Property      | Value                             |
|---------------|-----------------------------------|
| Access        | `search-only` — search tools only |
| Max turns     | 15                                |
| Display color | Green                             |

Default system prompt prefix:

> You are the explore agent. You can search and read files. Focus on quickly finding relevant code and answering questions about the codebase.

---

## Selecting an Agent with --agent

Pass `--agent <name>` to activate a named agent for a session:

```
mikmik --agent build "implement the OAuth2 login flow"
mikmik --agent plan "analyze the database schema and suggest improvements"
mikmik --agent explore "find all usages of the deprecated config API"
```

The `--agent` flag can be combined with `--provider` and `--model`:

```
mikmik --agent plan --provider openai --model o3 "review this architecture"
```

---

## The /agents Command

`/agents` opens the agents view. It also takes subcommands:

```
/agents list
/agents create <name>
/agents edit <name>
/agents delete <name>
```

Definitions live in `.mikmik/agents/` and in `settings.json`. Agents with
`visible: false` are left out of the listing.

---

## Custom Agent Definitions

Define custom agents in `~/.config/mikmik/settings.json` under the `agents` key. Custom definitions override built-in agents of the same name.

```json
{
  "agents": {
    "review": {
      "description": "Senior code reviewer focused on correctness and security",
      "model": "anthropic/claude-opus-4-6",
      "temperature": 0.3,
      "prompt": "You are a senior software engineer performing code review. Focus on correctness, security vulnerabilities, performance issues, and maintainability. Be specific about problems and suggest concrete fixes.",
      "access": "read-only",
      "visible": true,
      "max_turns": 30,
      "color": "magenta"
    },
    "test-writer": {
      "description": "Writes comprehensive unit and integration tests",
      "model": "anthropic/claude-sonnet-4-6",
      "prompt": "You are a test engineer. Write thorough tests covering happy paths, edge cases, and error conditions. Use the project's existing test framework and conventions.",
      "access": "full",
      "visible": true,
      "max_turns": null,
      "color": "blue"
    },
    "docs": {
      "description": "Technical documentation writer",
      "model": "anthropic/claude-sonnet-4-6",
      "temperature": 0.5,
      "prompt": "You are a technical writer. Write clear, accurate documentation for the code you are given. Use the project's existing documentation style.",
      "access": "read-only",
      "visible": true,
      "max_turns": 25,
      "color": "cyan"
    }
  }
}
```

### AgentDefinition Fields

| Field         | Type           | Description                                                                                                                            |
|---------------|----------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `description` | string         | Short description shown in `/agents`                                                                                                   |
| `model`       | string         | Model override in `provider/model` or bare `model` form. Omit to use the session default.                                              |
| `temperature` | number         | Sampling temperature override (0.0–1.0). Omit to use the model default.                                                                |
| `prompt`      | string         | System prompt prefix prepended before the main system prompt.                                                                          |
| `access`      | string         | Permission restriction: `"full"` (all tools), `"read-only"` (no writes/shell), `"search-only"` (search tools only). Default: `"full"`. |
| `visible`     | bool           | Whether to show in `/agents` output. Default: `true`.                                                                                  |
| `max_turns`   | number or null | Maximum agentic turns. Null means unlimited. Overrides the global turn budget.                                                         |
| `color`       | string         | ANSI terminal color for display: `"cyan"`, `"magenta"`, `"green"`, `"yellow"`, `"blue"`, etc.                                          |

Use the agent with the `--agent` flag:

```
mikmik --agent review "check the authentication module for security issues"
mikmik --agent test-writer "write tests for the payment processor"
```

---

## Running work in parallel

Sub-agents are spawned by the model, with the `Agent` tool, not by a mode you
switch on. Each one gets a fresh context, its own turn limit, and reports back a
single result. `isolation: "worktree"` gives an agent its own git worktree, which
is what stops two agents writing over each other.

| Tool          | Purpose                                          |
|---------------|--------------------------------------------------|
| `Agent`       | Run a sub-agent on a task of its own             |
| `SendMessage` | Send a message to a running agent                |
| `TeamCreate`  | Create a named team of agents on a shared task   |
| `TeamDelete`  | Dismantle a team                                 |
| `TaskCreate`  | Create a tracked background task                 |
| `TaskGet`     | Read one task's details                          |
| `TaskUpdate`  | Change a task's status or metadata               |
| `TaskList`    | List the active tasks                            |
| `TaskStop`    | Cancel a running task                            |
| `TaskOutput`  | Read what a task produced                        |

An agent never receives the `Agent` tool, so it cannot spawn further agents.

`/tasks` asks the model to list the tasks and their status.

For a manager and executors on different models, with their own turn budgets,
concurrency limits and budget splits, see
[Managed agents](#managed-agents-preview) below.

---

## Agent Definitions in settings.json: Complete Example

```json
{
  "provider": "anthropic",
  "agents": {
    "build": {
      "description": "Full-access implementation agent",
      "model": "anthropic/claude-sonnet-4-6",
      "prompt": "You are the build agent. Implement requested changes completely. Prefer targeted, minimal edits over rewrites.",
      "access": "full",
      "visible": true,
      "max_turns": null,
      "color": "cyan"
    },
    "plan": {
      "description": "Read-only analysis and planning agent",
      "model": "anthropic/claude-opus-4-6",
      "temperature": 0.2,
      "prompt": "You are the plan agent. Analyse the codebase carefully before producing a detailed, step-by-step implementation plan. Do not write or execute anything.",
      "access": "read-only",
      "visible": true,
      "max_turns": 20,
      "color": "yellow"
    },
    "explore": {
      "description": "Fast search-only exploration agent",
      "model": "anthropic/claude-haiku-4-5-20251001",
      "prompt": "You are the explore agent. Search and read files to answer questions quickly.",
      "access": "search-only",
      "visible": true,
      "max_turns": 15,
      "color": "green"
    },
    "security": {
      "description": "Security-focused read-only audit agent",
      "model": "anthropic/claude-opus-4-6",
      "temperature": 0.1,
      "prompt": "You are a security auditor. Look for authentication flaws, injection vulnerabilities, insecure dependencies, and data-exposure risks. Report findings with severity levels (critical, high, medium, low) and concrete remediation steps.",
      "access": "read-only",
      "visible": true,
      "max_turns": 40,
      "color": "magenta"
    },
    "architect": {
      "description": "System design and architecture advisor",
      "model": "anthropic/claude-opus-4-6",
      "temperature": 0.4,
      "prompt": "You are a software architect. Reason carefully about system design trade-offs, scalability, maintainability, and technical debt. Produce clear architectural recommendations with rationale.",
      "access": "read-only",
      "visible": true,
      "max_turns": 30,
      "color": "blue"
    }
  }
}
```

---

## Managed Agents (Preview)

Managed agents provide a formal **manager-executor** architecture. The manager reasons about the plan and delegates; the executors carry out individual tasks. You configure a model and a turn limit for each role, a concurrency limit, and one budget the whole session draws from.

The manager does not execute tools itself. Executors run concurrently up to the `concurrent` limit, and `isolation` gives each one its own worktree.

### Enabling and configuring

```
/managed-agents presets                               — list presets
/managed-agents preset <name>                         — apply a preset
/managed-agents configure manager-model  anthropic/claude-opus-4-6
/managed-agents configure executor-model anthropic/claude-sonnet-4-6
/managed-agents configure concurrent     3
/managed-agents budget 5.00
/managed-agents enable
```

See [Managed Agents](./advanced.md#managed-agents) in the advanced guide for the full configuration reference.
