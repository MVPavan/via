# S1-specs: Task 3 amendments applied to the specs

Status: DONE_WITH_CONCERNS (documentation only; C1 and C2 resolved in fix
round 1, C3 and C4 left as reported by orchestrator decision).

Bead `via-jm4.7.9.3`, S1 critic finding 13. Branch `wt/s1-specs`, cut from
`rust-foundation` at `7370e0e`. Source: `docs/workstreams/rust-foundation/t3/design.md`
§12, rows A1–A23 (A10 withdrawn) and the A14, A15 and A5 replacement texts.

## Commits

| Commit | Document | Amendments |
|---|---|---|
| `7ff0c7e` | `docs/specs/runtime-contracts.md` | A3, A12, A13, A14, A20 |
| `56e4309` | `docs/specs/via-api-v1.md` | A4, A5, A7, A8, A9, A15, A17, A21, A22 |
| `e848a6b` | `docs/workstreams/rust-foundation/t2/dispatch-design.md` | A1, A2, A11, A16, A18 |
| `5eba7e0`, `bbec36f` | `.repo-context/coding-style.md` §6 | A6 (the second commit makes it an exception to the "only daemon main and the anchor" sentence) |
| `f04260d`, `523e5f4` | this report | |
| `cfac18e` | runtime §8, §10; dispatch §1 | fix round 1: C1, C2 |
| the commit after `cfac18e` | this report | fix round 1 |

The orchestrator's mapping of rows to targets matches the table: I found
no row that targets another document.

## Per amendment

| # | Target | Result | Commit | Note |
|---|---|---|---|---|
| A1 | dispatch §1, §2 | applied | `e848a6b` | "No other code submits or cancels a queued turn" is replaced by the claim rule (T3 §3.1 says it replaces this sentence); the §2 `queue` bullet gains "each with its claim". |
| A2 | dispatch §5 | applied | `e848a6b` | "at least one counted queued turn" becomes "a counted queued turn **or** a close order". |
| A3 | runtime §6.1 | applied | `7ff0c7e` | After "Lock conflict refuses startup without deletion/takeover": `daemon.lock` contention exits 75; a `store.lock` conflict exits 4 (T3 §6.1, added so the 75 is not read as covering both locks); CLI retries within 15 s and shows startup stderr otherwise. |
| A4 | C1 §1 | applied | `56e4309` | The version-mismatch bullet: a mismatched `hello` stops nothing, returns `data: {daemon_version, store_path}`, permits one plain `daemon/stop` that only the idle predicate accepts. "The CLI restarts an idle daemon" gains "whose Store matches (runtime §6.1)", per A13. |
| A5 | C1 §7.1 | applied | `56e4309` | Verbatim replacement row. |
| A6 | coding-style §6 | applied | `5eba7e0`, `bbec36f` | Phrased as an exception to the "Only ... install signal handlers" sentence. |
| A7 | C1 §7.6 | applied | `56e4309` | Added as a paragraph under the table. "design §7.2" is written as a link to the Task 3 design §7.2 plus "(runtime §7)". |
| A8 | C1 §3.5 | applied | `56e4309` | Before "Idempotent." |
| A9 | C1 §3.14 | applied via A15 | `56e4309` | A9 names A15's replacement text; `store_failure` and `connections` are in the new shape. |
| A11 | dispatch §3 | applied | `e848a6b` | Appended to the "Queued turns keep their last durable state" bullet. |
| A12 | runtime §3 | applied | `7ff0c7e` | The C3 sketch drops `interrupt` and gives `execute` a `stop: StopWatch` parameter; a sentence under the sketch states the amendment. The rest of the sketch (`connection_id`, `FakeStart`) was already stale against the code and is not touched (C4). |
| A13 | runtime §6.1 | applied | `7ff0c7e` | After "do not stop that daemon or silently use its Store". |
| A14 | runtime §7 | applied, adapted | `7ff0c7e` | First paragraph and table replaced. Two parts not applied because Task 4 removed them (conflicts K1, K2). The three later paragraphs (batch; Host 3 s; 5 s window and 10 s bound) gain "On the latched path:". The `[r3.13]`/`[r3.14]` review tags are dropped from spec text; a closing sentence names A14 and the Task 3 design §7.2. |
| A15 | C1 §3.14 | applied, merged | `56e4309` | Replacement text applied; the Task 4 fields `limits` and `storage` and their sentence are kept (conflict K3). The trailing "Drain runs accepted turns ..." sentence, outside A15's passage, is kept. |
| A16 | dispatch §3 opening, §3.2, and the eight sites | applied | `e848a6b` | §3 opening and "Store read failures never latch" per the row. §3.2's "observes a failed or uncertain write" becomes "observes a latching write (§3)". One "not committed / uncertain" line at each site: §2.2 steps 5, 6, 7; §2.3 (new bullet); §2.4; §6; §7 row; §8 row. "design §7.2" is written "T3 §7.2", defined in the header list. |
| A17 | C1 §8.1 `store_error` | applied | `56e4309` | Two sentences added after the pre-receipt clause; the rest unchanged. |
| A18 | dispatch §2.4 | applied | `e848a6b` | Verbatim, with "(T3 §6.3)". |
| A19 | runtime §6.2, §7 | already applied | `7958f5a` (earlier) | Both sites already read "the latching (Store) failure (`failed_at`)". No change. |
| A20 | runtime §5.1 | applied | `7ff0c7e` | New paragraph after the Challenge/Status/Stop paragraph. |
| A21 | C1 §4 `deadlines` | applied | `56e4309` | Appended to the row's notes. |
| A22 | C1 §3.6 | applied | `56e4309` | "`Closed`" (a T3 internal name) is written as "`session.closed` commit"; reference to Task 3 design §4 step 6. |
| A23 | runtime §7 | already applied | `602bd21`, `7958f5a` (earlier) | The Host paragraph already carries A23 and cites it. It says "3 s from the instant the failure is raised"; A23 says "from the force instant". The latch raises the failure and sends force in one step (dispatch §3.2 phase one), so I left it. |

## Conflicts with Task 4 (T4 text kept)

- **K1. Raw thread and `StoreError::Raw` (A14).** A14's "uncertain" list names
  "the writer thread or the raw thread is gone", and a paragraph makes a
  raw append/sync failure or full raw queue fail its turn with an
  incomplete record. T4-A41/A46 removed the raw log, and the T4 design
  (§12, "T3, other raw text") voids T3's quoted runtime text at these lines.
  Applied: "the writer thread is gone"; the raw paragraph is omitted, and
  its last two sentences ("A latched Store failure cleans up every active
  connection. No Store task silently swallows failure.") are kept. The code
  has no `StoreError::Raw`.
- **K2. "Following affected history" row (A14).** Marked obsolete in A14
  itself for T4-A25; omitted.
- **K3. `daemon/status` shape (A15).** T4 added `limits` and `storage` after
  `health`. The applied shape is `... health, store_failure, connections,
  limits, storage` and keeps T4's sentence on `limits` and `storage`.

## Concerns

- **C1. Runtime §8 "Health channel" row** still says "first failure
  retained", while A14/A15 report the latest recorded failure in
  `store_failure` (code: `latch.rs` keeps a `latest`). No amendment targets
  this row, so I left it. **Resolved in fix round 1 (`cfac18e`).**
- **C2. Runtime §10 audit rows and dispatch §1's latch row** ("Whether a
  state write failed or was uncertain") still use pre-A14/A16 wording. They
  point to §7 and §3, which now carry the scoping, and no amendment names
  them. **Resolved in fix round 1 (`cfac18e`).**
- **C3. Specs now cite the Task 3 design** for the scoped-case list (runtime
  §7, C1 §7.6, C1 §3.6, dispatch-design). Runtime §7 already cited "amendment
  A23 in the Task 3 design", so this follows the existing pattern, but the
  row-level case list (design §7.2) lives only in a workstream document.
  Moving it into runtime §7 would add content beyond the amendments.
- **C4. The C3 sketch in runtime §3** was already stale against the code
  (`execute` takes `TurnStart`, a hop sender and a force watch, not
  `connection_id`/`FakeStart`). A12 only touches the stop watch and
  `interrupt`, so the rest stays.

No amendment conflicts with the code in a way I could see without deep
analysis. Spot checks: `LOCK_CONTENDED = 75` (`crates/via-cli/src/server.rs`,
`client.rs`), `AFFECTED_ADDRESSES = 16` (`crates/via-core/src/engine/latch.rs`),
`idle_ms == Some(0)` refused (`crates/via-core/src/api.rs`), `held_unproven`
in `crates/via-core/src/engine/status.rs`, and `execute(..., stop: StopWatch)`
with no `interrupt` entrypoint in `crates/via-routes/src/runtime.rs`.

## Verification

No cargo run (documentation only, per the brief).

- `git diff rust-foundation...HEAD --stat` (from the merge base `7370e0e`;
  `rust-foundation` has since gained the critic review and a handoff
  update, which the two-dot diff shows as reverse changes):

  ```text
   .repo-context/coding-style.md                      |   7 +-
   docs/specs/runtime-contracts.md                    |  93 ++++++++++++-----
   docs/specs/via-api-v1.md                           |  74 +++++++++----
   .../s1-critique/reports/S1-specs.md                | 114 +++++++++++++++++++++
   .../rust-foundation/t2/dispatch-design.md          |  58 +++++++----
   5 files changed, 275 insertions(+), 71 deletions(-)
  ```
- Key-phrase grep per amendment in its target: each found (A1 "only the
  claim owner writes the turn"; A2 "counted queued turn **or** a close
  order"; A3 "contention exits 75"; A4 "A mismatched `hello` stops nothing";
  A5 "for a session with unfinished work at force acceptance"; A6 "SIGINT
  handler that only exits 130"; A7 "Rows 1–2 cover caller-originated
  cancels"; A8 "A no-`wait` cancel of a running turn"; A9 "store_failure,
  connections"; A11 "also cancels the"; A12 "per-turn stop watch
  supersedes"; A13 "version-mismatched"; A14 "A Store write has one of
  three outcomes" and "On the latched path" ×3; A15 "Drain closes no
  session"; A16 "first **uncertain** state write" and the site lines; A17
  "never enqueued because the writer's queue was full"; A18 "queue entry or
  a running or settling turn"; A19 "latching" ×2; A20 "only to an anchor
  whose"; A21 "`0` is `invalid_params`"; A22 "is refused a second time";
  A23 "amendment A23"). The old A14 anchors "First SQLite/state write
  failure" and "best-effort cleanup but return" no longer occur in runtime
  §7.
- `git status`: clean after the report commit.

## Fix round 1 (orchestrator decision: fix C1 and C2; leave C3 and C4)

Commit `cfac18e`.

- **C1.** Runtime §8 "Health channel" row: "first failure retained" becomes
  "`health: store_failed` is sticky after the latch; `store_failure` reports
  the latest recorded failure and its scope". Confirmed against the code:
  `Engine::health` in `crates/via-core/src/engine/latch.rs` returns
  `store_failed` "from the latch's phase one on, sticky", and
  `FailureRecord` keeps only `latest` plus a `count`, which
  `store_failure_status` reports.
- **C2.** Runtime §10, first audit row (C1 summary, §3.8–3.9, §8.1): a write
  known not committed after a receipt is scoped to its turn, which ends
  `failed(store)` through one resolution write; the `store_error` with
  `terminal_persisted:false` applies once the failure is latched (an
  uncertain write, a failed turn resolution write or terminal retry, or
  SQLite corruption); "Add health" becomes "Add `health` and
  `store_failure`". The other §10 rows do not contradict A14/A16 and are
  unchanged. Dispatch-design §1 latch row: "Whether a state write failed or
  was uncertain" becomes the A16 rule.
- Not changed, since they are outside the named rows or already carry an
  A16 site line: dispatch-design header history (line 8, a record of the
  T2 correction), §2.3, §6, §7 and §8 left columns (A16 lines appended in
  `e848a6b`), and §3's "best-effort writes after the first failure".

Verification: `grep -n "first failure retained\|state write failed or was uncertain"`
on both files finds nothing; `git status` clean after this commit.
