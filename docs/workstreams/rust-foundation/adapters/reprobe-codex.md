# Codex app-server live re-probe report (via-5lr.4)

Worker: implementer (Opus 5.5 medium), 2026-09-30. Status: DONE_WITH_CONCERNS.
Saved by the coordinator from the worker's hand-back text (the harness rejected the worker's own Write).
Committed copy with absolute paths removed; raw evidence and scripts stay local in the gitignored `scratchpad/execution/adapter-reprobe/codex/`.

## Hand-back summary

- **Version:** `codex-cli 0.159.2` is installed; the packet pins 0.157.1. The model is `gpt-6-sol` (it is in the catalog) at effort `low`, the lowest the catalog lists.
- **Three most important drifts:**
  1. **The version gate is broken.** Codex shipped 0.157.1 → 0.158.0 → 0.159.0 → 0.159.1 → 0.159.2 in four days, and the standalone auto-updater silently re-pointed `~/.local/bin/codex` at 16:03 today. Every bound-bearing spawn is now `untested` under C2 A2, and any exact pin will go stale within days.
  2. **The v2 schema hash changed** (`2719fccd…` → `81a88c04…`), but the protocol surface VIA uses is unchanged. The only differences are additions (`CodexErrorInfo` gains `tooManyDenials`/`flexUnavailable`, `PlanType` gains `promax`, the `thread/items/list` cursor accepts more forms), removal of plugin fields VIA doesn't use, and one doc-string change on `Turn.error`. No methods, notifications, server requests or thread/turn fields changed.
  3. **Correction to the September evidence, true of both versions:** the September C4b claim "the model never attempted the denied write" is wrong. The model did attempt it through a code-mode `exec` call, which Codex's own log records, but the wire carried no tool item. Today this reproduced three times.
- **Does it still fit C2 as written?** Mostly. Steer, interrupt acknowledgement, the tool still alive 65 s after interrupt, unsubscribe detach, per-thread event separation, stored resume with `excludeTurns`, stdin-close behaviour, never-ask and the environment case all behave as in September. Four things don't fit cleanly:
  - Sandbox-denied fast commands emit no tool item.
  - Usage arrives once per model request, not per turn.
  - Commentary text is mixed into final text.
  - Auth and bad-model failures surface only after the turn is accepted.
- **Owner decisions needed:**
  - (a) A version policy for an auto-updating vendor: a range, or pinning the absolute path of a release binary (old releases stay on disk).
  - (b) Whether VIA should launch with `--disable hooks` and how to handle inherited user/project config (MCP servers, hooks, trusted-project `.codex/`).
  - (c) The C2/packet rules for final text (`phase:"final_answer"` only), turn usage (sum of the turn's `last` values), and auth classification (`httpConnectionFailed{401}`).
  - (d) Whether `action.denied` and tool quiescence can be claimed at all, given denied actions that never appear on the wire.
  - (e) Which model the owner means: the catalog now defaults to `gpt-6.1-sol`, while I used `gpt-6-sol` as briefed.

**Concerns about the run itself:**
- Codex's project `sessionStart` hook ran `bd prime` once, in case c6 (September's runs did the same in every case, unreported). I disabled hooks for every later case. Because `.beads/` was already modified before I started, I can't say whether that one run wrote anything.
- The brief described the September login recipe as a private seeded `CODEX_HOME`. The September script actually used the owner's default `CODEX_HOME` plus a private `CODEX_SQLITE_HOME`. I followed what the script did, and nothing from the login was read or copied.

## Report

Run 2026-09-30 16:47–16:59 UTC on `codex-cli 0.159.2`, using app-server processes I started over stdio, model **`gpt-6-sol`**, effort **`low`**. Folder: `scratchpad/execution/adapter-reprobe/codex/`. Each case has `out/<case>/<stamp>/{result.json,raw.jsonl,process_trees.jsonl}`. The raw logs are private and unsanitized: they contain the machine name, an installation id and file paths, but no credentials.

**Budget:** 21 `turn/start` calls reached the backend, under the cap of 25. 17 were real model turns, 3 were refused by the backend with HTTP 400 (bad effort, bad model twice), and 1 was unauthenticated (401).

## 1. Version and shape

| Item | September / packet | Today | Changed? |
|---|---|---|---|
| Binary | 0.157.1 | 0.159.2 (`~/.local/bin/codex` → `~/.codex/packages/standalone/current` → `releases/0.159.2-…`) | yes |
| Release churn | – | `releases/` has 0.157.1 (Sep 26), 0.158.0 (Sep 28), 0.159.0 and 0.159.1 (Sep 29), 0.159.2 (Sep 30 00:49). The auto-updater re-pointed `current` at 16:03 today. The 0.157.1 binary is still on disk. | new fact |
| Process shape | One owned `codex app-server` per connection, shared by threads | Same. Each has children: `codex-code-mode-host` (own process group) and one `bwrap --new-session … codex-linux-sandbox` per tool (own process group). | no |
| Transport / framing | stdio JSONL; v1 `initialize` then `initialized`; v2 methods | Same. `ss -xp` shows only socket pairs inside our own server process, so there was no contact with the owner's `--managed-daemon` socket. | no |
| `initialize` response | `userAgent, codexHome, platformFamily, platformOs` | Same | no |
| CLI | – | `app-server --help` differs only in one WebSocket-auth wording; `daemon` and `proxy` already existed in 0.157.1 | cosmetic |
| Cold start | `initialize` took 0.14 s (Sept c6) | 38.1 s for the first `initialize` with a fresh `CODEX_SQLITE_HOME`; 0.10 s after that (`out/cat/20260930T164753…` vs `…164859…`) | new observation |

## 2. C2 mapping table

"Sept" means `rust-foundation-release/codex-evidence/` (0.157.1, `gpt-6-luna`). The case is in parentheses.

| C2 item | September | Today (exact evidence) | Changed |
|---|---|---|---|
| describe / version gate | Version 0.157.1; v2 sha `2719fccd…` | Version 0.159.2; v2 sha `81a88c04ae4984b16d73080f4109d0477682bc76c8adbe483e371175ce54c054`. `model/list` (no turn needed) returns `gpt-6-sol` with efforts low/medium/high/xhigh/max/ultra and default `medium`. The catalog default is now `gpt-6.1-sol`. | version |
| open_session | `thread/start` → `thread.id` | `thread/start {model,cwd,sandbox:"read-only",approvalPolicy:"never",approvalsReviewer:"user",ephemeral}` echoes `never`, `approvalsReviewer:"user"`, `sandbox:{type:"readOnly",networkAccess:false}` and cwd. `reasoningEffort` echoes the model default (`medium`), not the turn's effort (c6). `approvalsReviewer` is now covered by a live run. | no |
| Identity confirmation | Response `thread.id`, then `thread/started` | Same; `thread.sessionId` equals `thread.id` | no |
| StartTurn / acceptance | Paired response carrying `turn.id` | Response `{turn:{id,status:"inProgress"}}` arrives first, in the same read as `turn/started` (c1, `id:3`) | no |
| Steer | Matching ID → `{turnId}`; stale ID → -32600 | Matching → `{"turnId":…}`. Stale → -32600 "expected active turn id `stale-turn-id` but found `…`". Idle → -32600 "no active turn to steer". One `turn/started`; final text `BASE VIA_STEER_159` (c2). | no |
| Interrupt | `{}`, then `turn/completed` with `interrupted` | `{}` and `turn/completed interrupted` arrive in the same 2 ms read. `turn.error` is `null` and `items` is `[]` / `notLoaded` (c3). Idle → -32600 "no active turn to interrupt" (c7). | no |
| Tool quiescence | `sleep` alive at 5/30/60/65 s; no `item/completed` | Identical: PID 402727 alive at 5/30/60/65 s with no `item/completed`. In c4, thread A's interrupted `sleep 12` item also never completed. | no |
| Close | `thread/unsubscribe` detaches; thread B unaffected | `{"status":"unsubscribed"}`; thread B finished and wrote `b_marker.txt`; no events leaked across threads (c4) | no |
| recover / resume | Persistent-thread resume by exact ID | After unsubscribe, `thread/resume {…,excludeTurns:true}` returns the exact ID with `turns:[]`. The next turn recalled `VIA_RESUME_159`. The resumed thread reports effort `low`, so turn overrides persist as thread defaults. A bogus ID → -32600 "no rollout found…". The probe thread was deleted and its rollout file confirmed gone (c5). `excludeTurns` is now covered live. | no |
| Losing an owned stdio server | stdin close → exit 0; tool gone within 5 s | Same; the code-mode host and bwrap groups are gone too (c0) | no |
| progress | Items and deltas | Same item types. `commandExecution.source` is `"unifiedExecStartup"`; `processId` is an opaque value, not an OS PID. | no |
| Completed final text | "Each completed agentMessage" | `agentMessage` carries `phase:"commentary"` or `phase:"final_answer"`. `turn/completed.items` holds only the final answer (`itemsView:"summary"`). This was already true in September. | pre-existing |
| Vendor terminal | `turn/completed` status | completed / interrupted / failed, with `turn.error {message,codexErrorInfo,additionalDetails,misalignment}` | no |
| usage | `last` / `total` | `thread/tokenUsage/updated` arrives once per model request (2 per turn in c1/c4b/c10). `last` covers that request; `total` is cumulative for the session. The sum of a turn's `last` values equals the change in `total` (20522+20613 = 41135). New fields: `cacheWriteInputTokens`, `modelContextWindow`. | new reading |
| denials | None observed | A slow denied command emits a `commandExecution` item with `status:"failed"`, exit 1 and "Read-only file system" (c10). Fast denied writes emit no item at all (c1, c4b, c4). There is no structured denial field. | new observation |
| declines (server requests) | Zero requests; bodies checked against schema | Zero server requests anywhere, even when the model called `request_user_input_async` under `never` (c1 log). The six no-grant reply bodies still validate against 0.159.2 schemas (`decline-validation.json`). Whether Codex accepts them live is still unproven. | no |
| warnings | – | `warning` "Model metadata for `…` not found…" (c7); `warning` "Falling back from WebSockets to HTTPS … 401" (c8); `configWarning` "project-local config/hooks disabled until trusted" (c8) | new coverage |
| Definite rejection | -32600 | Every refusal is -32600 with free text: `thread not found: <id>`, `no active turn to steer`/`…interrupt`, invalid params, and even an unknown method ("unknown variant"), which is not -32601 (c7) | new coverage |
| Uncertain submission | Not induced | Not induced | – |
| Auth failure | Not tested | With an empty private `CODEX_HOME`: `account/read` → `{account:null,requiresOpenaiAuth:true}`. `thread/start` and `turn/start` are accepted. Then 5× `error {willRetry:true, responseStreamDisconnected{401}}` and a WebSocket→HTTPS `warning`. After ~15 s, `turn/completed failed` with `codexErrorInfo:{httpConnectionFailed:{httpStatusCode:401}}` — not `"unauthorized"` (c8). | new coverage |
| Bad model | Not tested | `thread/start` is accepted with a `warning`; `turn/start` is accepted; then `failed` with `codexErrorInfo:"other"` and HTTP 400 "…not supported when using Codex with a ChatGPT account". A per-turn model override behaves the same (c7). | new coverage |
| Bad effort | – | Accepted, then fails with a 400 that lists the backend's efforts: none, minimal, low, medium, high, xhigh, max. The catalog's lowest is `low`. | new coverage |
| Environment (C6) | 7 variable names, tool-free turn | Same result with `VIA_ENV_159` (c6) | no |

## 3. Drift against `docs/specs/vendors/codex.md` and the 0.157.1 evidence

I regenerated the schema with `codex app-server generate-json-schema --out schema-0.159.2`: the same non-experimental form, 314 files, the same file set as 0.157.1. The diffs are in `schema-diff-v2.txt` and `schema-diff-root.txt` (script `schema_diff.py`).

| Drift | Class |
|---|---|
| Binary 0.157.1 → 0.159.2 while the packet §1 and C2 A2 pin an exact set; four releases in four days with auto-update | breaking (version gate) |
| v2 sha `2719fccd…` → `81a88c04…` | breaking (pin) |
| `CodexErrorInfo` gains `tooManyDenials` and `flexUnavailable`; the C2 class hints don't cover them, so they fall to `vendor_error` | additive |
| `PlanType` gains `promax`; `ListMcpServerStatusParams.serverName`; the `thread/items/list` cursor accepts an item anchor | additive, unused by VIA |
| `PluginSummary.extensions` and 8 `Plugin*` definitions removed | breaking for plugin clients; cosmetic for VIA |
| `Turn.error` description now says "failed or interrupted"; observed `null` on interrupt | cosmetic, worth watching |
| Methods, server requests, notifications, thread/turn/steer/interrupt/resume/unsubscribe params and responses, v1 `initialize`, decline schemas | unchanged |
| Feature flags: `instant_interrupt` added (under development, off); `write_stdin_approval` now stable and on; `guardianv2.thread_context` removed | additive (`instant_interrupt` matters for P7) |
| Runtime behaviour of steer, interrupt, unsubscribe, stdin close, resume and the environment case | no drift |

**Corrections to the September evidence** (both are also true of 0.157.1):

- **C4b's "no attempt" conclusion was wrong.** The September log database (`probe-sqlite-state/logs_2.sqlite`) shows a code-mode `exec` call in the read-only turn `01a0dd94-9597…`, and no tool item appeared on the wire.
- **The owner's hooks ran inside every September probe server.** That includes user hooks from `~/.codex/hooks.json` and the project `.codex/hooks.json`, whose `sessionStart` hook runs `bd prime`. Today they ran once (c6), then I used `--disable hooks` for every later case.

## 4. Interface pressure

1. **Some tool actions never appear on the wire (fact).**
   - Three read-only write attempts produced no `commandExecution` or `fileChange` item: c1 (log shows `ToolCall: exec` / "code-mode host operation completed … exec"), c4b and c4's changed-bound turn. The model reported "Read-only file system" each time.
   - The same write preceded by `sleep 8` did produce an item (`failed`, exit 1, c10).
   - A fast failure that the sandbox did not cause (`ls /nonexistent`, c11) also produced an item (exit 2).
   - Inference: fast sandbox-denied commands skip item emission.
   - Impact: C2's `action.denied` has no wire evidence for them. The packet §6 rule "complete history and no open items → quiescent" assumes every execution becomes an item, which is shown false for denials and unknown for other code-mode operations.
   - The read-only bound itself was observed enforced with wire evidence (c10), which is partial evidence for `via-5lr.3.4`.

2. **Usage accounting.** `last` covers one model request, and a turn often has two or more. The packet §7 rule "replace snapshots, never sum" therefore under-reports a turn. Per-turn usage is the sum of the turn's `last` values (equal to the change in `total`).

3. **Final text.** Completed `agentMessage` items include `phase:"commentary"` ones, so the packet §5 / C2 §4 mapping would fold commentary into the final text. Filtering on `phase:"final_answer"`, or using `turn/completed.items` with `itemsView:"summary"`, separates them.

4. **Error classification.**
   - Every JSON-RPC refusal is -32600, so telling `no_active_turn`, `turn_mismatch`, a missing thread and invalid params apart means matching message text.
   - Auth and bad-model failures come after the turn is accepted. Auth takes about 15 s of retries and reports `httpConnectionFailed{httpStatusCode:401}`, which the C2 hint "unauthorized → auth" misses.
   - Two checks could catch these before submission: `account/read` (`requiresOpenaiAuth:true`) and the `model/list` catalog. C2's `describe` starts nothing, so these would need a check at Host handshake time or a cached catalog (inference).

5. **Inherited config.** By default a VIA-launched server loads the owner's `~/.codex` config, user hooks and MCP servers (`openaiDeveloperDocs` and `codex_apps` start per thread). It also reaches the keyring over D-Bus for MCP OAuth refresh, and loads trusted-project `.codex/` hooks and config for any cwd inside a trusted repo.
   - None of this is covered by `config_hash`.
   - `--disable hooks` works as a startup argument.
   - An empty `CODEX_HOME` disables project config but also removes the login.

6. **Version churn.** Exact version sets go stale within days, and auto-update swaps the binary with no signal. Old release binaries stay on disk.

7. **Cold start.** The first `initialize` with a fresh SQLite home took 38 s (single observation). Host handshake deadlines need to allow for it.

8. **Cleanup verbs.** The server accepts 170 methods: 104 in the stable schema, 63 experimental-only (e.g. `thread/backgroundTerminals/terminate`, `process/kill`, `turn/settings/update`) and 3 legacy (`server-accepted-methods.txt`). Per-thread terminal cleanup exists behind `experimentalApi`; it is untested, and the packet forbids experimental capabilities (inference: it may matter for P7).

9. **Still fits unchanged:** steer, interrupt acknowledgement, unsubscribe detach, per-thread event separation on a shared server, stored resume, and stdin-loss behaviour.

## 5. Commands used

All from `scratchpad/execution/adapter-reprobe/codex/`:

```sh
codex --version
codex app-server --help > help-app-server-0.159.2.txt
~/.codex/packages/standalone/releases/0.157.1-x86_64-unknown-linux-musl/bin/codex app-server --help > help-app-server-0.157.1.txt
codex features list > features-0.159.2.txt      # 0.157.1 binary likewise
codex app-server generate-json-schema --out schema-0.159.2
codex app-server generate-json-schema --experimental --out schema-0.159.2-experimental
python3 schema_diff.py ../../rust-foundation-release/codex-evidence/schema-0.157.1/codex_app_server_protocol.v2.schemas.json schema-0.159.2/codex_app_server_protocol.v2.schemas.json > schema-diff-v2.txt
python3 validate_declines.py
for c in cat c6 c1 c2 c3 c4b c4 c10 c11 c0 c5 c7 c8 c9; do timeout 200 python3 run_codex.py $c; done
python3 msgs.py out/<case>/<stamp>/raw.jsonl; python3 decode_raw.py out/<case>/<stamp>/raw.jsonl
```

- **Script:** `run_codex.py` is adapted from the September script (original kept as `run_codex_sept_original.py`).
- **Changes from September:**
  - pinned to 0.159.2 and its schema sha;
  - `gpt-6-sol` at `low`;
  - `approvalsReviewer:"user"`;
  - `--disable hooks` (override with `VIA_PROBE_HOOKS=on`);
  - resume uses `excludeTurns:true` after an unsubscribe;
  - new cases `cat` and c7–c11.
- **Environment:**
  - Every case: the inherited environment plus `CODEX_SQLITE_HOME=probe-sqlite-state/`.
  - c6 only: the 7-name allow-list.
  - c8 only: `CODEX_HOME=private-empty-codex-home/<stamp>`, with no login.
- **Hygiene:**
  - I did not read or copy any auth or config content.
  - The owner's daemon (PIDs 604323 and 3030294) was not touched.
  - No probe processes remain.
  - The one persistent thread was deleted and its rollout file confirmed gone.
  - No tracked files were edited; `git status` shows only the `.beads/*.jsonl` changes that were there before I started.
