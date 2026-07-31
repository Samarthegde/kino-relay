# ── Build ─────────────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Prime the dependency cache with a stub main, so editing src/ doesn't force a
# rebuild of every crate in the tree.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Cargo skips relinking if the stub's mtime looks newer than the real source.
RUN touch src/main.rs && cargo build --release

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# The relay terminates no TLS and its only outbound call is enrollment with
# kino-control, whose root certificates are compiled in (webpki-roots) - so it
# needs nothing beyond libc. curl earns its keep as the container healthcheck.
# ca-certificates: enrollment verifies kino-control against the OS trust store,
# so the store has to exist in the image. Mount extra CAs into
# /usr/local/share/ca-certificates and run update-ca-certificates if your
# control plane sits behind a private or intercepting CA.
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Optional extra trust anchor, for networks that inspect TLS (a corporate
# FortiGate/Zscaler/etc re-signing certificates) or a control plane behind a
# private CA. Drop the PEM next to this Dockerfile as `extra-ca.crt` and it is
# trusted; the [.] glob makes the file optional, so builds work without it.
COPY extra-ca[.]crt /usr/local/share/ca-certificates/
RUN update-ca-certificates

RUN useradd --system --create-home --shell /usr/sbin/nologin kino \
    && mkdir -p /data && chown kino:kino /data
USER kino

COPY --from=builder /build/target/release/kino-relay /usr/local/bin/kino-relay

ENV PORT=3000
ENV RUST_LOG=info
# Where the kino-control public key received at enrollment is persisted. Mount
# a volume here, or a container restart loses it and tries to re-enroll with an
# already-consumed code.
ENV RELAY_JWT_PUBLIC_KEY=/data/control.pub.pem
WORKDIR /data
VOLUME ["/data"]
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${PORT}/healthz" || exit 1

ENTRYPOINT ["/usr/local/bin/kino-relay"]
