# Contributing to kino-relay

Thanks for your interest in improving kino-relay! The relay is the one publicly
reachable component of Kino, so changes - especially to connection handling and
(eventually) authentication - get a careful review. Please don't be discouraged
by questions on a PR.

## Getting started

You need a recent stable Rust toolchain (edition 2024, i.e. **Rust ≥ 1.85**):

```bash
git clone https://github.com/Samarthegde/kino-relay
cd kino-relay
cargo run --release   # listens on 0.0.0.0:3000
```

To exercise the full path, connect an [agent](https://github.com/Samarthegde/kino-agent)
and a manager:

```bash
kino-agent --relay-url ws://localhost:3000 --agent-id dev
```

A quick handshake check without any client:

```bash
curl -i -H "Connection: Upgrade" -H "Upgrade: websocket" \
     -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" -H "Sec-WebSocket-Version: 13" \
     "http://localhost:3000/ws/control?agent_id=probe"
# expect: HTTP/1.1 101 Switching Protocols
```

## Before you open a pull request

CI treats warnings as errors, so please run:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo build --release
```

If you change the Docker or deploy setup, verify the image still builds and the
container reports healthy:

```bash
docker build -t kino-relay:dev .
docker run -d -p 3000:3000 --name kino-relay-dev kino-relay:dev
curl -fsS http://127.0.0.1:3000/healthz   # -> ok
docker rm -f kino-relay-dev
```

## Good first contributions

- **Relay authentication** (the top roadmap item - see the README). A shared-secret
  or token handshake on the three WebSocket endpoints, plus per-agent connection
  tokens so an `agent_id` can't be hijacked. Open an issue to discuss the design
  first, since it touches the wire protocol shared with the agent and manager.
- Observability (a metrics endpoint, structured connection logs).
- Tests around the manager/agent handshake and reconnection edge cases.

## Pull request guidelines

- **Keep PRs focused** - one logical change each.
- **Explain the "why"** and how you verified it. For connection-lifecycle changes,
  describe how you tested reconnect, keepalive, and graceful shutdown.
- **Coordinate protocol changes.** The relay's endpoints and message format are a
  contract shared with kino-agent and kino-ssh-manager; changing them means
  updating all three. Open an issue first.
- **Update docs** when behavior or configuration changes.

## Commit messages

Imperative mood (`Add ...`, `Fix ...`), short subject, and a body explaining the
reasoning where it isn't obvious.

## Reporting security issues

Please **do not** open a public issue for security vulnerabilities - the relay's
current lack of authentication is already documented, but for anything else email
**samarth@nanokernel.net** with details and a reproduction if possible.

## License

By contributing, you agree that your contributions will be licensed under the
[GNU AGPL-3.0](LICENSE), and you grant the project maintainer a perpetual,
worldwide, non-exclusive, royalty-free right to relicense your contribution,
including under commercial terms. (This keeps dual licensing possible without
chasing every past contributor.)
