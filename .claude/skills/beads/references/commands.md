# Beads command reference

## Cheat-sheet

| Goal | Command |
|------|---------|
| Recover context / session-close protocol | `bd prime` |
| Ready (unblocked) work | `bd ready` |
| List by status | `bd list --status=open\|in_progress\|closed` |
| Inspect one issue | `bd show <id>` |
| Search text | `bd search "<query>"` — titles + open only; add `--status all` and use `--desc-contains` for full search |
| Create | `bd create "title" --description="why + what" -t task\|bug\|feature\|epic\|chore\|decision -p 0..4` |
| Claim | `bd update <id> --claim` |
| Edit fields | `bd update <id> --title/--description/--notes/--append-notes/--design/--priority/--status` |
| Close (one or many) | `bd close <id> [<id>…] [--reason="…"]` |
| Dependencies | `bd dep add <id> <depends-on>` · `bd dep tree <id>` · `bd blocked` |
| Labels | `bd label add\|remove <id> <label>` · `bd list -l <label>` · `bd label list-all` |
| Human-attention queue | `bd human list` · `bd human respond\|dismiss <id>` |
| Lifecycle | `bd defer <id> --until=<date>` · `bd supersede <id> --with=<id>` · `bd stale` · `bd orphans` |
| Epics | create: `-t epic [--spec-id <path>]` · child: `bd create … --parent <epic-id>` · `bd epic status` · `bd epic close-eligible` · children: `bd list --parent <id>` · by spec: `bd list --spec <prefix>` |
| Stats / health | `bd stats` (quick summary) · `bd lint` · `bd stale` · `bd orphans` · `bd gc` / `bd prune` |

Priority is `0..4` (0=critical), not high/med/low. Never `bd edit` (opens $EDITOR — use `bd update` flags). Add `--json` to parse output. Close only when the work is actually done.

Maintenance — on-demand, not per session: use the stats/health row above. Run `bd preflight --check` only when its checks match this repo's current toolchain; this CLI version includes some Go-specific checks.


Check current `bd <command> --help` for unfamiliar flags.
