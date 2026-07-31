# kino-relay

[![build](https://github.com/Samarthegde/kino-relay/actions/workflows/build.yml/badge.svg)](https://github.com/Samarthegde/kino-relay/actions/workflows/build.yml)
[![license: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)

A small, stateless WebSocket relay that lets [Kino SSH Manager](https://github.com/Samarthegde/kino-ssh-manager)
reach machines running [kino-agent](https://github.com/Samarthegde/kino-agent)
that have **no inbound port open** - behind NAT, CGNAT, or a firewall.

The relay is the one publicly reachable piece. Agents dial *out* to it and park a
connection; the manager asks it to open a session; the relay splices the two
together and forwards bytes. It holds no state on disk, terminates no SSH, and
never sees your SSH credentials - it only moves already-encrypted bytes.

---

## How it works

```
  ┌─────────────────────┐          ┌───────────────┐          ┌──────────────────────┐
  │  Kino SSH Manager   │   wss    │  kino-relay   │   wss    │      kino-agent      │
  │    (desktop app)    │ ───────► │   (this)      │ ◄─────── │   (on target host)   │
  └─────────────────────┘          └───────────────┘  control └──────────┬───────────┘
                                                                          │ tcp 127.0.0.1:22
                                                                          ▼  sshd
```

The relay exposes three WebSocket endpoints plus a health check:

| Route | Who connects | Purpose |
|-------|--------------|---------|
| `GET /healthz` | anyone / monitors | Liveness probe, returns `ok`. |
| `GET /ws/control?agent_id=<id>` | agent | Persistent channel. The agent registers here and waits for `new_connection` messages. Kept alive with 30s pings. |
| `GET /ws/manager/request?agent_id=<id>` | manager | The manager asks to reach an agent. The relay notifies the agent and waits (up to 10s) for it to dial back. |
| `GET /ws/agent/data?session_id=<id>` | agent | The agent's return data socket for one session. The relay hands it to the waiting manager and bridges the two. |

Each agent is identified by a caller-chosen `agent_id`; each session by a
relay-generated `session_id`. When auth is configured, every `/ws/*` endpoint
also requires an `Authorization: Bearer` header - see
[Authentication](#authentication).

---

## Quick start (local)

```bash
cargo run --release
# Relay listening on 0.0.0.0:3000 (plaintext - terminate TLS at your proxy)
```

Point an agent at it (use `ws://` for plaintext local testing):

```bash
kino-agent --relay-url ws://localhost:3000 --agent-id test
```

---

## Deployment

The relay speaks plain HTTP by default and expects **TLS to be terminated in
front of it** (nginx, Caddy, a cloud LB) so clients can use `wss://`. It can also
terminate TLS itself - see [Direct TLS](#direct-tls-no-proxy) below.

### Docker Compose (behind your own TLS proxy)

```bash
cp .env.example .env    # optional: set LISTEN_ADDR / RUST_LOG
docker compose up -d --build
```

This publishes the relay on port `3000`. Front it with a TLS-terminating reverse
proxy. **WebSocket upgrade headers are mandatory** - without them every
connection fails with `400 Bad Request`. A ready-to-use, correct nginx server
block is in [`deploy/nginx-kino-relay.conf`](deploy/nginx-kino-relay.conf); the
essential parts:

```nginx
# at http{} level, once:
map $http_upgrade $connection_upgrade { default upgrade; '' close; }

location / {
    proxy_pass http://127.0.0.1:3000;
    proxy_http_version 1.1;
    proxy_set_header Upgrade    $http_upgrade;      # required for WebSockets
    proxy_set_header Connection $connection_upgrade;
    proxy_read_timeout 1h;                          # don't cut idle SSH sessions
    proxy_buffering off;
}
```

> `proxy_read_timeout` defaults to 60s, which would kill an idle SSH terminal
> after a minute. The relay pings the *control* channel, but a data socket has no
> such keepalive - raise the timeout.

### Direct TLS (no proxy)

Set both `TLS_CERT` and `TLS_KEY` and the relay serves `wss://` itself (and
refuses plaintext). ALPN is pinned to HTTP/1.1, which is what WebSockets require.

```bash
TLS_CERT=/path/fullchain.pem TLS_KEY=/path/privkey.pem PORT=443 cargo run --release
```

### Configuration

| Env var | Default | Description |
|---------|---------|-------------|
| `PORT` | `3000` | Port to listen on (what most PaaS hosts inject). |
| `BIND` | `0.0.0.0:$PORT` | Full listen address; overrides `PORT`. |
| `TLS_CERT` / `TLS_KEY` | unset | PEM cert chain + private key. Set both to serve `wss://` directly. |
| `RELAY_TOKEN` | unset | Static bearer token; see [Authentication](#authentication). |
| `RELAY_JWT_PUBLIC_KEY` | `control.pub.pem` when enrolling | Path to kino-control's Ed25519 public key PEM - loaded if present, written on self-enrollment. |
| `KINO_CONTROL_URL` / `KINO_ENROLL_CODE` / `RELAY_PUBLIC_URL` | unset | Set all three to self-enroll with kino-control at startup; see [Enrolling](#enrolling-in-kino-control-self-enrollment). |
| `RELAY_NAME` | unset | Optional display name used when enrolling. |
| `RUST_LOG` | `info` | Log verbosity. |

The relay handles `SIGTERM`/`SIGINT` gracefully, so `docker stop` and systemd
restarts don't cut sessions mid-frame.

### Where to host it

The relay is tiny and stateless, but it holds **long-lived WebSocket
connections**, so it must run somewhere that doesn't idle-sleep or cap connection
duration. An always-on VM (e.g. Oracle Cloud's Always Free tier, which is ARM -
the aarch64 build applies) is a good fit; serverless/idle-sleeping tiers are not.

---

## Installing

Prebuilt Linux binaries (x86_64 and aarch64) are attached to every
[release](https://github.com/Samarthegde/kino-relay/releases) - no toolchain
needed:

```bash
curl -fsSL -o kino-relay \
  https://github.com/Samarthegde/kino-relay/releases/latest/download/kino-relay-x86_64-unknown-linux-gnu
chmod +x kino-relay && sudo mv kino-relay /usr/local/bin/
```

Or use the Docker image (see [Deployment](#deployment)).

## Building from source

Requires stable Rust (edition 2024, i.e. Rust ≥ 1.85):

```bash
cargo build --release   # binary at target/release/kino-relay
```

TLS uses [rustls](https://github.com/rustls/rustls) (`ring` backend) - no OpenSSL
system dependency.

---

## Project layout

```
src/main.rs        The whole relay: routing, auth, self-enrollment, control/manager/data handlers, bridging
Dockerfile         Multi-stage, non-root, healthchecked image
docker-compose.yml Relay service (plaintext, for use behind a proxy)
deploy/nginx-kino-relay.conf   Correct nginx reverse-proxy config
.env.example       Sample environment
.github/workflows/build.yml    CI matrix + release publishing
CHANGELOG.md       Release history
```

---

## Authentication

All three WebSocket endpoints require an `Authorization: Bearer <token>` header
when auth is configured (a header, not a query parameter, so tokens stay out of
proxy access logs). Two mechanisms, independently optional - a caller passes if
it satisfies either:

| Env var | Mechanism |
|---------|-----------|
| `RELAY_TOKEN` | One static shared secret, compared in constant time. The simple option for a personal relay: set it here, in `kino-agent --token`, and in the host's *Relay token* field in the manager. |
| `RELAY_JWT_PUBLIC_KEY` | Path to an Ed25519 public key PEM. Verifies JWTs minted by a [kino-control](https://github.com/Samarthegde/kino-control) instance. Tokens carry a role (`agent`/`manager`) and an `agent_id` scope, so a manager token for host A cannot register agents or reach host B. |

Verification is asymmetric on purpose: the relay holds no signing material, so
a relay operator can verify tokens but never mint them - the property that
makes a federated pool of community-run relays possible later.

> **With neither variable set the relay runs open** (and says so loudly at
> startup): anyone who can reach it and knows an `agent_id` can open an SSH
> transport to that agent's local `sshd`, whose own authentication becomes the
> only gate. Fine for a firewalled lab; set a token for anything public.

SSH remains end-to-end encrypted through the relay either way - a relay
operator cannot read sessions or credentials.

### Enrolling in kino-control (self-enrollment)

A relay can join a kino-control instance's directory, so agents discover it
automatically (and park on whichever enrolled relay answers fastest). The
relay enrolls **itself** at startup - two ways:

**Interactive** - run the relay in a terminal with no auth configured, and it
asks:

```
No auth is configured (RELAY_TOKEN / RELAY_JWT_PUBLIC_KEY unset).
Enroll this relay with a kino-control instance? [y/N]
```

Answer the prompts (control URL, one-time code from the kino-control web UI,
this relay's public URL) and it handles the rest.

**Env-driven** (docker/systemd - no TTY, no questions):

```bash
KINO_CONTROL_URL=https://control.example.com \
KINO_ENROLL_CODE=<one-time code> \
RELAY_PUBLIC_URL=wss://relay.example.com \
RELAY_NAME=eu-1 \
  kino-relay
```

Either way the relay starts serving first (kino-control probes `/healthz`
before accepting), then registers, receives kino-control's public key,
**saves it** (`RELAY_JWT_PUBLIC_KEY` path, default `control.pub.pem`), and
starts enforcing token auth immediately - no restart. Restarts load the saved
key and skip enrollment, so the one-time code isn't needed again. From then on
kino-control health-checks this relay every minute.

Roadmap:

- [x] Token auth on all three WebSocket endpoints.
- [x] Per-agent scoped tokens (via kino-control JWTs) so an `agent_id` can't be hijacked.
- [x] Relay registration & presence with kino-control (agents discover relays and park on the fastest).
- [ ] Optional metrics/observability endpoint.

Found a vulnerability? Report it privately - see
[CONTRIBUTING.md](CONTRIBUTING.md).

---

## Contributing

Contributions are welcome - see **[CONTRIBUTING.md](CONTRIBUTING.md)**.

## License

[GNU AGPL-3.0](LICENSE) © 2026 Samarth Kombemane

AGPL on purpose: if you run a modified relay as a service, you must share your
changes. For commercial licensing, contact the author.
