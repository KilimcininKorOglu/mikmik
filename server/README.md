# mikmik-server

A self-hosted configuration and identity server for an organisation's mikmik installations.

One organisation runs one of these. It holds the user accounts, the provider definitions and keys the organisation hands out, the settings policy it enforces, and each user's own settings backup. A mikmik installation logs in against it and receives what that user is entitled to.

```
admin ──web UI──►  server (Docker)  ◄──login, pull, sync──  mikmik (each developer)
                        │
                        └── SQLite: accounts, providers, policy, backups, audit
```

## Before you run it

`MIKMIK_SERVER_SECRET` encrypts every stored provider key and every settings backup, and derives every session token. Treat it as the key to everything the database holds.

- The server does not terminate TLS. Put a TLS-terminating reverse proxy in front of it, or reach it only over a VPN. Without TLS the passwords and API keys travel in plaintext.
- `docker-compose.yml` publishes on `127.0.0.1` by default for that reason.
- The secret must be at least 32 characters. The server refuses to start below that rather than running with a weak one.
- Back up the volume, not just the image. The accounts live in `/data`.

## Running

```bash
cp .env.example .env
openssl rand -hex 32          # paste into MIKMIK_SERVER_SECRET
docker compose up -d
```

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `MIKMIK_SERVER_SECRET` | none, required | Encryption and session key, 32 characters or more |
| `MIKMIK_SERVER_BIND` | `0.0.0.0:8420` | Listen address |
| `MIKMIK_SERVER_DB` | `mikmik-server.sqlite` | Database path; the image sets `/data/mikmik-server.sqlite` |
| `MIKMIK_SERVER_SESSION_TTL_SECS` | `2592000` (30 days) | How long a login lasts |
| `RUST_LOG` | `mikmik_server=info` | Log filter |

## Opening the first account

The web interface needs an account to log in with, so it cannot open the first
one. The binary does, against the same database:

```bash
echo 'a long enough password' | mikmik-server admin create ayse@firma.com --admin
mikmik-server admin list
```

The password is read from stdin and never from the command line, because the
shell records its history and `ps` shows arguments to every user on the
machine. A password must be at least 12 characters.

In the container:

```bash
docker compose exec -T server mikmik-server admin create ayse@firma.com --admin
```

## Logging in

```
POST /api/v1/login   {"email": "...", "password": "..."}  answers a token
GET  /api/v1/me      Bearer <token>                       answers the account
POST /api/v1/logout  Bearer <token>                       ends the session
```

The token also comes back as a `mikmik_session` cookie, marked `HttpOnly` and
`SameSite=Strict`, and `Secure` when a reverse proxy reports TLS. A guarded
route accepts either.

A failed login always answers the same 401, whether the password was wrong, the
address unknown, or the account disabled. Telling them apart would make this a
way to find out who works here.

## Providers, groups and assignment

An organisation defines each provider once, with its key, and decides who may
use it. A user reaches a provider either because it is assigned to them
directly or because it is assigned to a group they belong to.

```
POST   /api/v1/admin/providers          define a provider (name, protocol, api_base, api_key, models)
GET    /api/v1/admin/providers          list them, with who they are assigned to, never with a key
DELETE /api/v1/admin/providers/{id}     remove one; its assignments go with it
POST   /api/v1/admin/groups             create a group
GET    /api/v1/admin/groups             list them
DELETE /api/v1/admin/groups/{id}        remove one; its memberships go with it
POST   /api/v1/admin/memberships        {"user_id": ..., "group_id": ...}
POST   /api/v1/admin/memberships/remove the same body
POST   /api/v1/admin/assignments        {"provider_id": ..., "subject_kind": "user"|"group", "subject_id": ...}
POST   /api/v1/admin/assignments/remove the same body
GET    /api/v1/admin/users              list the accounts
POST   /api/v1/admin/users              {"email": ..., "password": ..., "is_admin": false}
```

Every one of these needs an administrator. A request from an ordinary account
answers 404 rather than 403, so the administration surface does not confirm its
own existence.

A user reads their own entitlement:

```
GET /api/v1/providers   Bearer <token>   the definitions and keys they may use
```

The key is what makes the entitlement real. A provider nobody assigned is not
merely hidden from the client; the client has no credential to use it with.

Provider keys are stored encrypted with XChaCha20-Poly1305 under a key derived
from `MIKMIK_SERVER_SECRET`. The server can read them, which is what lets it
hand them out; what this buys is that a copied `.sqlite` file is not enough on
its own. Changing the secret makes every stored key unreadable.

## Settings policy

An organisation writes one policy. Every client fetches it and merges it over
its own settings, so whatever the policy names, the user cannot override.

```
PUT    /api/v1/admin/policy   the settings object; answers its checksum
GET    /api/v1/admin/policy   read it back
DELETE /api/v1/admin/policy   remove it
GET    /api/v1/policy         what a client fetches, with the checksum as ETag
```

A client sends the checksum back as `If-None-Match` and receives 304 with no
body, which is what makes an hourly poll cheap. No policy at all answers 204,
so a client can tell "nothing configured" from "unchanged".

A policy may not set any of these keys, in either spelling and at either level:

```
hooks  mcpServers  formatter  lspServers  skills  acpAgents  remoteControl  workspace
```

Each names something the client would run, fetch or connect to, so a policy
server able to set them would be a way to execute code on every machine in the
organisation. It may also make `permissionMode` stricter but not set it to
`bypassPermissions`.

The write is refused with the offending key named, rather than accepted and
dropped by the client. An administrator who is told "no" can ask why; one whose
policy is silently ignored believes it applied.

## Settings backup

Each account holds one backup. A client uploads what it has and restores it on
a new machine.

```
GET    /api/v1/settings                     the backup, its version and its checksum
PUT    /api/v1/settings   If-Match: <n>     upload, replacing version n; 0 for the first
DELETE /api/v1/settings                     remove it
```

`If-Match` is required. A write without it is a client that has not read what
is stored, and letting it through is the silent overwrite the version exists to
stop; the server answers 428 instead.

Two machines syncing one account is the normal case. A write against a version
that has moved on answers 409 with the current version in the body, and nothing
is written. The client re-reads and decides what to keep.

The stored blob is sealed, because the decision was that a user's own provider
keys ride along with their settings. Losing `MIKMIK_SERVER_SECRET` makes every
backup unreadable rather than wrong.

## Development

```bash
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
MIKMIK_SERVER_SECRET=$(openssl rand -hex 32) MIKMIK_SERVER_BIND=127.0.0.1:8420 cargo run
```

This is a separate Cargo project with its own lockfile, so `cargo` in `src-rust/` or `relay/` never builds it.
