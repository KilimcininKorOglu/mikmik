# Remote Control

Drive a mikmik session from your phone or another browser, through a relay you host yourself.

The CLI dials out and long-polls. Your machine needs no inbound port, no port forward and no firewall change. The relay only queues and forwards; it never runs anything.

```
phone/web  ──HTTP+SSE──►  relay (Docker)  ◄──long-poll──  mikmik (your machine)
```

---

## Read this before you start

The relay token is a remote command-execution credential. Anything holding it can send a prompt into a running session, and that session runs tools on your machine.

Three consequences, all enforced in code rather than left to you:

- The token must be at least 32 characters. The relay refuses to start below that, and mikmik refuses to connect.
- The relay does not terminate TLS. Put a TLS-terminating reverse proxy in front of it, or reach it only over a VPN or LAN. Without TLS the token and your source travel in plaintext.
- `docker-compose.yml` publishes on `127.0.0.1` for that reason. Changing it to `0.0.0.0` without TLS in front puts the token on the wire.

The relay keeps everything in memory and writes nothing to disk, so the relay host never holds a durable copy of your code. It does see the transcript in transit; there is no end-to-end encryption.

---

## 1. Run the relay

The relay lives in `relay/` in this repository. It is a standalone Cargo project, not part of the `src-rust` workspace.

```bash
cd relay
cp .env.example .env
openssl rand -hex 32          # paste the output into RELAY_TOKEN
docker compose up -d
```

Check it is up:

```bash
curl http://127.0.0.1:8350/healthz     # -> ok
```

| Variable                 | Default              | Meaning                                             |
|--------------------------|----------------------|-----------------------------------------------------|
| `RELAY_TOKEN`            | none, required       | Shared secret; at least 32 characters               |
| `RELAY_BIND`             | `0.0.0.0:8350`       | Listen address inside the container                 |
| `RELAY_SESSION_TTL_SECS` | `900`                | Drop a session after this long without a poll       |
| `RELAY_EVENT_BUFFER`     | `500`                | Events retained per session for replay              |
| `RELAY_INBOUND_QUEUE`    | `100`                | Messages queued for a session before the oldest goes |
| `RUST_LOG`               | `mikmik_relay=info` | Log filter                                          |

---

## 2. Point mikmik at it

In your user settings file (`~/.config/mikmik/settings.json`, or wherever `MIKMIK_HOME` points):

```json
{
  "remoteControl": {
    "url": "https://relay.example",
    "token": "the same token you put in RELAY_TOKEN",
    "label": "workstation"
  }
}
```

`label` is what the session list shows. Without it the hostname is used.

This block is read from the user settings file only. A project settings file cannot set it, because a repository should not be able to point your machine's bridge at a relay.

For a temporary redirect while developing, `MIKMIK_BRIDGE_URL` and `MIKMIK_BRIDGE_TOKEN` override the settings file.

---

## 3. Enable the bridge

```
/remote-control start
```

Restart mikmik. The bridge connects on launch. `/remote-control` with no argument shows which relay it resolved, where each value came from, and whether the token is usable.

`/remote-control stop` disables it again.

---

## 4. Open the relay

Point a browser at the relay address and enter the token. Three views:

- **Token entry** — once per browser. The token goes into an `HttpOnly` cookie, so the page cannot read it back.
- **Session list** — every connected machine, most recently active first, with its label and working directory.
- **Session screen** — the live transcript, a prompt box, a stop button, and the cards for anything the session is waiting on.

On opening a session you get the conversation so far, not just what happens next: the newest 40 turns are sent on connect, and the client says how many earlier ones were left out. Tool output is there too, folded into each tool row and opened on demand; a failed tool opens on its own. Extended thinking renders separately from the answer, and a "Working…" line shows while a turn runs.

The layout starts at phone width and adapts upward, so a phone, a tablet and a desktop browser all get a usable screen. Session cards fill a grid once there is room for it, and the transcript stops widening past a readable measure instead of running the width of a monitor.

---

## Permissions

Whether a tool asks for approval is decided entirely by the local session, before the relay is involved at all. That is `config.permission_mode`:

| Mode                                        | Does a tool ask?                                    |
|---------------------------------------------|-----------------------------------------------------|
| `default`                                   | Yes, by the tool's danger level                     |
| `acceptEdits`                               | Yes, except `Edit`, which is allowed outright       |
| `plan`                                      | No. Reads are allowed, writes are refused           |
| `bypassPermissions` (`--dangerously-skip-permissions`) | No. Everything is allowed outright        |

Once a tool does ask, the request appears in the terminal and on the remote client, and either side may answer. There is no separate remote permission policy: the token is the boundary, and it already gates prompting.

The card offers three answers:

| Button              | Effect                                                                 |
|---------------------|------------------------------------------------------------------------|
| Allow once          | This call only.                                                        |
| Allow this session  | This tool for the rest of the session. Nothing is written to settings. |
| Deny                | Refuse the call.                                                       |

A remote tap never writes a permanent rule into your settings file. Persistent allows are a keyboard-only decision.

### The bypass warning

Switching into `bypassPermissions` raises a warning that stops everything until it is answered: no turn starts, no tool runs, and no other prompt is shown. It is raised however the mode was reached, whether from `--dangerously-skip-permissions` at startup, `/yolo`, the settings screen, or a plugin.

The remote client sees it as its own card, louder than a tool approval and with two answers rather than three, because what it grants is every later tool call rather than one of them. Either side may answer, and a remote answer takes the same path as a keyboard answer:

| Button                            | Effect                                                                        |
|-----------------------------------|-------------------------------------------------------------------------------|
| Yes, I accept                     | The session runs without asking permission. The answer is remembered, so the warning is not raised again |
| No, exit (at startup)             | The session ends                                                              |
| No, keep asking (mid-session)     | The mode in force beforehand is put back, in the live session and on disk      |

A client that connects while the warning is up is sent it as part of the session snapshot. Without that, a session that is waiting on it looks idle and stays that way until someone answers at the terminal.

Two consequences worth stating plainly:

- The remote client is a security boundary. Anyone holding your unlocked phone can approve a tool call on your machine.
- Sending a prompt is not a permission at all. Anything holding the relay token can start a turn regardless of the mode. Under `bypassPermissions` that means the token alone runs arbitrary tools with no approval step anywhere.

`/remote-control` reports which of these situations the session is in.

---

## What you can do from a remote client

| Action | Notes |
|---|---|
| Send a prompt | Enter sends, Shift+Enter inserts a newline |
| Attach files | Images become something the model can look at; text is folded into the prompt. 5 MB per prompt |
| Run a slash command | Takes the same route as one typed at the keyboard, so `/compact`, `/clear` and `/model <id>` behave identically. Commands that open a picker still render on the terminal |
| Answer a permission request | Either side may answer |
| Answer the bypass warning | Either side may answer. Accepting turns off every later permission prompt for the session |
| Answer a question | Either side may answer |
| Stop the current turn | Same as Ctrl+C at the keyboard |

A command sent while a turn is running is queued and runs when the turn ends, exactly as a message typed at the keyboard during a turn.

---

## The execution timeline

When `timelineEnabled` is on (see [Configuration](configuration.md#interface)), the browser gets the same step-by-step panel the terminal draws: every tool call and finished turn, with its status, how long it took and what the turn spent. It sits folded above the transcript; tap the header to open it, and tap a row to read its detail.

The rows arrive as `timeline_row` events carrying the row the terminal built, timings included. A row is sent when it opens and again whenever it changes, and `row.id` says which one, so a client replaces the row it already holds rather than appending a second copy.

The timings are the machine's, not the browser's. A long poll can hold a batch of events for its whole interval, so a client that stamped its own arrival times would report transport delay as step duration.

A client that attaches mid-session gets the recent rows in the same backfill as the transcript, after the `history` event that clears them. The backfill is bounded at 40 rows; older steps stay on the terminal only.

Each `tool_end` event also carries `duration_ms`, how long that one call took, and the web client prints it beside the tool's name. The field is absent for a call that was blocked or cancelled before it ran, so a client should treat a missing value as "no time to report" rather than as zero. It travels whatever the terminal's own `showToolDuration` setting says: that setting decides what the terminal draws, and a remote client is a front end of its own.

---

## Questions

The model can call `AskUserQuestion` to ask you something mid-turn. That also blocks the turn, and it can be answered from either side.

The card shows the question, the model's suggested answers as buttons, and a free-text field for anything else. Submitting an empty answer counts as dismissing it, which the model sees as "the user dismissed the question without answering".

Whichever side answers first wins; the other side's card stops accepting input for that question.

---

## What survives a restart

Nothing on the relay. Sessions, queues and buffers are in memory, so a relay restart drops them and the CLI re-registers on its next poll.

Each session keeps a bounded ring buffer of recent events. A browser that lost its connection reconnects with the last sequence number it saw and resumes from there. Once events fall out of the buffer they are gone; the terminal transcript remains complete.

A session whose runner stops polling is swept after `RELAY_SESSION_TTL_SECS`.

---

## Building a different client

The relay speaks two separate protocols on purpose.

The **runner surface** (`/api/claude_code/sessions/...`) is fixed by what the CLI already calls and cannot change.

The **client surface** is ours, and a native app should use it:

| Method | Path                                           | Notes                                       |
|--------|------------------------------------------------|---------------------------------------------|
| `POST` | `/api/client/auth`                             | Sets the cookie                             |
| `GET`  | `/api/client/sessions`                         | Open sessions, most recently active first   |
| `GET`  | `/api/client/sessions/{id}/stream?since=<seq>` | SSE; resumes from the ring buffer           |
| `POST` | `/api/client/sessions/{id}/prompt`             | `{"content", "attachments"}`; 5 MB total    |
| `POST` | `/api/client/sessions/{id}/permission`         | `{"request_id", "tool_use_id", "decision"}` |
| `POST` | `/api/client/sessions/{id}/mcp-approval`       | `{"request_id", "decision"}`                |
| `POST` | `/api/client/sessions/{id}/bypass`             | `{"request_id", "accept"}`                  |
| `POST` | `/api/client/sessions/{id}/answer`             | `{"question_id", "answer"}`                 |
| `POST` | `/api/client/sessions/{id}/rename`             | Rename the session                          |
| `POST` | `/api/client/sessions/{id}/cancel`             | Body optional                               |

Authentication accepts a bearer token or the cookie. A native client should use the bearer token; the cookie exists because a browser `EventSource` cannot set request headers.

`GET /healthz` needs no token.

---

## Troubleshooting

**The session list is empty.** The CLI has not registered. Check `/remote-control` in the terminal: it prints the relay it resolved and whether the token is usable. A token under 32 characters stops the bridge from starting at all, and says so.

**The stream reconnects in a loop.** The session was swept for going quiet, or the relay restarted. Go back to the session list and reopen it.

**A prompt is accepted but nothing happens.** The session is waiting on a permission request, a question, or the bypass warning. The card appears at the bottom of the session screen; answer it there or at the terminal.

**Nothing loads over HTTPS.** The relay does not terminate TLS. The reverse proxy in front of it must, and should set `X-Forwarded-Proto: https` so the session cookie is marked `Secure`.
