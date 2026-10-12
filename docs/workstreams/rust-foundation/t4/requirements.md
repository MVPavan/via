# Task 4 requirements: events, progress and storage

Status: owner-approved (2026-09-29). Revised the same day after the critical
review (`critical-review.md`); the revisions are marked **(owner,
2026-09-29 r16)**. It supersedes the "every event is durable and followable"
assumption behind T4-0 rounds 1–4.

## Decision

VIA's callers are programs and model orchestrators. No human watches a turn
live. A caller needs to know when a turn ends, the result, and which step the
agent is on (so it can steer or cancel). For post-mortem detail it reads the
agent's own transcript and the turn's evidence folder. VIA does not store or
stream per-message detail, and keeps no copy of vendor traffic **(owner,
2026-09-29 r16)**.

## Terms

- **Step**: one model call inside a turn. The model writes text and may request
  tools, the tools run, and the next step reads their results.
- **Observation**: what an adapter reports from one vendor message. Core turns
  an observation into a durable event, a progress update, or envelope
  accumulation.
- **Event**: a durable record in SQLite with a per-session `seq` (R1).
- **Event trace**: the ordered events of one turn.
- **Evidence folder**: one folder per turn under VIA's state directory, holding
  the agent's stderr, the message VIA failed to understand (if any) and a
  final text too large for the envelope (R6, R8).

## Requirements

**R1. Durable events.** SQLite stores only lifecycle, control and safety
events, plus the envelope:
- `session.opened`, `session.reopened`, `session.closed`;
- `turn.queued`, `turn.submitted`, `turn.started`, `turn.ended`, `turn.revised`;
- `cancel.requested`, `cancel.settled`, `steer.delivered`;
- `action.denied`, `vendor.request_declined`;
- `process.exited`, `server.lost`, `warning`.

Rule: an event is durable when crash recovery or the envelope depends on it.
`raw_log.incomplete` is gone with the raw log **(owner, 2026-09-29 r16)**.

**R2. Not stored or streamed.** `assistant.text`, `reasoning.summary`,
`tool.started`, `tool.ended`, `usage.updated`, `file.changed` and
`vendor.other` stop being events. The agent's own transcript keeps them.

From a vendor message, VIA reads only what these requirements need:
- the message type;
- correlation IDs: vendor session or thread, vendor turn, tool call;
- acceptance and identity evidence;
- tool names;
- usage numbers;
- the payloads of `action.denied`, `vendor.request_declined` and
  `steer.delivered`;
- final text, and the terminal fields the envelope already carries.

It does not interpret tool inputs or outputs.

**R3. Progress snapshot.** `status` on a running turn returns a small snapshot,
held in memory and updated as vendor messages arrive:

| Field | Meaning |
|---|---|
| `current_step` | VIA's own count of the step running now, labelled as VIA's. It goes up by one when the model produces output after tool results, derived the same way for every vendor. |
| `phase` | `model` or `tools` |
| `running_tools` | names of the tools running now; no inputs or outputs |
| `last_activity_at` | time of the last vendor message |
| `tokens` | approximate running total, updated once per step, labelled with its scope; about 95% accuracy is the target |

Step counts and token totals are unproven for each vendor until that vendor's
probe validates them against captured fixtures, including tool-only and
parallel-tool steps. Until then a vendor's `tokens` may be `null` **(owner,
2026-09-29 r16)**. The fake agent proves the mechanism.

The snapshot ends with the turn. The envelope holds the final figures.

**R4. Step history.** When a step ends, Core writes one row:
- the row is `(session_id, turn, step, started_at, ended_at, tokens)`;
- there is one `steps` table, keyed by session first;
- writes go through the existing single Store writer, and may be batched;
- there is no write per vendor message.

A caller can read the step rows of any turn, running or finished. After a
daemon crash, rows survive up to the last committed step. The step in
progress, and a step whose row commit was in flight, are missing from the
history; the agent's transcript still has them.

**R5. Caller interface.**
- `wait` blocks until the turn ends. It checks at once, then re-reads as
  soon as the Store's commit signal changes, and every 5 s without a change
  (a safety recheck), until its deadline **(owner, 2026-10-04; supersedes
  the earlier "then once per second", owner 2026-09-29)**.
- `status` returns the progress snapshot and step history (R3, R4) for one
  moment of one turn; callers poll it.
- `events` pages the durable events. There is no follow stream.
- `logs` returns where the evidence is: the vendor session ID, the path of the
  vendor's transcript, and the turn's evidence folder with its files. The
  caller reads the files; VIA does not decode them **(owner, 2026-09-29 r16)**.
- `list` pages sessions in creation order, newest first. Every row shows the
  session's last-active time **(owner, 2026-09-29 r16)**.

**R6. Envelope.** It carries:
- final text, inline when it fits; otherwise the path and size of a
  `final_text.txt` file in the turn's folder, so a turn never fails for a
  large answer **(owner, 2026-09-29 r16)**;
- `steps`: the vendor's own step count, or `null` when the vendor reports
  none. VIA's own count stays in `status` **(owner, 2026-09-29 r16)**;
- `usage` (exact where the vendor reports it);
- denied actions and declined requests: the first 1,000 of each and their
  total counts;
- cost;
- the evidence locations (R5 `logs`), in place of raw log spans.

It keeps its 1 MiB accumulation bound. The terminal transaction that carries
it may exceed runtime §8's 1 MiB transaction cap.

**R7. Bounds** **(owner, 2026-09-29 r16; replaces the pool and budget
strategy of round 8)**.
- **Memory is bounded by construction.** Every buffer has a fixed maximum
  size, and the number of each kind of holder is fixed (running connections,
  caller sockets). There is no memory pool, counter or memory setting. Flood
  tests measure the worst case against an RSS gate.
- **Caller requests.** One request line is at most 1 MiB. A larger prompt is
  passed as a file path; VIA copies the file into its own storage in small
  chunks.
- **Disk.** There are no fixed size budgets.
  - VIA stops admitting new work when free space on its state disk falls
    below a floor, 5 GiB by default. Running turns finish, and their endings
    are recorded.
  - `daemon/status` warns when VIA's data grows past a size, 2 GiB by default.
    Cleanup belongs to the retention task.
  - A checkpoint policy bounds the WAL. At its limit, ordinary writes are
    refused while checkpoints are retried; the Store is not latched unhealthy.
- **Evidence files** have size caps (R8).
- **Configuration.** The disk floor, the warning size and the WAL limit and
  triggers are keys in a daemon config file with provisional defaults. The
  daemon reads it at start, so a change takes effect at the next start;
  invalid values refuse to start with a named error. C1 API limits are not
  configurable. Task: `via-jm4.7.8.1`.

`via-d9o.2.3` measures memory and disk use in end-to-end testing and sets the
defaults.

**R8. Evidence without a raw log** **(owner, 2026-09-29 r16; reverses D4's
raw log, `docs/brainstorms/README.md` §15)**.
- VIA reads the vendor stream live, keeps only what R1–R6 need, and discards
  the rest. It writes no copy of vendor traffic.
- The agent's stderr goes to a file in the turn's evidence folder, written
  by the operating system.
- When VIA cannot understand a vendor message, it writes that message,
  capped, to the evidence folder, and the failure names the file.
- VIA's own warnings and errors go to one daemon log, `via.log`, in its
  state directory. It is for diagnosis and is not part of the caller API.
- Agent stderr is not capped. Its size is measured in `via-d9o.2.3`.
- SQLite stores the vendor session ID, the vendor transcript path as a hint,
  and the evidence folder path. The transcript is the vendor's file: VIA
  neither parses nor deletes it.
- Task 4 covers per-turn connections only (the fake, Claude). Evidence for
  OpenCode's per-session server and Codex's shared server belongs to those
  adapter tasks.

## Non-goals

- Human live viewing.
- A progress stream.
- A VIA copy of vendor traffic, or decoding vendor files on request.
- Reporting changed files. The caller tracks file changes with git, using one
  worktree per concurrent agent.
- Retention and pruning. Step rows, events, envelopes and evidence folders are
  retired together per session by a later retention task. Retiring a
  session's step rows must be a single keyed delete.

## Accepted costs

- A late reader cannot page text chunks or tool calls from VIA. It reads the
  envelope, the durable events, the step rows and the agent's own transcript.
- If the vendor deletes its transcript, the conversation detail is gone.
- Diagnosing a VIA decode failure relies on the saved message. The exact
  traffic of successful turns is not kept; adapter development captures it
  with test tooling.
- Transcript paths follow each vendor's internal layout. VIA records them as
  hints, and each vendor task confirms them.
- Memory has no enforced ceiling; its worst case is measured, not guaranteed.
- Simple first, measure later **(owner, 2026-09-29 r16)**. Problems not yet
  observed get the simplest behaviour: uncapped stderr, a 1 MiB per-message
  cap, and one-transaction WAL overshoot. `via-d9o.2.3` measures each with
  every adapter, and hardening follows the data.
- Step count and live tokens are approximate across vendors. The envelope holds
  exact usage where the vendor reports it.
- This changes VIA API v1 before its first release: §3.7 `status`, §3.11
  `events`, §3.12 `logs`, `list` ordering, the request line cap, §6 event
  types, and §5 references to event `seq`.

## Process

1. Opus 5.5 high wrote the design against these requirements; Sol high
   reviewed it until SOUND (round 15).
2. Fable 5.1 high and GPT-6 Astra high reviewed it critically; the consolidated
   report is `critical-review.md`.
3. The owner reviewed the report and decided the r16 revisions above.
4. Opus 5.5 high revises the design once (round 16); Sol high reviews it once.
5. Then slice planning.
