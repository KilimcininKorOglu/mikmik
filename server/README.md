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

## Development

```bash
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
MIKMIK_SERVER_SECRET=$(openssl rand -hex 32) MIKMIK_SERVER_BIND=127.0.0.1:8420 cargo run
```

This is a separate Cargo project with its own lockfile, so `cargo` in `src-rust/` or `relay/` never builds it.
