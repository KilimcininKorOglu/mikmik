# Workspace server

A self-hosted configuration and identity server for an organisation's MikMik
installations. One organisation runs one of these. It holds the user accounts,
the provider definitions and keys the organisation hands out, the settings
policy it enforces, and each user's own settings backup.

A developer signs in once. The company's providers arrive on their machine,
the company's policy applies to every session, and their own settings are
backed up so a rebuilt machine comes back with everything on it.

```
admin ──web UI──►  server  ◄──login, pull, sync──  mikmik (each developer)
                     │
                     └── SQLite: accounts, providers, policy, backups, audit
```

The server lives in `server/` in this repository. It is a separate Cargo
project with its own lockfile; `server/README.md` covers running it.

---

## For the developer

### Signing in

```bash
mikmik workspace login https://mikmik.firma.com --email ayse@firma.com
```

The password is read from stdin, never from an argument: the shell records its
history, and `ps` shows every argument to every user on the machine. Pipe it in
to keep it off the screen as well:

```bash
pass show work/mikmik | mikmik workspace login https://mikmik.firma.com --email ayse@firma.com
```

The address must be `https`, unless the host is `localhost`, `127.0.0.1` or
`[::1]`. Signing in sends a password, and the answer carries every provider key
the organisation assigned you.

Signing in writes two things:

- `workspace.url` in your global `settings.json`, which holds no credential;
- a session token in `auth.json`, which is written `0o600` and carries the
  address it was issued for, so it can never be sent to another server.

It then takes the providers you are entitled to and the settings policy.

### What arrives

The company's providers become ordinary accounts, and `mikmik accounts` lists
them beside your own. Each one carries `managedBy` naming the server it came
from, which decides three things:

- it stays out of your settings backup, because the server already holds it;
- `mikmik workspace logout` removes exactly these and leaves your own alone;
- `/workspace` lists them apart, so you do not edit an entry the next pull
  overwrites.

An account you configured yourself is never replaced, even when the company
offers one by the same name. Yours is kept and the clash is reported; rename
yours to take the company's.

An entitlement the organisation withdraws takes its key with it on the next
pull.

The company can also hand out a web-search key. It is not an account: it has no
`settings.json` entry and does not show in `mikmik accounts`. Its key lands in
`auth.json` under the provider id the search tool reads (`tavily`, `brave`,
`exa`, ...), marked with the server it came from. `/workspace` lists these
under "Company search providers", `mikmik workspace logout` drops them, and a
withdrawn one goes on the next pull, the same as any other entitlement. A
search key you entered yourself, or one in an environment variable, is left
alone; the company's key never overwrites it.

### The policy

The organisation's policy is the last settings layer, so whatever it names,
neither you nor a repository can override it. `/workspace` lists the keys it
decides.

A policy may not name anything the client would run: `hooks`, `mcpServers`,
`formatter`, `lspServers`, `skills`, `acpAgents`, `remoteControl` or
`workspace`. The server refuses such a policy when an administrator writes it,
and the client refuses it again when it applies one. Neither check depends on
the other holding.

The policy is cached on disk. A session opens when the server is unreachable,
and it opens with the organisation's rules rather than without them.

### The backup

Your settings are backed up to the server, and `mikmik workspace restore`
brings them back on a new machine.

What goes up is this machine's configuration minus everything that belongs
somewhere else:

| Left out                           | Why                                                             |
|------------------------------------|-----------------------------------------------------------------|
| The company's providers            | The server already holds them, and a restored copy would outlive the entitlement. |
| `config.workspace_paths`           | Names this filesystem.                                           |
| `config.additional_dirs`           | Names this filesystem.                                           |
| `config.project_dir`               | Names this filesystem.                                           |
| `remoteControl.token`              | Authorises driving this machine. The url stays.                  |
| The workspace session itself       | This machine's, and it expires.                                  |

Your own provider keys **do** go up. A backup that restores provider
definitions without their keys restores nothing usable on the day the machine
is rebuilt, which is the day it is wanted. The server seals the blob at rest;
see [What the server can read](#what-the-server-can-read).

When it uploads is up to you:

| Trigger           | Default | Setting                    |
|-------------------|---------|----------------------------|
| After a change    | on      | `workspace.sync.onChange`  |
| On a timer        | off     | `workspace.sync.intervalMinutes` |
| At session start  | on      | `workspace.sync.pullAtStartup`   |
| By hand           | —       | `/workspace sync` or `mikmik workspace sync` |

The change trigger waits for the writes to stop, so an editor save is one
upload rather than several. A timer shorter than five minutes is raised to
five.

Two machines syncing one account is the normal case. A write against a version
that has moved on is refused, nothing is stored, and you are told: only the
person using both machines can say which settings are right.

### Restoring

```bash
mikmik workspace restore
```

A settings backup can carry a hook, a formatter, a language server or a skill
source. Restoring one unasked would run whatever the server was holding, so
those are listed verbatim and nothing runs until you accept them. Declining
restores everything else.

The answer is filed against that exact set. A backup edited afterwards asks
again, and an answer given to one organisation does not cover another's.

### Signing out

```bash
mikmik workspace logout
```

Ends the session on the server, forgets the token, removes the company's
providers and their keys, and drops the cached policy. Your own providers and
settings stay. The address stays in `settings.json`, so signing in again needs
only the account.

A key the company handed out is on your disk until you sign out. Removing your
entitlement on the server stops the **next** pull from handing it back; it does
not reach into a machine that already has it. Rotating the key does.

---

## For the administrator

Run the server, open the first account from the command line, and use the web
interface for everything after that. `server/README.md` has the environment
variables, the Docker setup and the API.

```bash
cp server/.env.example server/.env
openssl rand -hex 32                 # paste into MIKMIK_SERVER_SECRET
docker compose -f server/docker-compose.yml up -d
docker compose -f server/docker-compose.yml exec -T server \
  mikmik-server admin create ayse@firma.com --admin
```

Then open the server's address in a browser.

### TLS is not optional

The server does not terminate TLS. Put a TLS-terminating reverse proxy in front
of it, or reach it only over a VPN. Without TLS the passwords and the API keys
travel in plaintext, and the client refuses a plain `http` address to anything
but a loopback host for exactly that reason.

`docker-compose.yml` publishes on `127.0.0.1` by default so that a server put
up for a first look is not put up for everybody.

### What the server can read

`MIKMIK_SERVER_SECRET` encrypts every stored provider key and every settings
backup, and derives every session token. Treat it as the key to everything the
database holds. Changing it makes every stored key and backup unreadable.

The server can read what it holds. That is the decision this design was built
around: it hands out provider keys, so it has to be able to read them. What the
encryption buys is that a copied `.sqlite` file, a stray backup or a disk image
is not enough on its own.

### The audit log

Every action that changes something, and every attempt to reach something that
needs a credential, leaves a row: who acted, what they did, and what they did it
to. It never holds a password, an API key, a policy body or a settings backup.

A failed audit write fails the request. A log that silently drops entries reads
as a complete record.

---

## What this is not

- **A gateway.** The server does not sit in the request path. It hands out
  provider definitions and keys, and the installation talks to the vendor
  directly. The `providers` table and its `api_base` field do not stand in the
  way of a gateway later.
- **Multi-tenant.** One installation serves one organisation.
- **SSO or LDAP.** Email and password.
- **Client-side encryption under the user's password.** The decision was that
  the server can read the keys it hands out.
