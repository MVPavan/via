# W5-S report: split `via-core/src/engine.rs`

Branch `claude/w5-s-task-9c9d29`, based on `64d5a40` (Task 1 closed at
`e24bb2c`). This refactor keeps behaviour unchanged. It has no failure-first
regression because there is no defect to reproduce. Evidence that behaviour
is unchanged comes from the unchanged test count and the line-multiset check
below.

## Layout

`engine.rs` remains the module root, and `engine/journal.rs` already sat
beside it, so no `mod.rs` move was needed. Every body was moved verbatim. The
only edits are module docs, `use` lists, `impl Engine {` wrappers, the
visibility changes listed below, and one `rustfmt` rewrap of the `classify`
signature, which got longer after `pub(super)` was added.

| File | Lines | Responsibility |
|---|---|---|
| `crates/via-core/src/engine.rs` | 305 | `Engine` and its fields, `open`, `active`, and the C1 entry points `spawn`, `steer`, `result`, `wait`, `events` and `logs`. It also holds the shared small types used by more than one sibling: `Started`, `Accepted`, `TurnRecord`, `Terminal` (with `fail`), `ForcedTurn`, `RouteClose`, and the helpers `lock` and `failure`. It re-exports `stop::{EngineShutdown, StopMode}`, so the `lib.rs` exports are unchanged. |
| `crates/via-core/src/engine/drive.rs` | 519 | Turn driving: `drive`, `Driven`, `execute`, `observe`, `commit_event`, `submit`/`commit_submission`, `accept`, `finish`/`finish_turn`/`commit_turn_ended`, `FORCE_CLOSE_REASON` and `event_body`. |
| `crates/via-core/src/engine/stop.rs` | 249 | Stop and shutdown: `StopMode`, `EngineShutdown`, `request_stop`, `stop_mode`, cancel settlement (`settle`, `stop_outcome`), final `shutdown` with forced-turn terminals, `unresolved_turns` and `FORCED_COMMIT_RESERVE`. |
| `crates/via-core/src/engine/terminal.rs` | 254 | Failure classification and envelope assembly: `classify`, `failed_terminal`, `route_disposition`, `canonical_stop_reason` and `terminal_envelope`. It also holds the unit test `route_causes_keep_their_c1_disposition`, which moved with `failed_terminal` without changes. |
| `crates/via-core/src/engine/journal.rs` (+ `journal/tests.rs`) | unchanged | The existing module, not touched. Its `use super::…` and `crate::engine::…` paths still resolve. |

Deviation from the brief's example: failure classification and envelope
assembly are in `terminal.rs`, not `drive.rs`. Together they are about 250
lines of pure functions with their own unit test. Keeping them separate puts
`drive.rs` near the 500-line review prompt instead of about 750 lines. It also
lets a later worker who changes C1 §7.6/§8.2 dispositions own a file apart
from whoever changes the drive loop.

The shared types stay in the parent module on purpose. Their private fields
are visible to every descendant (`drive`, `stop`, `terminal`,
`journal::tests`), so no struct field had to widen.

## Visibility widened

Each item below was private in `engine.rs`, which made it visible to all of
`engine` and its descendants. It is now `pub(super)` in a child module. That
gives exactly the same effective scope: `crate::engine` and its descendants.
Nothing became visible outside `engine`.

| Item | Now in | Needed by |
|---|---|---|
| `Engine::finish` | `drive.rs` | `stop.rs` (`shutdown` commits forced terminals) |
| `Engine::commit_event` | `drive.rs` | `stop.rs` (`settle` commits `cancel.settled`) |
| `Engine::finish_turn` | `drive.rs` | `journal/tests.rs` |
| `Engine::commit_turn_ended` | `drive.rs` | `journal/tests.rs` |
| `Engine::submit` | `drive.rs` | `journal/tests.rs` |
| `Engine::settle` | `stop.rs` | `drive.rs` (deadline cancel) |
| `stop_outcome` | `stop.rs` | `drive.rs` (deadline cancel) |
| `classify`, `terminal_envelope` | `terminal.rs` | `drive.rs` |

## Behaviour-preservation check

I compared the multiset of non-blank, trimmed lines of the original
`engine.rs` (`git show HEAD:crates/via-core/src/engine.rs`) with the union
of the four new files, ignoring `pub(super) ` prefixes. The only
differences are `use` lists, `mod`/`pub use` lines, the three module doc
lines, the added `impl Engine {`/`}` wrappers and the `classify` signature
rewrap. No statement, comment or doc line of any body was added, removed or
changed.

`git diff --stat` shows `engine.rs` losing about 985 lines and three new
files adding them. Git cannot detect a one-to-many split as a rename.

## Gate results

`XDG_RUNTIME_DIR` was set to a private 0700 directory for each run.

| Command | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | pass |
| `cargo nextest run --locked --workspace` | 122 passed, 2 skipped. This is the same as the `e24bb2c` baseline. |
| `cargo deny check` | `advisories ok, bans ok, licenses ok, sources ok` |
| `python3 scripts/check-layers.py` | pass |
| Root-only peer-UID check (`--run-ignored only -E 'test(c1_client_refuses_daemon_socket_of_another_uid)'`) | 1 passed. This session runs as root. |

Tooling note: prebuilt cargo-deny 0.18.3 could not fetch the advisory DB
through this machine's proxy. Prebuilt 0.20.2, the version W1-B used, fetched
it and passed. The problem is in the environment, not in the repository.

## Open or uncertain

- `drive.rs` is 519 lines, just past the 500-line review prompt. The
  submission and acceptance commits (`submit`, `commit_submission`,
  `accept`) could move into their own module later if Task 2 needs another
  disjoint file. I did not move them here because the brief asks for the
  minimum split.
- No other files were touched.
