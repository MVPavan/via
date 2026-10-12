# A fresh Ubuntu 24.04 user: VIA from a GitHub release (checksum-verified)
# and every harness CLI from its official installer. See README.md here.
FROM ubuntu:24.04
ARG VIA_VERSION
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git xz-utils procps jq \
    && rm -r /var/lib/apt/lists/*
# Node 22 for the Codex and Pi npm packages.
RUN curl -fsSL https://nodejs.org/dist/v22.22.0/node-v22.22.0-linux-x64.tar.xz \
      | tar -xJ -C /usr/local --strip-components=1
USER ubuntu
WORKDIR /home/ubuntu
ENV HOME=/home/ubuntu \
    PATH=/home/ubuntu/.local/bin:/home/ubuntu/.npm-global/bin:/home/ubuntu/.opencode/bin:/usr/local/bin:/usr/bin:/bin
# OpenCode must be version 2: its default installer gives 1.x.
RUN npm config set prefix /home/ubuntu/.npm-global \
    && npm install -g @openai/codex @earendil-works/pi-coding-agent \
    && curl -fsSL https://claude.ai/install.sh | bash \
    && curl -fsSL https://opencode.ai/v2/install | bash
RUN test -n "$VIA_VERSION" && mkdir -p /tmp/via-dl && cd /tmp/via-dl \
    && base=https://github.com/MVPavan/via/releases/download/v$VIA_VERSION \
    && file=via-$VIA_VERSION-x86_64-unknown-linux-musl \
    && curl -fsSLO "$base/$file" && curl -fsSLO "$base/$file.sha256" \
    && sha256sum -c "$file.sha256" \
    && install -D -m 755 "$file" /home/ubuntu/.local/bin/via \
    && cd / && rm -r /tmp/via-dl
# The mounted Pi login needs private parents owned by this user.
RUN umask 077 && mkdir -p /home/ubuntu/.via/state/vendor/pi/agent /home/ubuntu/work
