#!/usr/bin/env bash
# SessionStart hook: prime the agent with Beads workflow context (`bd prime --hook-json`).
#
# `bd` may be installed under nvm, and agent lifecycle hooks do not always
# inherit the nvm shims on PATH. This wrapper self-locates `bd` without any
# machine-local absolute path, and degrades to a silent no-op if `bd` cannot be
# found (it must never fail the session).

# 1. Already on PATH? (hook inherited an nvm-active shell)
if ! command -v bd >/dev/null 2>&1; then
  # 2. Activate nvm's default node, which puts its bin (with `bd`) on PATH.
  export NVM_DIR="${NVM_DIR:-$HOME/.nvm}"
  if [ -s "$NVM_DIR/nvm.sh" ]; then
    . "$NVM_DIR/nvm.sh" >/dev/null 2>&1
    nvm use --silent default >/dev/null 2>&1 || nvm use --silent node >/dev/null 2>&1
  fi
fi

# 3. Last resort: prepend any nvm-installed node bin that actually contains `bd`.
if ! command -v bd >/dev/null 2>&1; then
  for d in "$HOME"/.nvm/versions/node/*/bin; do
    [ -x "$d/bd" ] && PATH="$d:$PATH" && break
  done
fi

command -v bd >/dev/null 2>&1 && bd prime --hook-json
exit 0
