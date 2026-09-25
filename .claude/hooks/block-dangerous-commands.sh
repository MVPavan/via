#!/bin/bash
# PreToolUse(Bash) guard. Blocks destructive commands and asks the user first.
# Covers: git working-tree/history destroyers, --no-verify, beads store re-init,
# recursive removes (rm -r / rm -rf) targeting anything outside /tmp, and
# shell writes to bd-generated workstream mirrors.
set -euo pipefail

INPUT=$(cat)

if command -v jq >/dev/null 2>&1; then
  COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')
else
  COMMAND=$(echo "$INPUT" | grep -o '"command":"[^"]*"' | sed 's/"command":"//;s/"$//')
fi

[ -z "${COMMAND:-}" ] && exit 0

# Fixed-string dangerous patterns (git working-tree / history destroyers).
DANGEROUS_PATTERNS=(
  "git push --force"
  "git push -f"
  "git reset --hard"
  "git clean -fd"
  "git clean -f"
  "git branch -D"
  "git checkout ."
  "git restore ."
  "--no-verify"
)

for pattern in "${DANGEROUS_PATTERNS[@]}"; do
  if echo "$COMMAND" | grep -qF "$pattern" 2>/dev/null; then
    echo "BLOCKED: '$COMMAND' matches dangerous pattern '$pattern'. Ask the user before proceeding." >&2
    exit 2
  fi
done

# Beads store re-initialization. `bd init --force` (deprecated alias for --reinit-local) and
# `--reinit-local` bypass bd's local data-safety guard and WIPE the Dolt store; recovery would then
# depend entirely on the committed .beads/issues.jsonl mirror. Block any `bd ... init` that carries a
# force/reinit flag (order-independent), and ask the user first.
if echo "$COMMAND" | grep -qE '\bbd\b' 2>/dev/null \
   && echo "$COMMAND" | grep -qE '\binit\b' 2>/dev/null \
   && echo "$COMMAND" | grep -qE -- '--force|--reinit' 2>/dev/null; then
  echo "BLOCKED: '$COMMAND' re-initializes the beads store (wipes the Dolt DB; recovery would depend on the committed .beads/issues.jsonl mirror). Ask the user before proceeding." >&2
  exit 2
fi

# Recursive remove outside /tmp. Any `rm -r`/`-R`/`--recursive` (with or without -f, any flag
# spelling) is destructive, so allow it only when EVERY target is under /tmp/. If a target lives
# elsewhere — or can't be proven /tmp-only (globs, quotes, command substitution, redirections) —
# block and ask the user to review. Detection is scoped per-rm-segment so an unrelated -r elsewhere
# on the line can't trip it. (Non-recursive `rm -f`/`rm <file>` is not covered — no tree deletion.)
__rm_is_recursive() { echo "$1" | grep -qE -- '(^|[[:space:]])-[A-Za-z]*[rR]' || echo "$1" | grep -qiF -- '--recursive'; }

while IFS= read -r SEG; do
  [ -z "${SEG:-}" ] && continue
  __rm_is_recursive "$SEG" || continue
  found_target=0; unsafe=0
  set -f; set -- $SEG; set +f                  # word-split the segment WITHOUT glob expansion
  for tok in "$@"; do
    [ "$tok" = "rm" ] && continue
    case "$tok" in -*) continue ;; esac         # a flag, not a target
    found_target=1
    case "$tok" in
      /tmp/*) case "$tok" in *..*) unsafe=1 ;; esac ;;   # under /tmp, but reject path-escape (..)
      *) unsafe=1 ;;                             # anything not under /tmp
    esac
  done
  if [ "$found_target" = 0 ] || [ "$unsafe" = 1 ]; then
    echo "BLOCKED: '$COMMAND' is a recursive remove (rm -r/-rf) with a target outside /tmp (or one that can't be proven /tmp-only). Review with the user before proceeding — only recursive rm under /tmp/ runs without confirmation." >&2
    exit 2
  fi
done <<EOF
$(echo "$COMMAND" | grep -oE '(^|[[:space:]])rm[[:space:]]+[^;&|<>]*' || true)
EOF

# bd-generated tracking mirrors. The per-workstream tracking trio and the cross-cutting board under
# docs/workstreams/ are read-only projections of Beads. The sanctioned writer is a project renderer
# invoked with BD_RENDER=1. Block direct shell-redirect / tee / in-place-sed writes unless that
# whitelist token is present in the command.
GEN_PATH_RE='docs/workstreams/([^|;]*/tracking/[^/[:space:]]*\.md|(status|ideas|backlog)\.md)'
if ! echo "$COMMAND" | grep -qF 'BD_RENDER=1' 2>/dev/null \
   && echo "$COMMAND" | grep -qE "(>>?|[[:space:]]tee[[:space:]]|sed[[:space:]]+-i)[^|;]*$GEN_PATH_RE" 2>/dev/null; then
  echo "BLOCKED: '$COMMAND' writes to a bd-generated workstream mirror. Update bd, then regenerate the mirror with the project renderer (run with BD_RENDER=1). Do not hand-edit generated mirrors." >&2
  exit 2
fi

exit 0
