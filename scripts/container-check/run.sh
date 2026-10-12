#!/usr/bin/env bash
# Usage: scripts/container-check/run.sh <via-version> [harness ...]
# Builds the fresh-user image for a released VIA version and runs check.sh in
# it, mounting only your Claude Code and Codex homes and VIA's Pi login.
# OpenCode needs no login. Writes container-check-<version>.log here.
set -eu
version="$1"; shift
dir=$(cd "$(dirname "$0")" && pwd)
docker build -q -t "via-check:$version" --build-arg VIA_VERSION="$version" "$dir"
docker run --rm -t \
  -e HARNESSES="${*:-claude codex opencode pi}" \
  -v "$HOME/.claude:/home/ubuntu/.claude" \
  -v "$HOME/.claude.json:/home/ubuntu/.claude.json" \
  -v "$HOME/.codex:/home/ubuntu/.codex" \
  -v "$HOME/.via/state/vendor/pi/agent:/home/ubuntu/.via/state/vendor/pi/agent" \
  -v "$dir/check.sh:/home/ubuntu/check.sh:ro" \
  "via-check:$version" bash /home/ubuntu/check.sh 2>&1 \
  | tr -d '\r' | sed 's/\x1b\[[0-9;]*m//g' | tee "$dir/container-check-$version.log"
