# OpenCode serve live re-probe report (via-4sw.5)

Worker: implementer (Opus 5.5 medium), 2026-09-30. Status: DONE_WITH_CONCERNS.
Saved by the coordinator from the worker's hand-back text (the harness rejected the worker's own Write).
Committed copy with absolute paths removed; raw evidence and scripts stay local in the gitignored `scratchpad/execution/adapter-reprobe/opencode/`.

## Hand-back summary

**Installed version:** 1.18.32. The binary SHA-256 and the served `/doc` hash are byte-identical to the packet's pin, so the vendor itself has not drifted. Every drift below is a place where live behaviour contradicts or extends what the packet assumed.

**Three most important drifts:**
1. **Terminal rule is wrong (breaking).** Some completed turns end with `finish:"tool-calls"`:
   - structured output, where the answer is in `info.structured`;
   - a rejected permission, which halts the turn with a tool part in `status:error`.
   
   Also, a prompt sent while a turn is busy gets merged into that turn, so the first user message never gets its own terminal answer.
2. **Tool children escape the server's process group (breaking).** The bash tool runs in its own process group. After I SIGKILLed the server's group, `sleep 25` survived, reparented to the subreaper; I stopped it myself. A normal abort does kill the child.
3. **Structured output breaks message reads (vendor defect, breaking).** After any `format: json_schema` turn, `GET /session/{id}/message` (any page containing that user message) and a single-message GET of it both return 400 "Expected OutputFormatJsonSchema". This reproduced in 3 sessions. The packet's output_schema-to-`format` mapping can't be used as written.

Also notable:
- A 204 does not validate the model. An unknown model gets a 204 and then only a `session.error` with no messageID, named `UnknownError`.
- A provider 401 arrives as `APIError statusCode 401`, not `ProviderAuthError`.
- On 429 the vendor retries 5 times by itself, visible only as `session.status {type:"retry"}` events.
- An unsupported `variant` is silently accepted.
- Worktree `.claude/skills` are loaded even with `--pure` and `OPENCODE_DISABLE_PROJECT_CONFIG=1`.
- The remote free-model catalog changed: `mimo-v2.5-free` and `muse-spark-1.2-contributor-free` are gone; `longcat-2.5-preview-free` and `space-bunny-free` are new.

**Closed items from the packet's unproven list:**
- **Multi-step usage:** there is one assistant message per model call. Turn usage is the sum over that turn's assistants. Session tokens are cumulative. Subagent (task) child-session tokens are not included in the parent's totals.
- **Active abort with tool cleanup:** the child was dead right after the 200 `true`. The aborted tool part reports `status:"completed"` (not error).
- **Continuity across a server restart:** proven over the same private DB.
- **Never-ask precedence:** holds for external-directory reads. `question: deny` removes the tool rather than producing a request to decline.
- **Permission reject flow:** observed end to end (`permission.asked` → reply reject → tool error).
- **Hostile project config:** a project `opencode.json` and `.opencode/opencode.json` in the working directory were ignored.

**Fit with C2:** the operations still map, but not cleanly as written:
- Terminal detection must combine session idle with several correlated assistant messages.
- Uncorrelated `session.error` can only be attributed to the active turn while exactly one turn is in flight.
- `Cleanup::Quiescent` via process-group absence is unsound for this harness.
- Steer does exist: a v1 prompt sent while busy is injected at the next step, which matches C2's `queued_after_current_tool`. The v2 `delivery:"steer"` API is different: it runs a separate conversation on the same session ID, and v2 `/interrupt` does not stop a v1 turn.

**Decisions for you:**
1. Declare `output_schema` unsupported on 1.18.32, or accept the read defect with a workaround. Filing the vendor bug needs your approval.
2. Use busy-prompt injection as steer, or keep steer unsupported.
3. How to clean up tool processes that escape the server's process group (Host/runtime §5.2).
4. Whether `APIError 401` may map to C1 `auth`.
5. Whether ambient worktree skill discovery is acceptable; I found no switch that suppresses it.

**Budget and isolation:**
- Exactly 25 prompt submissions: 21 reached the free model `opencode/mimo-v2.6-flash-free` (still free), 2 were refused inside OpenCode before any provider call, and 2 went to a loopback fake provider.
- Every run used a private HOME, all XDG roots, a private DB and a generated password. The password files were deleted afterwards.
- Nothing I started is still running.
- The owner's OpenCode DB and WAL were unchanged. Its `-shm` file changes because of the owner's own `opencode-sqlite-reader.cjs` (Orca relay), confirmed with no probe process alive.
- No tracked files were edited, no `bd`, no commit.

## Report

Scope: evidence only. Runs are under `scratchpad/execution/adapter-reprobe/opencode/`:
- `runA/`: main run, three server generations over one private DB.
- `runF/`: a hostile project config plus a loopback fake provider.

Model: `opencode/mimo-v2.6-flash-free`, still listed at zero cost; it worked for every turn. Budget: 25 submissions in total:
- 21 reached the free model;
- 2 were refused inside OpenCode before any provider call (unknown model, unconnected provider);
- 2 went to a loopback fake provider (synthetic 401 and 429).

Isolation settings for every server:
- fresh private `HOME`, all XDG roots and `TMPDIR`;
- absolute private `OPENCODE_DB`;
- generated `OPENCODE_CONFIG` and `OPENCODE_CONFIG_DIR`;
- `OPENCODE_DISABLE_PROJECT_CONFIG`, `OPENCODE_DISABLE_AUTOUPDATE` and `OPENCODE_DISABLE_PRUNE` set to 1;
- a fresh 256-bit Basic password (the files were deleted after the run);
- `--pure`, loopback only, an explicit port.

No login was used and no credential file was read. Cleanup records are `runA/stop2.json`, `runA/stop3.json` and `runF/stop1.json`. runA generation 1 was killed on purpose for the crash case.

## 1. Version and shape

| Item | Packet (09-26) | Today | Changed |
|---|---|---|---|
| Binary | 1.18.32, SHA-256 513f500a…0080 | same | no |
| Served /doc | 478,968 B, SHA-256 46db9860…aa5c | identical | no |
| Process shape | one owned `opencode serve` per VIA session | same; healthy in 0.76 s | no |
| Transport | HTTP JSON + SSE `/event`, Basic auth | same; 401 unauthenticated and with a wrong password | no |
| SSE framing | `data:` lines only; heartbeat 10 s | same; heartbeats 10.0 s, sometimes 11.9 s | no |
| Free catalog | 8 models incl. mimo-v2.5-free and muse-spark-1.2 | 8 models: those two gone; longcat-2.5-preview-free and space-bunny-free new; default still big-pickle | yes (remote catalog) |

## 2. C2 mapping

Where the evidence is: turn records are in `runA/turns/<name>.json`, raw SSE in `runA/sse{1,2,3}.raw` and `runF/sse1.raw`, and HTTP calls in `run*/calls.jsonl` (no Authorization header logged).

- **describe / version gate:** health returns 1.18.32. `GET /provider` returns the remote catalog (6.5 MB) and per-model `variants`; mimo has none.
- **open_session:** `POST /session?directory=` with `agent:"via"` and the packet's exact 4-rule array. The readback returns the identical ordered array and directory (`runA/sessA.json`).
- **StartTurn / acceptance:** 204 in 7–19 ms.
  - The caller-supplied `messageID` becomes the user message ID.
  - Every assistant message has `parentID` equal to that ID.
  - The SSE user `message.updated` echoes `system` and `model`.
  - A 204 does **not** validate the model.
- **Definite rejection:**
  - `prompt_async` on an unknown session: 404 `NotFoundError`.
  - Bad part type: 400 `BadRequest kind:Payload`.
  - Malformed `messageID`: 400.
  - None of these create a message.
- **Uncertain submission / crash:** after a server SIGKILL during a tool call, then a restart, the vendor never reconciles the old turn. A later turn on the same session works (`crash_after_restart.json`, `t22_after_crash.json`).
  - The assistant is never marked completed.
  - The tool part stays `running` forever.
  - `/session/status` shows `{}` (idle).
- **Bad model:** 204, and the user message is persisted, but no assistant is ever created. What follows (same for an unconnected provider, `t14_badmodel.json`):
  - two `session.error` events with no messageID, both named `UnknownError`;
  - the first says "Model not found"; the second carries a `ProviderModelNotFoundError` stack;
  - then busy → idle.
- **Auth failure (fake 401):** the assistant and `session.error` both carry `APIError` with `statusCode:401`, `isRetryable:false`, the provider's response headers and body, and the provider URL. It is not `ProviderAuthError` (`runF/sse1.raw`, `runF_logs/fake_provider.jsonl`).
- **Rate limit (fake 429):** 5 internal retries, visible only as `session.status {type:"retry", attempt, message, next}`, then `APIError 429 isRetryable:true`. That is 6 provider requests for one submission.
- **Progress:** `message.part.updated` for step-start, reasoning, text, tool and step-finish, plus `message.part.delta` for text and reasoning. Tool parts go pending → running → completed/error with `callID`, `input` and `metadata.exit`.
- **Final text / vendor terminal:** a plain turn ends with an assistant at `finish:"stop"`. Three other terminal shapes exist:
  - (a) Structured output ends with an assistant at `finish:"tool-calls"` carrying `info.structured`, produced through a `StructuredOutput` tool.
  - (b) A permission reject ends with an assistant at `finish:"tool-calls"`, a tool in `status:error`, then idle with no further assistant.
  - (c) A v1 prompt sent while busy merges into the running turn: the final assistant's `parentID` is the second user message.
  
  In the abort case, SSE `idle` and `session.idle` arrive before the final tool-part update and the terminal `message.updated`.
- **Usage:**
  - Each model call produces its own assistant message with exactly one step-finish mirroring it.
  - For each message, `total = input + output + reasoning + cache.read`, and `input` excludes cached input.
  - Turn usage is the sum over the turn's assistant messages.
  - Session `tokens` is cumulative (verified: 11072/72/37/43136).
  - Cost was 0 throughout.
  - Task (subagent) child-session tokens (9,856 input) are not included in the parent's messages or session totals (`t03_multistep`, `t23_task`).
- **Tool quiescence / Interrupt:**
  - `/abort` returns 200 `true` in 15 ms; the `sleep` child was already dead at the first check.
  - The aborted tool part shows `status:"completed"`, output "User aborted the command", `exit:null`.
  - `session.error MessageAbortedError` arrives before idle.
  - `/abort` on a nonexistent session also returns 200 `true`.
  - v2 `/api/session/{id}/interrupt` returns 204 but does not stop a v1 turn (`t08`).
- **Tool process group:** the bash tool child runs in its own process group (pgid equals its pid, ppid is the server). It survived SIGKILL of the server's group, reparented to the subreaper `Relay(1698398)` (`desc_crash.json`, `crash_survivor.txt`).
- **Steer:**
  - A v1 `prompt_async` sent while busy returns 204, persists at once, and is injected at the next step boundary. The final answer was "SLEPT_T07_V1BUSY BUSY_INPUT_SEEN".
  - v2 `/api/session/{id}/prompt {delivery:"steer"}` returns 200 with `admittedSeq`, but it runs a separate, concurrent v2 agent loop with its own message store and no v1 context. It answered "Standing by for the actual task" (`t06`, `v2hist.json`, `v2msgs.json`).
  - `session.next.*` events do appear on v1 `/event`.
- **Close:** there is no close verb. `DELETE /session/{id}` exists but was not used.
- **Resume / recover:** continuity holds across a server restart (new generation, port and password; same DB). The GET readback matches ID, directory and permission array (`t17`).
- **Identity:** the session ID is returned synchronously on create and appears on every event.
- **Denials / declines:**
  - The flow: `permission.asked {id:"per_…", permission, patterns, metadata, always, tool:{messageID, callID}}` → `POST /permission/{id}/reply {"reply":"reject"}` → 200 `true` in 2 ms → `permission.replied` → the tool errors with "The user rejected permission to use this specific tool call." The turn then stops.
  - `question: deny` removes the tool from the model's tool list, so there is no request to decline (`declines.jsonl`, `t13`, `t12`).
- **Never-ask precedence:**
  - Agent rules include `external_directory: ask`, `doom_loop: ask` and `read *.env: ask` before the generated config's `*: allow`.
  - A read of `/etc/hostname` ran with no permission request (`t04`).
  - The task child session gets its own array `[question, plan_enter, plan_exit, task: deny]` without the parent's `*: allow`; its effective allow comes from the generated global config (inference from rule order).
- **Warnings:** no vendor warning channel was seen; `retry` status is the only retry signal.
- **Structured output:**
  - It works: `info.structured={"answer":"blue","n":7}`.
  - Afterwards, the message-list page that contains the format-bearing user message and a single-message GET of it both return 400.
  - Reproduced with `retryCount:0`, with it omitted (the vendor stored 2), and with a minimal schema. `limit=1` still worked.
  - Not fully characterised: one page decoded in session A while a larger one failed.
- **Effort:** `variant:"high"` on mimo (which has no variants) is accepted silently and recorded.

## 3. Drift against the packet and September evidence

The vendor is byte-identical, so none of these are version drift; they are differences from what the packet assumed or claimed.

1. **Breaking:** packet §4's terminal rule does not hold. Terminals occur at `finish:"tool-calls"`, and a busy prompt leaves the first input without a terminal of its own.
2. **Breaking:** tools run in their own process groups and outlive a kill of the server's group. Group absence does not prove quiescence.
3. **Breaking:** a `format: json_schema` turn breaks the message reads that §4 reconciliation depends on (vendor defect).
4. **Breaking vs §7:** a provider 401 is `APIError statusCode:401`, not `ProviderAuthError`. Under the current mapping, C1 `auth` is unreachable for key rejection.
5. **Additive:** a 204 does not validate the model; the failure comes only through an uncorrelated `session.error` named `UnknownError`.
6. **Additive:** steer exists in two forms the packet treats as absent:
   - v1 busy-prompt injection at the next step (`queued_after_current_tool`);
   - v2 `delivery:steer`, which is a separate conversation, and whose v2 interrupt does not affect v1.
7. **Additive:** SSE event types the packet did not list:
   - `plugin.added` (45 on bootstrap), `catalog.updated`, `reference.updated`, `integration.updated`, `file.watcher.updated`, `session.diff`;
   - `permission.asked` and `permission.replied`;
   - `session.error` (no messageID);
   - `session.status retry`;
   - `session.next.*` from the v2 loop.
8. **Additive:** usage scope is now measured as in §2. Child sessions are excluded from parent totals.
9. **Additive:** the worktree resolves to the git root (the repository root).
   - 40 `.claude/skills` were loaded, each with an `external_directory: allow` rule added to every agent, and the `skill` tool is exposed, despite `--pure` and `DISABLE_PROJECT_CONFIG`.
   - The project `opencode.json` and `.opencode/opencode.json` were ignored.
   - A cwd `AGENTS.md` sentinel was reported absent, and input size (~10.78k) matched other turns. That no instruction files are injected is inference, not proof.
10. **Additive:** the free catalog changed remotely, with no binary change.
11. **Cosmetic:** `/abort` on a nonexistent session returns 200 `true`.
12. **Cosmetic:** an aborted tool part reports `status:"completed"`.

## 4. Interface pressure

Facts, with inferences marked.

- **Terminal detection:** C2 has one `vendor_terminal`, but OpenCode needs session idle plus correlation across several assistant and user messages, the `structured` field, and tool-error shapes. The adapter can own this within C2; the packet's rule is what must change.
- **Uncorrelated failures:** `session.error` carries only the session ID. Attributing it to the active turn is safe only while exactly one v1 turn is in flight and no v2 loop runs on that session (inference).
- **Acceptance vs validity:** a failure after a 204 arrives as a post-acceptance vendor error, not `StartRejected`. Checking the model and variant against `GET /provider` before submission would keep these as definite refusals before any vendor I/O (inference). The vendor does not validate variants at all.
- **Cleanup:** `Quiescent` via process-group absence (runtime §5.2) is unsound for this harness. Only the vendor's own abort killed the tool child. Force-close and crash recovery need descendant tracking by session, or they must report `Uncertain`. This is a Host/runtime question, not only an adapter one.
- **Structured output:** `output_schema` → `format` breaks the read path. Options (inference): declare it unsupported, validate the final text in Core instead, or rely on SSE-only reconciliation, which the packet forbids treating as durable.
- **Steer:** v1 busy-prompt injection fits C2's `queued_after_current_tool`. Using it conflicts with the packet's "no busy-prompt emulation" and with the one-active-turn gate, and blurs turn boundaries: the answer merges both inputs and the first input has no terminal of its own. The v2 steer API does not fit C2 at all.
- **Usage:** turn scope can be exact for the parent turn, but child sessions spend tokens outside it. C1 has no scope marker for "excludes delegated work".
- **Retries:** C2 has no "vendor is retrying" observation. Retry status should probably count as activity so the idle deadline is not tripped (inference). Whether 401 → `auth` is acceptable is still open.
- **Config isolation:** worktree skill discovery is not covered by `generated_config_digest`. It is covered only indirectly by `canonical_cwd`, since the worktree is derived from cwd.

## 5. Owner decisions

1. `output_schema` on 1.18.32: declare unsupported, or use a workaround. Filing the vendor bug needs your approval.
2. Use busy-prompt steer, or keep steer unsupported.
3. How to clean up tool processes that escape the server's process group.
4. Whether 401 `APIError` may map to `auth`.
5. Whether ambient worktree skill discovery is acceptable. It may also apply to `.opencode/skill*` according to source; unverified. No suppression switch was found.

## 6. Commands

Run from `scratchpad/execution/adapter-reprobe/opencode`. The scripts are adapted from the September `run_*_probe.py` scripts.

```bash
python3 oc.py start runA                       # owned server + SSE recorder, private env, generated never-ask config
python3 oc.py noauth runA GET /global/health [--wrong]
python3 oc.py call runA GET /doc --out doc.json   # also /agent /provider /config /skill /path /project/current
./mksession.sh runA A '<ordered permission array>'
python3 turn.py runA <sid> <name> "<prompt>" [--system S] [--format-json [--retry N] | --schema-json '<s>'] [--variant V] [--model M] [--no-wait]
python3 abort_probe.py runA <sid> <name> --sleep N [--v2-interrupt] [--busy-prompt v1|v2steer|v2queue] [--no-interrupt]
python3 decline_watch.py runA 60 &
python3 sse_summary.py runA/sse1.raw --since <epoch> --session <sid> --full session.error
# crash: turn.py … --no-wait; oc.py desc runA; kill -KILL -<server pgid>; ps <tool pid>
python3 oc.py start runA --reuse               # new generation, same DB
python3 fake_provider.py <port> runF_logs/fake_provider.jsonl &
python3 oc.py start runF --config config_fake.json --hostile-project
python3 oc.py stop runA
```

Turn ledger (25):
- t01 marker, t02 continuity, t03 multistep, t04 extdir, t05 abort;
- t06 (v1 + v2 steer), t07 (v1 + busy v1), t08 v2 interrupt;
- t09, t10, t11 format;
- t12 question, t13 permission reject;
- t14 bad model†, t15 unconnected provider†;
- t16 crash, t17 restart continuity;
- t18 fake 401‡, t19 fake 429‡;
- t20 ambient instructions, t21 variant, t22 after crash, t23 task.

† no provider call; ‡ loopback fake provider only.
