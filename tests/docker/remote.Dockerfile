FROM node:22-bookworm-slim

ARG CLAUDE_CODE_VERSION=2.1.319

RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-server ca-certificates git procps netcat-openbsd sshfs fuse3 bubblewrap \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd

RUN npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && claude --version

RUN useradd --create-home --shell /bin/bash remote
COPY --chown=remote:remote tests/docker/remote/claude.json /home/remote/.claude.json

COPY tests/docker/remote/sshd.conf /etc/ssh/sshd_config.d/claudette.conf
COPY tests/docker/remote/entrypoint.sh /usr/local/bin/remote-entrypoint
RUN chmod 755 /usr/local/bin/remote-entrypoint

EXPOSE 22
HEALTHCHECK --interval=1s --timeout=2s --retries=30 CMD bash -c '</dev/tcp/127.0.0.1/22'
ENTRYPOINT ["/usr/local/bin/remote-entrypoint"]
