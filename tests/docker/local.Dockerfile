FROM node:22-bookworm-slim AS t3-download
ARG T3_VERSION=0.1.7
RUN npm install -g "t3@${T3_VERSION}" && T3CODE_HOME=/opt/t3 t3 --version

FROM node:22-bookworm-slim AS paseo-download
ARG PASEO_VERSION=0.15.3
RUN npm install -g --prefix /opt/paseo "@getpaseo/cli@${PASEO_VERSION}" && /opt/paseo/bin/paseo --version

FROM rust:1-slim-bookworm AS rust-build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --release --locked && install -m 755 target/release/claudette /claudette

FROM rust-build AS unit
RUN apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target cargo test --locked

FROM oven/bun:1.4.4-debian AS local
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-client git procps ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY tests/package.json tests/bun.lock* ./
RUN bun install --frozen-lockfile --omit optional \
 && ! find / -xdev -name claude -type f 2>/dev/null | grep -q .

COPY tests/ ./
COPY --from=rust-build /claudette /usr/local/bin/claudette

COPY tests/docker/local/entrypoint.sh /usr/local/bin/local-entrypoint
RUN chmod 755 /usr/local/bin/local-entrypoint && chown -R bun:bun /app
USER bun
ENTRYPOINT ["/usr/local/bin/local-entrypoint"]

FROM local AS t3
ARG T3_VERSION=0.1.7
USER root
COPY --from=t3-download /opt/t3/runtime /opt/t3/runtime
RUN chmod -R a+rX /opt/t3 && ln -s "/opt/t3/runtime/versions/${T3_VERSION}/t3" /usr/local/bin/t3 && t3 --version
USER bun

FROM local AS paseo
USER root
COPY --from=paseo-download /usr/local/bin/node /usr/local/bin/node
COPY --from=paseo-download /opt/paseo /opt/paseo
RUN ln -s /opt/paseo/bin/paseo /usr/local/bin/paseo && paseo --version
USER bun
