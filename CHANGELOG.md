# Changelog

All notable changes to kino-relay are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-07-31

### Added
- **Token authentication on every WebSocket endpoint.** `/ws/control`,
  `/ws/manager/request`, and `/ws/agent/data` now require an
  `Authorization: Bearer <token>` header (a header, not a query parameter, so
  tokens stay out of proxy access logs). Two independent mechanisms, either of
  which admits a caller:
  - `RELAY_TOKEN` - one static shared secret, compared in constant time. The
    simple option for a personal relay.
  - `RELAY_JWT_PUBLIC_KEY` - path to an Ed25519 **public** key PEM, verifying
    JWTs minted by a [kino-control](https://github.com/Samarthegde/kino-control)
    instance. Tokens carry a role (`agent`/`manager`) and an `agent_id` scope,
    so a manager token for one host cannot register agents or reach another.
    Verification is asymmetric on purpose: the relay holds no signing material,
    so a relay operator can verify tokens but never mint them.
- **Self-enrollment with kino-control.** A relay can register itself in a
  control plane's directory so agents discover it automatically, either from
  the environment (`KINO_CONTROL_URL`, `KINO_ENROLL_CODE`, `RELAY_PUBLIC_URL`,
  optional `RELAY_NAME`) or by answering an interactive prompt when started in
  a terminal with no auth configured. The relay starts serving first (control
  probes `/healthz` before accepting), then registers, saves the received
  public key, and **begins enforcing token auth without a restart**. Restarts
  reuse the saved key and skip enrollment, so the one-time code is never needed
  twice; a rejected code is logged loudly but leaves the relay running.

### Changed
- **License: MIT - AGPL-3.0.** Running a modified relay as a network service
  now requires publishing those modifications. Commercial licensing is
  available from the author.
- With no auth configured the relay still runs open, but now says so much more
  loudly at startup, and the README documents the risk instead of carrying a
  "not a security boundary" warning.

## [0.1.0] - 2026-07-13

### Added
- Initial release: a stateless WebSocket relay that splices a Kino SSH Manager
  connection to a kino-agent parked behind NAT or a firewall, forwarding
  already-encrypted bytes without terminating SSH.
- Control / manager-request / agent-data endpoints with session pairing,
  30-second keepalive pings on the control channel, and stale-connection
  eviction that can't unseat a newer registration for the same `agent_id`.
- Optional direct TLS (`TLS_CERT` / `TLS_KEY`, ALPN pinned to HTTP/1.1) for
  running without a reverse proxy, plus graceful `SIGTERM`/`SIGINT` shutdown.
- Docker image, compose file, and a correct nginx reverse-proxy config.
