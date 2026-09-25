#!/usr/bin/env bash
# Extract one task from an implementation plan into a brief file the
# implementer reads in one call, so the task text never passes through the
# coordinator's context. The brief = the plan preamble (everything before the
# first task heading — Origin, Goal, Out of scope, Constraints, which every
# task implicitly includes per plan-format.md) plus the requested task's
# section. Matches plan-format.md headings: "### Task N: <name>" at any
# heading depth; a task section ends at the next heading of any kind
# (plan-format tasks contain no sub-headings).
#
# Usage: task-brief.sh PLAN_FILE TASK_NUMBER WORKSPACE_DIR
# Writes: WORKSPACE_DIR/task-<N>-brief.md
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "usage: task-brief.sh PLAN_FILE TASK_NUMBER WORKSPACE_DIR" >&2
  exit 2
fi

plan=$1
n=$2
dir=$3
[ -f "$plan" ] || { echo "no such plan file: $plan" >&2; exit 2; }
case "$n" in
  ''|*[!0-9]*|0*) echo "TASK_NUMBER must be a positive integer: '$n'" >&2; exit 2 ;;
esac
mkdir -p "$dir"
out="$dir/task-${n}-brief.md"

awk -v n="$n" '
  /^(```|~~~)/ { infence = !infence; if (intask || !seentask) print; next }
  infence      { if (intask || !seentask) print; next }
  /^#+[ \t]/ {
    if ($0 ~ /^#+[ \t]+Task[ \t]+[0-9]+:/) {
      seentask = 1
      intask = ($0 ~ ("^#+[ \t]+Task[ \t]+" n ":"))
    } else {
      intask = 0
    }
  }
  { if (intask || !seentask) print }
' "$plan" > "$out"

if ! grep -Eq "^#+[[:space:]]+Task[[:space:]]+${n}:" "$out"; then
  rm -f "$out"
  echo "task ${n} not found in ${plan} (no heading matching 'Task ${n}:')" >&2
  exit 3
fi

echo "wrote ${out}: $(wc -l < "$out" | tr -d ' ') lines"
