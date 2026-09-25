#!/bin/bash
# PreToolUse(Write|Edit) guard. Blocks hand-edits to bd-GENERATED tracking mirrors.
#
# The per-workstream tracking trio (docs/workstreams/*/tracking/{status,checklist,progress}.md) and the
# cross-cutting board (docs/workstreams/{status,ideas,backlog}.md) are read-only projections of Beads.
# The sanctioned writer is a project renderer run from Bash with BD_RENDER=1. Update bd
# (create / claim / close --reason), then render. Never hand-edit these generated mirrors.
#
# Hand-authored files in the same tree (roadmap.md, README.md, plans/*.md) are NOT blocked.
set -euo pipefail

INPUT=$(cat)

if command -v jq >/dev/null 2>&1; then
  FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')
else
  FILE_PATH=$(echo "$INPUT" | grep -o '"file_path":"[^"]*"' | sed 's/"file_path":"//;s/"$//')
fi

[ -z "${FILE_PATH:-}" ] && exit 0

# Generated paths: the three board files directly under docs/workstreams/, or anything under */tracking/.
if echo "$FILE_PATH" | grep -qE 'docs/workstreams/(.+/tracking/[^/]+\.md|(status|ideas|backlog)\.md)$' 2>/dev/null; then
  echo "BLOCKED: '$FILE_PATH' is a bd-generated workstream mirror. Update bd, then regenerate it with the project renderer. Do not hand-edit generated mirrors." >&2
  exit 2
fi

exit 0
