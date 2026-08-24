<div align="center">

<h1>MIKMIK</h1>
<h2><em>Agentic Coding for Builders who Ship</em></h2>
<img src="public/Ship.png" alt="Rustle on the ship" width="350" />

<p>
    <a href="https://github.com/KilimcininKorOglu/mikmik"><img src="https://img.shields.io/badge/Built_with-Rust-CE4D2B?style=for-the-badge&logo=rust&logoColor=white" alt="Built with Rust"></a>
    <a href="https://github.com/KilimcininKorOglu/mikmik"><img src="https://img.shields.io/badge/Version-0.1.7-2E8B57?style=for-the-badge" alt="Version 0.1.7"></a>
    <a href="https://github.com/KilimcininKorOglu/mikmik/blob/main/LICENSE.md"><img src="https://img.shields.io/badge/License-GPL--3.0-blue?style=for-the-badge" alt="GPL-3.0 License"></a>
</p>

<br />

<img src="public/screenshot.png" alt="MIKMIK in action" width="1080" />
</div>

---

MikMik is an **open-source, multi-provider terminal coding agent** built from the ground up in Rust. It started as a clean-room reimplementation of Claude Code's behavior (from [spec](https://github.com/KilimcininKorOglu/mikmik/tree/main/spec)) and has since evolved into an amazing TUI pair programmer with multi-provider support, a rich UI, plugin system, a companion named Rustle, chat forking, memory consolidation, and much more.

It's fast, it's memory-efficient, it's yours to run however you want, and there's no tracking or telemetry.

---

> [!IMPORTANT]
> **MikMik is now officially in Beta (v0.1.7).** The core agent, multi-provider routing, and TUI are stable enough for daily driving — expect rough edges around experimental features (flagged below). Bug reports and PRs welcome.

> [!NOTE]
> **Recent Updates:**
>
> - **/share support:** Use `/share` to share chat sessions with others via unlisted GitHub Gists. `[EXPERIMENTAL]`
>
> - **Free Mode:** Try out Free in '/connect' to get a great agentic coding experience in MikMik for absolutely free (or as good as free gets you :P). `[EXPERIMENTAL]` 
>
> - **/goal support:** Try out `/goal <objective>` to see mikmik keep working an objective, spanning multiple turns instead of stopping after one normal turn. `[EXPERIMENTAL]`
>
> - **Remote control:** Drive a running session from your phone or another browser through a relay you host yourself (`relay/`, one `docker compose up`). The CLI dials out and long-polls, so your machine needs no inbound port and no firewall change. Start it with `/remote-control`; see [docs/remote-control.md](docs/remote-control.md). `[EXPERIMENTAL]`
>
> - **ultracode:** The **highest effort level** — pick it in the effort selector (`/effort`, where it sits past `max` on the "Smarter" end with an animated purple spectrum) or just type **`ultracode`** anywhere in your prompt. The keyword lights up with a purple gradient (mikmik's take on Claude Code's `ultrathink`) and that turn runs at the model's top reasoning **plus** a disciplined plan → delegate → integrate → verify workflow that fans bounded packets out across native subagents (`Agent`), swarms (`TeamCreate`), and background tasks (`TaskCreate`). Composes with `/goal` for sustained multi-turn objectives. `[EXPERIMENTAL]`

---

# Getting Started

## Quick install (one-liner)

**Linux / macOS:**

```bash
curl -fsSL https://github.com/KilimcininKorOglu/mikmik/releases/latest/download/install.sh | bash
```

**Windows (PowerShell):**

```powershell
irm https://github.com/KilimcininKorOglu/mikmik/releases/latest/download/install.ps1 | iex
```

This drops `mikmik` into `~/.local/bin` (or `%LOCALAPPDATA%\Programs\mikmik` on Windows; Git Bash uses that same Windows location) and adds it to your `PATH` automatically. Open a new terminal and run `mikmik`.

## Via npm / bun

If you have Node.js or Bun installed, you can install MikMik as a global package. The postinstall script automatically downloads the right pre-built binary for your platform.

```bash
# npm
npm install -g mikmik

# bun
bun install -g mikmik

# or run without installing
npx mikmik
bunx mikmik
```

To upgrade later, run:

```bash
mikmik upgrade
```

> Pin a specific version with `--version 0.1.0` on either installer, or `mikmik upgrade --version 0.1.0`.

## Manual download

If you'd rather grab the binary yourself, the latest archives are on [**GitHub Releases**](https://github.com/KilimcininKorOglu/mikmik/releases):

| Platform                | Archive                        |
|-------------------------|--------------------------------|
| **Windows** x86_64      | `mikmik-windows-x86_64.zip`   |
| **Linux** x86_64        | `mikmik-linux-x86_64.tar.gz`  |
| **Linux** aarch64       | `mikmik-linux-aarch64.tar.gz` |
| **macOS** Intel         | `mikmik-macos-x86_64.tar.gz`  |
| **macOS** Apple Silicon | `mikmik-macos-aarch64.tar.gz` |

Each archive contains a single `mikmik` (or `mikmik.exe`) binary. Extract it and put it on your `PATH`.

## Build from source

```bash
git clone https://github.com/KilimcininKorOglu/mikmik.git
cd mikmik/src-rust
cargo build --release --package mikmik

# Binary is at target/release/mikmik
```

**Raspberry Pi / systems without ALSA** (e.g. Debian Trixie, headless servers):

```bash
# Build without voice/microphone support — no libasound2-dev required
cargo build --release --package mikmik --no-default-features
```

## First run

```bash
# Set your API key (or use /connect inside MikMik to configure)
export ANTHROPIC_API_KEY=sk-ant-...

# Start MikMik
mikmik

# Or run a one-shot headless query
mikmik -p "explain this codebase"
```

MikMik stores everything it persists under one directory: `$MIKMIK_HOME` if set, otherwise `$XDG_CONFIG_HOME/mikmik` (`~/.config/mikmik`). Settings, sessions, credentials and memory all live there.

A second model can review the work: consulted by the main model when it decides to, or reading every turn on its own and interrupting when it sees a problem. See [`/advisor`](docs/commands.md#advisor).

Edits can be held to what the session actually read, so a file that changed underneath the agent, or a line it never displayed, is refused instead of silently written. See [`editGuard`](docs/configuration.md#edit-guard).

## Devcontainer setup

After cloning this repository, open it in VS Code and use Reopen in Container to start the development environment.

Prerequisites:
- Docker installed on your host machine: https://www.docker.com/products/docker-desktop/

GPG and SSH forwarding is enabled in the devcontainer, given you have it set up on your host machine. Follow [this guide](https://code.visualstudio.com/remote/advancedcontainers/sharing-git-credentials) if you need help with that.

### Devcontainer features

- Base image: `rust:1-bullseye`.
- Preinstalled build dependencies: `gnupg2`, `libasound2-dev`, `libxdo-dev`, and `pkg-config`.
- Devcontainer features enabled: `common-utils` (with `vscode` user `uid/gid 1000` and Zsh install disabled), `git`, and `docker-outside-of-docker` (`moby: false`).
- Runs as `vscode` user by default.
- Persistent Cargo caches via named volumes for `/usr/local/cargo/registry` and `/usr/local/cargo/git`.
- Binds local `.mikmik` into `/home/vscode/.mikmik` for local settings/session history access.
- Sets `GNUPGHOME=/home/vscode/.gnupg` and prepends `src-rust/target/debug` and `src-rust/target/release` to `PATH`.
- Post-create setup creates and permissions `.gnupg`, and fixes ownership for `/usr/local/cargo`.
- VS Code setting `terminal.integrated.inheritEnv` is enabled.

## Editor integration (Agent Client Protocol)

MikMik speaks the [**Agent Client Protocol (ACP)**](https://agentclientprotocol.com) — the open protocol pioneered by Zed for editor-to-agent communication. Any ACP-compatible editor (Zed, Neovim, JetBrains plugins, …) can drive MikMik as a subprocess and present it in the editor's native chat UI.

To use MikMik as the agent in your editor, point its ACP integration at:

```
command: mikmik
args:    ["acp"]
```

**Zed example** (`~/.config/zed/settings.json`):

```jsonc
{
  "agent_servers": {
    "mikmik": {
      "command": "mikmik",
      "args": ["acp"]
    }
  }
}
```

MikMik will run in JSON-RPC 2.0 mode over stdio. It implements `initialize`, `session/new`, `session/prompt`, `session/cancel`, `session/set_mode`, `session/set_config_option` and `session/set_model`, plus the session lifecycle: `session/list`, `session/load`, `session/resume`, `session/fork` and `session/close`. It streams `session/update` notifications (text deltas, agent thinking, tool calls with their progress, results and per-file diffs, the agent's plan, the session's name) and routes every tool permission through `session/request_permission` so the editor can show a native approval dialog.

`session/new` reports the model, the account and the reasoning effort as configuration options, plus the modes the session can run in, so an editor renders native pickers for all four. Those choices apply to that session only and are never written to `settings.json`.

Every turn is written to the same session store the terminal reads, so an editor can list earlier conversations and reopen one. The slash commands are announced as available commands and run in the agent: a prompt naming one is answered by the command rather than sent to the model.

The agent uses whatever the editor offered to host. With `fs.read_text_file` it reads the buffer the user is looking at rather than the older text on disk; with `fs.write_text_file` its edits go through the editor and stay undoable; with `terminal` its commands run in the editor's shell and are attached to the tool call that started them. Each is honoured on its own, and anything the editor does not host stays with the agent.

A session can bring its own MCP servers over stdio, HTTP or SSE, connected for that session alone. A permission request carries what it is approving, not just the tool's name. Images in a prompt reach the model; audio does not, and `initialize` says so.

Configure your provider / API key before launching — run `mikmik auth login`, use `/connect` inside the TUI, or edit `settings.json` directly. The ACP agent uses the same credentials and providers as the interactive TUI.

Enable verbose ACP logging (to stderr — never stdout, which would corrupt the protocol) by setting `MIKMIK_ACP_LOG=debug`.

### VS Code

VS Code has no ACP client of its own, so this repo ships one: [`editors/vscode/`](editors/vscode/). It spawns one `mikmik acp` process for the window and gives each panel its own session inside it, renders the transcript, diffs and plan in a webview, completes slash commands and `@file` mentions, hosts the files the agent reads and writes so unsaved edits are visible and its writes stay undoable, and can reopen or fork an earlier conversation. Build it with `npm install && npm run compile` in that directory, then press F5 to open an Extension Development Host. Setup and scope are in [its README](editors/vscode/README.md).

### Listing on the ACP Registry

The [Agent Client Protocol registry](https://github.com/agentclientprotocol/registry) is the canonical directory editors look up when offering "available agents". To get MikMik listed:

1. Fork [`agentclientprotocol/registry`](https://github.com/agentclientprotocol/registry).
2. Create a `mikmik/` folder at the repo root and drop in the prepared manifest from this repo: [`src-rust/crates/acp/registry-template/agent.json`](src-rust/crates/acp/registry-template/agent.json). Bump the `version` and release-archive URLs to match the latest GitHub release.
3. Add `mikmik/icon.svg` (16×16 recommended) — the Rustle logo from [`public/`](public/) is a fine starting point.
4. Open a PR to the registry. The registry CI validates `agent.json` against [the schema](https://github.com/agentclientprotocol/registry/blob/main/agent.schema.json) before merge.

After merge, Zed and other ACP-aware editors will pick up MikMik on their next registry refresh.

## Supported providers

Native wire-format implementations, each with its own request shaping, streaming, and tool conversion:

| Provider | Notes |
|----------|-------|
| **Anthropic** | Default. API key or OAuth; multi-account supported. |
| **OpenAI** | Also the base for every OpenAI-compatible endpoint below. |
| **Google (Gemini)** | |
| **Azure OpenAI** | |
| **AWS Bedrock** | |
| **GitHub Copilot** | |
| **Codex** | OAuth; multi-account supported. |
| **Cohere** | |
| **MiniMax** | |
| **Free Mode** | Rotating free endpoints, configured through `/connect`. `[EXPERIMENTAL]` |

On top of those, MikMik ships around forty **OpenAI-compatible** endpoints — Groq, DeepSeek, Mistral, xAI, OpenRouter, Together, Perplexity, DeepInfra, Cerebras, Venice, SambaNova, Fireworks, Nebius, Moonshot, Qwen and more — plus local runtimes (**Ollama**, **LM Studio**, **llama.cpp**, and **MLX LM** on Apple Silicon) and two escape hatches, `custom-openai` and `custom-anthropic`, for anything not on the list.

Setup instructions, env vars and `settings.json` shapes are in [docs/providers.md](docs/providers.md); local runtimes have their own page in [docs/local-models.md](docs/local-models.md). The authoritative list lives in `src-rust/crates/api/src/providers/`.

## Documentation

For more info on how to configure MikMik, [head over to our docs](https://kilimcininkoroglu.github.io/mikmik/docs).

| Page | Covers |
|------|--------|
| [installation.md](docs/installation.md) | Every install path and upgrading |
| [auth.md](docs/auth.md) | API keys, OAuth, multiple accounts |
| [providers.md](docs/providers.md) · [local-models.md](docs/local-models.md) | Provider setup; Ollama / LM Studio / llama.cpp |
| [commands.md](docs/commands.md) · [keybindings.md](docs/keybindings.md) | Slash commands and key bindings |
| [configuration.md](docs/configuration.md) | `settings.json` reference |
| [tools.md](docs/tools.md) · [agents.md](docs/agents.md) | Built-in tools; subagents and teams |
| [mcp.md](docs/mcp.md) · [plugins.md](docs/plugins.md) · [hooks.md](docs/hooks.md) | Extending MikMik |
| [remote-control.md](docs/remote-control.md) | Driving a session from your phone |
| [workspace-server.md](docs/workspace-server.md) | A company's own server: accounts, providers, policy, backups |
| [advanced.md](docs/advanced.md) | Everything else |

>**PS:** The original breakdown of the findings from Claude Code's source that started this project is on [my blog](https://kuber.studio/blog/AI/Claude-Code's-Entire-Source-Code-Got-Leaked-via-a-Sourcemap-in-npm,-Let's-Talk-About-it) - the full technical writeup of what was found, how the leak happened, and what it revealed.

---

## Contributing

MikMik is built for the community, by the community and we'd love your help making it better.

Before opening a PR, from `src-rust/`:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1
cargo fmt --all
```

If you touched `relay/`, run its own `cargo test -- --test-threads=1` and `cargo clippy --all-targets -- -D warnings` from that directory; it is a separate Cargo project with its own lockfile.

[Open an issue](https://github.com/KilimcininKorOglu/mikmik/issues/new) for bugs, ideas, or questions, or [Raise a PR](https://github.com/KilimcininKorOglu/mikmik/pulls/new) to fix bugs, add features, or improve documentation.

---

## Important Notice

This repository does not hold a copy of the proprietary Claude Code TypeScript source code.
This is a **clean-room Rust reimplementation** of Claude Code's behavior.

The process was explicitly two-phase:

**Specification** [`spec/`](https://github.com/KilimcininKorOglu/mikmik/tree/main/spec) — An AI agent analyzed the source and produced exhaustive behavioral specifications and improvements, deviated from the original: architecture, data flows, tool contracts, system designs. No source code was carried forward.

**Implementation** [`src-rust/`](https://github.com/KilimcininKorOglu/mikmik/tree/main/src-rust) — A separate AI agent implemented from the spec alone, never referencing the original TypeScript. The output is idiomatic Rust that reproduces the behavior, not the expression.

This mirrors the legal precedent established by Phoenix Technologies v. IBM (1984) — clean-room engineering of the BIOS — and the principle from Baker v. Selden (1879) that copyright protects expression, not ideas or behavior.

---

