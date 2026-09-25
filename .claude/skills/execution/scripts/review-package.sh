#!/usr/bin/env bash
# Package the work under review from the WORKING TREE — implementers do not
# commit (conservative git), so commit-range diffs would be empty. A package
# holds: git status, the tracked diff vs SCOPE_BASE (staged or not), and the
# full content of untracked files. Every run also snapshots the packaged
# files, so a fix round can be diffed against the previous review without
# any commits existing.
#
# Usage:
#   review-package.sh full SCOPE_BASE WORKSPACE LABEL [PATH...]
#   review-package.sh fix  SCOPE_BASE PREV_LABEL WORKSPACE LABEL [PATH...]
# Writes WORKSPACE/review-LABEL.diff and WORKSPACE/snap-LABEL/.
set -euo pipefail

mode=${1:-}
case "$mode" in
  full)
    [ $# -ge 4 ] || { echo "usage: review-package.sh full SCOPE_BASE WORKSPACE LABEL [PATH...]" >&2; exit 2; }
    base=$2; dir=$3; label=$4; shift 4 ;;
  fix)
    [ $# -ge 5 ] || { echo "usage: review-package.sh fix SCOPE_BASE PREV_LABEL WORKSPACE LABEL [PATH...]" >&2; exit 2; }
    base=$2; prev=$3; dir=$4; label=$5; shift 5 ;;
  *)
    echo "usage: review-package.sh full|fix ..." >&2; exit 2 ;;
esac

git rev-parse --verify --quiet "$base" >/dev/null || { echo "bad SCOPE_BASE: $base" >&2; exit 2; }
mkdir -p "$dir"
out="$dir/review-${label}.diff"
snap="$dir/snap-${label}"
rm -rf "$snap"
mkdir -p "$snap"

mapfile -t changed < <(git diff --name-only "$base" -- "$@")
mapfile -t untracked < <(git ls-files --others --exclude-standard -- "$@")

for f in ${changed[@]+"${changed[@]}"} ${untracked[@]+"${untracked[@]}"}; do
  [ -f "$f" ] || continue
  case "$f" in "$dir"/*) continue ;; esac   # never snapshot the workspace itself
  mkdir -p "$snap/$(dirname "$f")"
  cp "$f" "$snap/$f"
done

if [ "$mode" = full ]; then
  {
    echo "# Review package (${label}): worktree vs ${base}"
    echo
    echo "## Status"
    git status --short -- "$@"
    echo
    echo "## Files changed vs base"
    git diff --stat "$base" -- "$@"
    echo
    echo "## Diff (tracked)"
    git diff -U10 "$base" -- "$@"
    if [ ${#untracked[@]} -gt 0 ]; then
      echo
      echo "## Untracked files (full content)"
      for f in "${untracked[@]}"; do
        [ -f "$f" ] || continue
        case "$f" in "$dir"/*) continue ;; esac
        echo
        echo "### ${f}"
        echo '```'
        cat "$f"
        echo '```'
      done
    fi
  } > "$out"
else
  prevsnap="$dir/snap-${prev}"
  [ -d "$prevsnap" ] || { echo "no previous snapshot: $prevsnap" >&2; exit 3; }
  {
    echo "# Fix package (${label}): changes since review ${prev}"
    echo
    echo "## Diff vs previous snapshot"
    git diff --no-index -U10 "$prevsnap" "$snap" || true
  } > "$out"
fi

echo "wrote ${out}: $(wc -c < "$out" | tr -d ' ') bytes; snapshot ${snap}"
