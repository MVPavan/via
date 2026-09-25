#!/usr/bin/env bash
# bd-render-tracking.sh — regenerate the bd-derived tracking mirrors (read-only projections of beads).
#
# beads (bd) is the SINGLE SOURCE OF TRUTH for work-state. The files this script writes are GENERATED:
# update bd (create / claim / close --reason), then run this — never hand-edit the generated files.
#
# Renders, purely from live bd queries:
#   • the cross-cutting board at   <DOCS_ROOT>/docs/workstreams/{status,ideas,backlog}.md
#   • per-workstream trio at        <DOCS_ROOT>/<dirname(spec_id)>/tracking/{status,checklist,progress}.md
#     for every workstream whose epics' spec_id lives under docs/workstreams/.
#
# Epics are enumerated with `bd list -t epic --all` (NOT `bd epic status`, which silently drops CLOSED
# epics) so completed phases still render. Only the region between the BD:GENERATED markers is rewritten;
# anything a human adds after the END marker is preserved. A freshness stamp makes staleness the only
# failure mode (re-run to fix) — divergent hand-edits are impossible.
#
# Run with BD_RENDER=1 (the guard whitelist that lets writes land on the generated paths):
#   BD_RENDER=1 bash <beads-skill-dir>/scripts/bd-render-tracking.sh [workstream-name]
#
# Env knobs:
#   DOCS_ROOT  output base dir that repo-relative paths are joined to (default: repo root ".")
#   BD_C       value for `bd -C <dir>` — point at a sandbox store for testing (default: auto-discover)
# Positional $1 (optional): restrict the per-workstream render to one workstream (by folder basename).

set -euo pipefail

readonly START_MARK='<!-- BD:GENERATED START -->'
readonly END_MARK='<!-- BD:GENERATED END -->'
readonly PRESERVE_HINT='<!-- Human notes below this line are preserved across renders. Everything above is bd-generated; do not hand-edit it. -->'

DOCS_ROOT="${DOCS_ROOT:-.}"
WS_REL='docs/workstreams'                       # repo-relative root for workstreams (matches spec_id prefix)
ONLY_WS="${1:-}"                                 # optional single-workstream filter
NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

bd_() { bd ${BD_C:+-C "$BD_C"} "$@"; }

# write_generated <path> <body> : splice <body> between the markers, preserving any post-END human notes.
write_generated() {
  local path="$1" body="$2" tail
  mkdir -p "$(dirname "$path")"
  if [ -f "$path" ] && grep -qF "$END_MARK" "$path"; then
    tail="$(awk -v m="$END_MARK" 'seen{print} $0==m{seen=1}' "$path")"
  else
    tail="$(printf '\n%s\n' "$PRESERVE_HINT")"
  fi
  { printf '%s\n%s\n%s\n' "$START_MARK" "$body" "$END_MARK"; printf '%s\n' "$tail"; } >"$path"
  echo "  rendered  $path"
}

header() { printf '# %s\n_generated from bd @ %s — DO NOT EDIT (run: BD_RENDER=1 bash <beads-skill-dir>/scripts/bd-render-tracking.sh)_\n' "$1" "$NOW"; }

# All epics (open + closed), one per line, TSV: id <TAB> status <TAB> spec_id <TAB> title.
# Sorted by spec_id then title so phases display in roadmap order (titles carry sortable prefixes).
all_epics_tsv() { bd_ list -t epic --all --json 2>/dev/null | jq -r '.[] | [.id, .status, (.spec_id // "(no anchor)"), .title] | @tsv' | sort -t$'\t' -k3,3 -k4,4; }

# "closed total" child counts for an epic (counts dotted direct children + any descendants alike).
epic_counts() { bd_ list --parent "$1" --all --json 2>/dev/null | jq -r '"\([.[]|select(.status=="closed")]|length) \(length)"'; }

# One markdown status line for an epic.  args: <epic-id> <epic-status> <title>
epic_status_line() {
  local id="$1" st="$2" title="$3" c t mark
  read -r c t < <(epic_counts "$id")
  if   [ "$st" = "closed" ];                        then mark="  ✅ DONE"
  elif [ "$t" -gt 0 ] && [ "$c" -eq "$t" ];         then mark="  ✅ stages done (close epic)"
  elif [ "$t" -eq 0 ];                              then mark="  (no stages)"
  else                                                   mark="  ⏳"; fi
  printf -- "- **%s** — %s/%s%s\n" "$title" "$c" "$t" "$mark"
}

# direct children (stages) of an epic = id starts with "<epic>." and has no further dot.
DIRECT_FILTER='.[] | select((.id|startswith($e+".")) and ((.id|ltrimstr($e+"."))|contains(".")|not))'

# ── board: cross-cutting status / ideas / backlog ───────────────────────────────────────────────
render_board() {
  local dir="$DOCS_ROOT/$WS_REL"
  echo "board -> $dir"

  # status.md : every epic (incl. closed) grouped by workstream (spec_id) + a "ready now" queue.
  local rollup="" spec last=""
  if [ -n "$(all_epics_tsv)" ]; then
    while IFS=$'\t' read -r id st spec title; do
      [ "$spec" != "$last" ] && { rollup+=$'\n'"### $spec"$'\n'; last="$spec"; }
      rollup+="$(epic_status_line "$id" "$st" "$title")"$'\n'
    done < <(all_epics_tsv)
  else
    rollup="_no active workstreams._"
  fi
  local ready
  ready="$(bd_ ready --json 2>/dev/null | jq -r '
    ([.[] | select(.issue_type != "epic")] | sort_by(.title) | .[:20]) as $w
    | if ($w|length)==0 then "_nothing ready._" else ($w[] | "- `" + .id + "` " + .title) end')"
  write_generated "$dir/status.md" "$(header 'Workstream Status (global)')
$rollup
## Ready now (top of queue)
$ready"

  # ideas.md / backlog.md : parked label-based inbox.
  local ideas backlog
  ideas="$(bd_ list -l idea --json 2>/dev/null | jq -r 'if length==0 then "_none._" else (.[] | "- `" + .id + "` " + .title) end')"
  backlog="$(bd_ list -l backlog --json 2>/dev/null | jq -r 'if length==0 then "_none._" else (.[] | "- `" + .id + "` " + .title) end')"
  write_generated "$dir/ideas.md" "$(header 'Ideas (parked, unvetted)')
$ideas"
  write_generated "$dir/backlog.md" "$(header 'Backlog (parked, vetted)')
$backlog"
}

# ── per-workstream trio ─────────────────────────────────────────────────────────────────────────
render_workstream() {
  local spec_id="$1" trk name
  name="$(basename "$(dirname "$spec_id")")"
  [ -n "$ONLY_WS" ] && [ "$ONLY_WS" != "$name" ] && return 0
  trk="$DOCS_ROOT/$(dirname "$spec_id")/tracking"
  echo "workstream '$name' -> $trk"

  # this workstream's epics (incl. closed), TSV.
  local epics; epics="$(all_epics_tsv | awk -F'\t' -v s="$spec_id" '$3==s')"

  # roadmap (spec_id, the workstream anchor) + brainstorm (design) links for the tracking header.
  # Paths are relative to the tracking/ dir: the anchor is a sibling-of-parent (../<file>); design
  # paths are repo-relative, reached by climbing to the repo root (../../../../<path>).
  local anchor_base designs ws_links
  anchor_base="$(basename "$spec_id")"
  designs="$(bd_ list -t epic --all --json 2>/dev/null | jq -r --arg s "$spec_id" '[.[]|select(.spec_id==$s)|.design//empty]|unique|.[]')"
  ws_links="_Roadmap: [$anchor_base](../$anchor_base)"
  while IFS= read -r d; do [ -n "$d" ] && ws_links+=" · Brainstorm: [$(basename "$d")](../../../../$d)"; done <<<"$designs"
  ws_links+="_"

  # status.md
  local st=""
  if [ -n "$epics" ]; then
    while IFS=$'\t' read -r id estat spec title; do st+="$(epic_status_line "$id" "$estat" "$title")"$'\n'; done <<<"$epics"
  else st="_no epics yet._"; fi
  write_generated "$trk/status.md" "$(header "Status — $name")
$ws_links
$st"

  # checklist.md + progress.md : per epic, walk its direct-child stages.
  local checklist="" progress=""
  if [ -z "$epics" ]; then checklist="_no epics yet._"; progress="_no epics yet._"; fi
  while IFS=$'\t' read -r e estat spec title; do
    [ -z "$e" ] && continue
    local ready_ids
    ready_ids="$(bd_ ready --parent "$e" --json 2>/dev/null | jq -r --arg e "$e" "$DIRECT_FILTER | .id")"
    checklist+="## $title"$'\n'
    checklist+="$(bd_ list --parent "$e" --all --json 2>/dev/null | jq -r --arg e "$e" --argjson r "$(printf '%s' "$ready_ids" | jq -R . | jq -s .)" "
      [ $DIRECT_FILTER ] | sort_by((.id|ltrimstr(\$e+\".\"))|(tonumber? // 0)) | .[]
      | (if .status==\"closed\" then \"- [x] \`\" + .id + \"\` \" + .title
        elif .status==\"in_progress\" then \"- [ ] \`\" + .id + \"\` \" + .title + \"  🔄 in progress\"
        elif (.id as \$i | \$r | index(\$i)) then \"- [ ] \`\" + .id + \"\` \" + .title + \"  ← ready\"
        else \"- [ ] \`\" + .id + \"\` \" + .title + \"  (blocked)\" end)
        + (if ((.notes // \"\")|length) > 0 then \"\n  - 📝 \" + (.notes|gsub(\"\n\";\"  /  \")) else \"\" end)")"$'\n\n'
    progress+="## $title"$'\n'
    progress+="$(bd_ list --parent "$e" --status closed --json 2>/dev/null | jq -r --arg e "$e" "
      [ $DIRECT_FILTER ] | sort_by((.id|ltrimstr(\$e+\".\"))|(tonumber? // 0)) | .[]
      | \"- \`\" + .id + \"\` \" + .title + \" — \" + (.close_reason // \"(no reason)\") + \"  (\" + (.closed_at // \"?\") + \")\"")"$'\n\n'
  done <<<"$epics"
  write_generated "$trk/checklist.md" "$(header "Checklist — $name")
$ws_links
$checklist"
  write_generated "$trk/progress.md" "$(header "Progress — $name")
$ws_links
$progress"
}

# ── main ────────────────────────────────────────────────────────────────────────────────────────
render_board
# Per-workstream: drive off the spec_ids of all epics (incl. closed).
#   • under docs/workstreams/  → render its tracking trio
#   • "(no anchor)"            → legitimate: an epic that belongs to no workstream
#   • anything else            → MISCONFIGURED. spec_id is the workstream ANCHOR: it
#     names the workstream and decides where tracking/ is written. A spec_id pointing
#     somewhere else means that workstream silently gets no mirrors, so fail loudly.
SPEC_IDS="$(all_epics_tsv | cut -f3 | sort -u)"
misanchored=0
while IFS= read -r spec; do
  [ -n "$spec" ] || continue
  case "$spec" in
    "$WS_REL"/*)   render_workstream "$spec" ;;
    "(no anchor)") echo "  skip: $(all_epics_tsv | awk -F'\t' '$3=="(no anchor)"' | wc -l | tr -d ' ') epic(s) with no spec_id (not workstream epics)" ;;
    *)
      misanchored=1
      {
        echo "  ERROR: epic spec_id is not under $WS_REL/ — no tracking was written for it."
        echo "         spec_id:  $spec"
        echo "         spec_id is the workstream ANCHOR. It names the workstream and"
        echo "         determines the tracking output directory. Point it at the roadmap:"
        echo "           bd update <epic> --spec-id $WS_REL/<name>/roadmap.md \\"
        echo "                            --design <the governing spec>"
        echo "         Affected epics:"
        all_epics_tsv | awk -F'\t' -v s="$spec" '$3==s {printf "           %s  %s\n", $1, $4}'
      } >&2
      ;;
  esac
done <<<"$SPEC_IDS"

if [ "$misanchored" -ne 0 ]; then
  echo "done (with errors) — some workstreams have no tracking mirrors." >&2
  exit 1
fi
echo "done."
