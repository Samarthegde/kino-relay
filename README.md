# kino-relay

[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

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
relay-generated `session_id`.

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
| `RUST_LOG` | `info` | Log verbosity. |

The relay handles `SIGTERM`/`SIGINT` gracefully, so `docker stop` and systemd
restarts don't cut sessions mid-frame.

### Where to host it

The relay is tiny and stateless, but it holds **long-lived WebSocket
connections**, so it must run somewhere that doesn't idle-sleep or cap connection
duration. An always-on VM (e.g. Oracle Cloud's Always Free tier, which is ARM -
the aarch64 build applies) is a good fit; serverless/idle-sleeping tiers are not.

---

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
src/main.rs        The whole relay: routing, control/manager/data handlers, bridging
Dockerfile         Multi-stage, non-root, healthchecked image
docker-compose.yml Relay service (plaintext, for use behind a proxy)
deploy/nginx-kino-relay.conf   Correct nginx reverse-proxy config
.env.example       Sample environment
```

---

## Security & roadmap

> **⚠️ The relay currently has no authentication.** Anyone who can reach it and
> knows (or guesses) an `agent_id` can open an SSH transport to that agent's local
> `sshd`, and anyone can register an `agent_id`. Your `sshd`'s own authentication
> is the only gate. **Do not treat the relay as a security boundary yet.**

SSH remains end-to-end encrypted through the relay, so a relay operator cannot
read sessions or credentials - but the lack of relay-level auth is the top
priority on the roadmap:

- [ ] **Shared-secret / token auth** on all three WebSocket endpoints.
- [ ] Per-agent connection tokens so an `agent_id` can't be hijacked.
- [ ] Optional metrics/observability endpoint.

If you want to help, the auth handshake is a great first substantial
contribution. Found a vulnerability? Report it privately - see
[CONTRIBUTING.md](CONTRIBUTING.md).

---

## Contributing

Contributions are welcome - see **[CONTRIBUTING.md](CONTRIBUTING.md)**.

## License

[MIT](LICENSE) © 2026 Samarth Kombemane
