# Task 4 requirements: events, progress and storage

Status: draft for owner review (2026-09-29). It supersedes the "every event is
durable and followable" assumption behind T4-0 rounds 1–4.

## Decision

VIA's callers are programs and model orchestrators. No human watches a turn
live. A caller needs to know when a turn ends, the result, and which step the
agent is on (so it can steer or cancel). For post-mortem detail it reads the
raw log. VIA does not store or stream per-message detail.

## Terms

- **Step**: one model call inside a turn. The model writes text and may request
  tools, the tools run, and the next step reads their results.
- **Observation**: what an adapter reports from one vendor message. Core turns
  an observation into a durable event, a progress update, or envelope
  accumulation.
- **Event**: a durable record in SQLite with a per-session `seq` (R1).
- **Event trace**: the ordered events of one turn.

## Requirements

**R1. Durable events.** SQLite stores only lifecycle, control and safety
events, plus the envelope:
- `session.opened`, `session.reopened`, `session.closed`;
- `turn.queued`, `turn.submitted`, `turn.started`, `turn.ended`, `turn.revised`;
- `cancel.requested`, `cancel.settled`, `steer.delivered`;
- `action.denied`, `vendor.request_declined`;
- `process.exited`, `server.lost`, `raw_log.incomplete`, `warning`.

Rule: an event is durable when crash recovery or the envelope depends on it.

**R2. Not stored or streamed.** `assistant.text`, `reasoning.summary`,
`tool.started`, `tool.ended`, `usage.updated`, `file.changed` and
`vendor.other` stop being events. The raw log keeps their exact bytes.

From a vendor message, VIA reads only what these requirements need:
- the message type;
- tool names;
- usage numbers;
- the terminal fields the envelope already carries.

It does not interpret tool inputs or outputs.

**R3. Progress snapshot.** `status` on a running turn returns a small snapshot,
held in memory and updated as vendor messages arrive:

| Field | Meaning |
|---|---|
| `current_step` | number of the step running now. It goes up by one when the model produces output after tool results, derived the same way for every vendor. |
| `phase` | `model` or `tools` |
| `running_tools` | names of the tools running now; no inputs or outputs |
| `last_activity_at` | time of the last vendor message |
| `tokens` | approximate running total, updated once per step, labelled with its scope; about 95% accuracy is enough |

The snapshot ends with the turn. The envelope's `steps` and `usage` hold the
final figures.

**R4. Step history.** When a step ends, Core writes one row:
- the row is `(session_id, turn, step, started_at, ended_at, tokens)`;
- there is one `steps` table, keyed by session first;
- writes go through the existing single Store writer, and may be batched;
- there is no write per vendor message.

A caller can read the step rows of any turn, running or finished. After a
daemon crash, rows survive up to the last completed step. The step in progress
is recoverable only from the raw log.

**R5. Caller interface.**
- `wait` blocks until the turn ends.
- `status` returns the progress snapshot and step history (R3, R4); callers
  poll it.
- `events` pages the durable events. There is no follow stream.
- `logs` returns raw log excerpts, undecoded; the calling model reads vendor
  JSON.

**R6. Envelope.** Unchanged in content. It still carries:
- final text;
- steps and usage (exact where the vendor reports it);
- denied actions and declined requests;
- cost and the raw log spans.

It keeps its 1 MiB accumulation bound.

**R7. Memory and disk bounds stay** for the parts that remain:
- message splitting and its per-message cap;
- the raw log;
- the envelope accumulation;
- blob files for large prompts;
- the snapshot and the step rows.

Bounding strategy (owner decision 2026-09-29, after round 7 found exact
accounting was not converging):
- **Memory:** one 128 MiB pool. Each kind of buffer is charged a
  conservative flat amount. A request that cannot be charged is refused with
  a named overload error. The measured RSS gate verifies the whole.
- **Disk:** SQLite and raw/blob files get separate hard budgets. These are
  checked against actual file sizes when a turn is admitted, and admission
  stops early to leave headroom for running turns. A checkpoint policy bounds
  the WAL. Hitting the hard limit mid-turn fails that turn visibly.
- **Shared-server `logs`:** Task 4 covers private connections only. The
  per-session split of shared Codex and OpenCode raw data belongs to those
  adapter tasks, with D4 session isolation as a fixed constraint.

`via-d9o.2.3` verifies these bounds thoroughly in end-to-end testing.

## Non-goals

- Human live viewing.
- A progress stream.
- Decoding the raw log on request, or decoding raw logs from an older VIA
  version.
- Reporting changed files. The caller tracks file changes with git, using one
  worktree per concurrent agent.
- Retention and pruning. Step rows, events, envelopes and raw logs are retired
  together per session by a later retention task. Retiring a session's step
  rows must be a single keyed delete.

## Accepted costs

- A late reader cannot page text chunks or tool calls. It reads the envelope,
  the durable events, step rows and raw excerpts.
- Step count and live tokens are approximate across vendors. The envelope holds
  exact usage where the vendor reports it.
- This changes VIA API v1 before its first release: §3.7 `status`, §3.11
  `events`, §6 event types, and §5 references to event `seq`.

## Process

1. Opus 5.5 high writes the design against these requirements, starting from
   T4-0 round 4 and removing what no longer applies.
2. Sol high reviews, and the loop repeats until SOUND.
3. Fable 5.1 high and GPT-6 Astra high review critically; the orchestrator
   consolidates one report.
4. The owner reviews the report before implementation planning.
