# Multi-stage build. Explicit beats relying on a builder's conventions: the
# first attempt used Nixpacks with a hand-written startCommand pointing at
# ./target/release/, which does not exist in the runtime image, and the service
# crash-looped.

FROM rust:1.99-slim-bookworm AS builder

WORKDIR /app

# Dependencies first, in their own layer, so editing src/ does not rebuild all
# ~370 crates. The dummy main.rs exists only to make that layer cacheable.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --bin discord_ev_bot \
    && rm -rf src

COPY src ./src
# Cargo skips rebuilding when only mtimes changed, so force it to see the real
# sources as newer than the dummy ones.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --bin discord_ev_bot

# ---------------------------------------------------------------- runtime

FROM debian:bookworm-slim

# ca-certificates is REQUIRED, not optional. rustls verifies TLS against the
# system root store, and without it every HTTPS call to api.owlsinsight.com and
# discord.com fails with a certificate error that looks like a network problem.
# This is the classic slim-image trap.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/discord_ev_bot /usr/local/bin/discord_ev_bot
# Config::load resolves config/default.toml relative to the working directory,
# so the directory has to travel with the binary.
COPY config ./config

# Run as a non-root user. Nothing here needs privileges.
RUN useradd --system --uid 10001 evbot && chown -R evbot:evbot /app
USER evbot

CMD ["discord_ev_bot"]
