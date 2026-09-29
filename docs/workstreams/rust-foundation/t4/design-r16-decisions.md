# T4-0 design round 16: decisions

Input:
- the owner's revised requirements, `requirements.md`, marked "r16";
- the consolidated critical review, `critical-review.md`, with the reviews in
  `critical-review-{fable,astra}.md`;
- the round-15 design, SOUND at `778752c`.

Tag each change `[t4r16.N]`. Round 16 is one revision followed by one Sol high
review. It removes two large mechanisms, so the design should become
materially shorter than 1,907 lines. Earlier decisions stand except where
changed here.

## 1. No raw log (R8; reverses D4's raw log)

The owner found that the agents already keep the conversation. The
orchestrator checked this on 2026-09-29 with Claude Code 2.1.284, Codex
0.159.0 and OpenCode 1.18.32: one small turn and one bad-model failure each
[V].
- **Claude** writes prompts, replies, tool calls, tool results and
  per-message `usage` to `~/.claude/projects/<escaped cwd>/<session>.jsonl`.
  It does not write the stream's `system/init` or `result` messages, or
  stderr. `--debug-file <path>` writes a debug log, which recorded the
  bad-model error.
- **Codex** writes a rollout to `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
  with messages, tool calls and outputs, `token_count`, and `task_complete`,
  whose `error` recorded the failure. There is no debug file; diagnostics go
  to stderr.
- **OpenCode** writes its SQLite database (messages, parts including
  step-start and step-finish, session token totals) and `opencode.log`. The
  bad-model error appeared only on its stream.

Remove everything that exists only for VIA's raw log:
- the raw worker, its inbox, staging, group commit and `fsync`;
- offsets: `enqueued_end`, `durable_end`, `high_water`, and `high_water` on
  `FinishReport` and on the failed-open path;
- `raw_log.incomplete`, `RawFaultSink` and the raw fault latch;
- the raw disk counter and budget, and raw recovery;
- the `logs` byte cursors, and `raw_ref` on observations;
- A27's and A28's raw text, and every raw test (the `s1_raw_*` cases, and the
  raw parts of F24 and the disk test).

Keep the `connections` table only if something other than raw needs it, and
say which. Blobs for prompts stay.

What replaces it:
- **Evidence folder** per turn under the state directory. Choose its layout
  and file names. It holds:
  - the agent's stderr: Host opens the file and hands it to the child as its
    stderr, so the operating system writes it and no VIA task reads it;
  - on a decode failure, the message VIA could not understand, capped (for
    example the first 64 KiB plus its total length); the failure summary
    names the file;
  - Claude's `--debug-file`, which a route may request with a path in the
    folder. Whether debug mode changes Claude's behaviour is [U]; the Claude
    slice probes it.
- **A bound for the stderr file.** The operating system writes it, so VIA
  cannot stop individual writes. State the smallest bound, for example a size
  check on an existing timer that fails the turn past a cap.
- **SQLite** stores:
  - per session: the vendor session ID, and a transcript-path hint (nullable;
    the vendor's layout is internal, and each vendor task confirms it);
  - per turn: the evidence folder path.
- **C1 `logs`** returns the vendor session ID, the transcript hint, the
  evidence folder, and its files with their sizes. It has no paging and does
  not read file contents.
- **Wire** keeps reading stdout, splitting messages and capping their size.
  - State the per-message cap. Today it is 1 MiB (`MAX_STDOUT_MESSAGE_BYTES`).
    A Claude message carrying an image or a large tool result may exceed it,
    so record a vendor-probe item.
  - With no memory pool (decision 2), you may raise the cap if the worst-case
    estimate allows; give the reason either way.
- **Terms:** A24's list replaces "Raw log" with "Evidence folder".

## 2. No memory pool (R7)

Memory is bounded by construction: every buffer has a fixed maximum, and every
kind of holder has a fixed count.

Remove:
- `MemoryBudget` and its class charges;
- every `memory.*` configuration key and its validation;
- `MEMORY_BUDGET` refusals;
- the counting-allocator test;
- `codex_shared`.

The dispatch hold-and-wait and tokio permit-hoarding finding (critical review,
change 1) dissolves with the pool; say so in the report.

Keep, and list in one table, each fixed maximum and its count:
- four connection slots;
- 32 sockets;
- Store lane capacities;
- the C2 A1 observation bounds;
- the per-message cap;
- the 1 MiB envelope;
- the reply bounds.

Give their worst-case sum as an estimate [I]. The F24 RSS gate measures it,
with a stated CI margin.

**C1 requests:**
- A request line is at most 1 MiB. A longer line gets a named error; choose
  it, and amend C1 §1 (today "Proposed 16 MiB").
- A larger prompt is passed as a file path (choose the parameter, for example
  `prompt_file`: an absolute path to a regular file that the user can read).
  VIA copies it into the blob store in 64 KiB chunks with SHA-256. The retry
  identity uses the blob.
- State what happens if the file changes during the copy.
- Q-R5-8 is moot.

## 3. Disk: a free-space floor, not budgets (R7)

Remove:
- `sqlite_budget`, `files_budget` and both headrooms;
- the terminal-reserve arithmetic;
- `max_page_count` and the `page_count` check before `COMMIT`;
- `store_over_budget` and the rule that refuses start.

Keep the `SQLITE_FULL` and `ENOSPC` handling: roll back, report known not
committed, latch only a failed rollback.

Add:
- **Admission.** `statvfs` of the state directory is checked when new work is
  admitted (spawn, resume, a queued dispatch). Below `disk.free_floor`
  (default 5 GiB) VIA refuses with a named error. Running turns finish, and
  lifecycle and terminal writes proceed.
- **Warning.** `daemon/status` reports VIA's data size and a warning above
  `disk.warn_size` (default 2 GiB). Measure the size cheaply, for example
  cached and refreshed at most once per interval; choose the rule.
- **WAL** (critical review change 5):
  - keep the checkpoint policy;
  - at `wal.max`, refuse ordinary writes by name and retry checkpoints, with
    no Store health latch;
  - lifecycle and terminal writes still commit;
  - amend runtime §6, `runtime-contracts.md:769-771`;
  - the WAL may exceed `wal.max` by one transaction, which `via-d9o.2.3`
    measures (Q-R9-1).
- **Config keys:** `disk.free_floor`, `disk.warn_size`, `wal.max`,
  `wal.checkpoint_bytes` and `wal.checkpoint_commits`. Keep reading them at
  start, exit 78 on an invalid file, and report the effective values in
  `daemon/status` `limits`. Q-R8-3 is moot: a store already below the floor
  starts and refuses new work.

## 4. Envelope `steps` and `list` (owner)

- The envelope's `steps` is the vendor's own count (Claude `num_turns`), or
  `null` when the vendor reports none. VIA's count appears only in `status`
  `progress`, labelled as VIA's. N1 is decided this way.
- `list` pages in creation order, newest first: a keyset on `ord`. Every row
  carries `last_active_at`. Remove `stamp`, the second phase and its proof.
  N2 is decided this way.

## 5. Critical-review changes that still apply

1. **Steps per vendor** (change 2).
   - Claim step counts, like tokens, per vendor only after its probe.
   - Add tool-only and parallel-tool fixtures to each vendor's probe.
2. **`status` describes one moment** (change 3).
   - `progress` appears only when it belongs to the selected turn.
   - State that the open step has no row yet.
3. **`json_limits`** (change 4, N4). It stays in via-store; via-wire
   re-exports it for Routes, so the layer graph is unchanged.
4. **`wait` backs off** (change 6).
   - The poll interval starts at 20 ms and doubles up to 250 ms.
   - Document one request per socket, 32 sockets, and that the 33rd is
     closed.
5. **Reply deadline** (change 7). A32's 10 s runs from when the reply is ready
   to write, not from its first byte.
6. **Tests** (change 8).
   - The many-steps test uses 2,000 steps.
   - The allocator test goes with the pool.
   - The RSS gate states its margin.
7. **Drop A34** (Q-R5-13 declined).
   - On the item that crosses 1 MiB, Core records the overrun and drops its
     observation receiver.
   - The route then fails `Overflow` through the stall's existing path.
   - Core rule: a recorded overrun gives `failed(overflow)`, unless a `store`
     failure takes precedence.
   - Remove `StopCause::Overflow`, its T3 deadline row and its coalescing
     rank.
8. **Final text** (Q-R6-1).
   - Completed text only, in pieces of at most 256 KiB.
   - Remove keys and `replace`.
   - Codex sends each completed `agentMessage` text.
9. **Remove** `HISTORY_PRUNED`.

## 6. Owner answers to the carried questions

- **Accepted:**
  - Q-R5-1, Q-R5-2, Q-R5-4, Q-R5-7, Q-R5-9 and Q-R5-14;
  - Q-R5-5, with Astra's wording: `progress` adds no Store read, while
    `status` still makes one;
  - Q-R5-10, with change 7;
  - Q-R8-2 and Q-R8-4.
- **Now in the requirements:** Q-R5-11 (R3) and Q-R5-15 (R2, R4, R6).
- **Replaced by R7:** Q-R7-1 to Q-R7-3, and Q-R8-1.
- **Deferred with its key:** Q-R9-2.
- **Moot:** N3.

## Outputs (commit on `wt/t4-0`, then stop)

1. **`t4/design.md` as round 16.** Rewrite the affected sections rather than
   annotate them. Rewrite the A-amendment list:
   - withdraw what served only the raw log, the pool or the budgets;
   - add what these decisions need: C1 `logs`, `list` order, the request cap
     and prompt file, envelope `steps`, runtime §4 without a raw log, §6 WAL,
     §8 bounds, and C2 without `raw_ref`.
2. **`t4/reports/T4-0.md` §20**, with:
   - a removed / kept / changed / added table;
   - the disposition of each critical-review item;
   - any remaining owner questions, which should be few.
3. Report the line count.

This step writes no production code and no tests, and does not edit
`docs/specs/`.

## Self-check before committing

- Grep for leftovers: `high_water`, `raw_log`, `RawFaultSink`, `MemoryBudget`,
  `memory.`, `sqlite_budget`, `files_budget`, `max_page_count`, `stamp`,
  `StopCause::Overflow`, `replace`, `HISTORY_PRUNED`. Remove each one or
  justify it.
- Every file:line citation is rechecked.
- There are no absolute paths and no "frame".

## 7. Owner follow-up on the round-16 design (8772edb)

The owner reviewed the round-16 choices on 2026-09-29. **Principle:** do not
design mechanisms for problems that have not been observed. Choose the
simplest behaviour now, and list the case for end-to-end measurement with
every adapter (`via-d9o.2.3`). Apply these as a small update to round 16,
tagged `[t4r16.7.N]`:

1. **`via.log`.** The daemon's own warnings and errors (its `tracing`
   output) go to `<state>/via.log`, with session and turn IDs where they
   exist.
   - Keep stderr only for startup, so the auto-starting CLI can still report
     a failed start (`crates/via-cli/src/server.rs:96`,
     `crates/via-cli/src/client.rs:133`).
   - Lines are rare, so a plain synchronous append is acceptable.
   - Bound the file simply: at daemon start, a `via.log` over 10 MiB replaces
     `via.log.1`.
   - It is diagnostic, not a C1 contract.
2. **Drop Claude's debug file** (`claude-debug.log`, `--debug-file`) from
   Task 4. The Claude task may add it later on evidence.
3. **Keep** the transcript path hint (Q: B).
4. **Accept** Q-R16-1: the RSS gate is 1.25 × the §5.1 sum.
5. **Agent stderr stays uncapped.** The operating system writes
   `stderr.log`, and VIA adds no size check, tick, failure or tail.
   - Remove `EVIDENCE_FILE_MAX`, the 1 s tick and
     `Reader(EvidenceTooLarge)`.
   - Record uncapped stderr as a limitation and as a `via-d9o.2.3`
     measurement item. Q-R16-2 is withdrawn.
6. **At `wal.max`, running turns' step rows still commit.** Treat them like
   lifecycle writes. Only new work and other ordinary writes are refused
   (Q-R16-3).
7. **`wait`** checks at once, then once per second. The backoff goes.
8. **Large results travel as files; there is no envelope-overrun failure.**
   - **Final text.** When the final text would not fit inline, choose the
     threshold so that the envelope can never exceed 1 MiB. Core writes the
     whole final text to `final_text.txt` in the turn's folder, and the
     envelope carries its path and byte length instead of the text. The file
     has a simple cap, for example 64 MiB: past it, stop appending and mark
     it truncated. Do not fail the turn.
   - **Denied and declined lists.** Keep the first 1,000 entries and a total
     count.
   - **Remove** the envelope-overrun path: Core's receiver drop on the
     crossing item, the overrun precedence rule, and the overflow-only parts
     of the bounded failure summary. The stall's `overflow` path (C2 A1) is
     unchanged.
   - The folder now also holds a result file. Rename it only if that clearly
     reads better; keep the churn minimal.
9. **Measurement list.** Add a short section listing every assumption left
   to `via-d9o.2.3`:
   - the RSS against §5.1;
   - stderr sizes;
   - the largest vendor message against 1 MiB;
   - final-text sizes and how often the spill is used;
   - list lengths;
   - disk and WAL growth (Q-R9-1);
   - floor behaviour;
   - per-vendor steps and tokens;
   - the size of `via.log`.

   Do not redesign other sections for this principle. Name other
   speculative mechanisms you would simplify in T4-0.md §20 as candidates
   only.
