# Cross-harness adapter interface: C2 settled from the live harnesses

Status: Proposed, revision 9 (coordinator fixes for Sol r8), 2026-09-30, bead `via-jm4.22`. Author:
implementer (Opus 5.5 high). Revision 2 answered Sol review r1 (UNSOUND, 32
findings) and applied the owner decisions of 2026-09-30. Revision 3 answers Sol
review r2 (partly fixed findings 7, 8, 9, 17, 23, 30, 31 and new defects N1–N8)
and the later owner updates. Revision 4 answers Sol review r3 (R3-1 to R3-4).
Revision 5 answered Sol review r4. Revision 6 answers Sol review r5 and
applies the owner's OD3 redirect: processes a coding agent starts are the
agent's responsibility; VIA stops only the agent and reports leftovers.
Revision 7 answered Sol review r6 and the coordinator's decisions on report
destinations and persistence; revision 8 answers Sol review r7 (AD20's scan
and tests); revision 9 applies the coordinator's fixes for Sol review r8. The lifecycle research is cited as `LH`
([harnesses](lifecycle-harnesses.md)) and `LM` ([mechanisms](lifecycle-mechanisms.md)).
OD3's policy is decided, and on 2026-10-01 the owner chose leftover detection
A (conflict 4): the report-only marker scan.

Inputs:
- the live re-probes of 2026-09-30: [Claude Code](reprobe-claude-code.md),
  [Codex](reprobe-codex.md) and [OpenCode](reprobe-opencode.md), including the
  OpenCode `format` addendum;
- [C2](../../../specs/adapter-contract.md) and the packets under `docs/specs/vendors/`;
- C1 §3–§8, and runtime §2, §5, §6.1, §8 and §11.1;
- the T3 `[s1c.r2]` rules;
- the adapter-architecture council;
- the code at `a4f3cdb`.

Labels: **decided** (owner or an earlier approved decision); **Proposed** (this
design); **probe** (observed live on 2026-09-30); **help** (vendor `--help`
text, not run); **unverified**; **inference**.
Citations: `CC`, `CX` and `OC` are the re-probe reports, followed by the section
and case (for example `CC §2 c7`). `PK-CC`, `PK-CX` and `PK-OC` are the vendor
packets.

## 1. Summary for review

The live harnesses fit C2's operations. What breaks is narrower.
1. Core is built around the fake.
2. Several C2 and packet rules do not match live behaviour: the version gate,
   cleanup claims, synthetic errors, usage, and terminal and final-text rules.
3. All three harnesses inherit personal configuration.

This design keeps C2 as the one interface and keeps the S1 turn mechanics. It
moves every type an adapter produces into `via-adapters` and makes the fake an
ordinary adapter variant. It amends C2, C1, the runtime contract and the three
packets only where the evidence requires it.

### 1.1 Decisions adopted (decided)

| # | Decision |
|---|---|
| K1 | C2 stays the interface. Adapters are a compiled-in closed enum, one per harness. No plugins, no out-of-process adapter protocol, no bridged ACP. A native ACP adapter comes later, and the unused `agent-client-protocol` dependency is dropped now (OD5b, owner-approved). |
| K2 | Types an adapter produces are the public surface of `via-adapters`. Core consumes them and never names a harness. A CI literal guard backs this (§5.6). |
| K3 | The fake survives only as a test double for deterministic failure tests. |
| K4 | Live checks are opt-in and use cheap models: Claude Haiku; Codex `gpt-6-luna` at low or medium effort, never Sol; OpenCode free tier. They never run in the default gate. The re-probe scripts seed them. |
| K5 | A Claude vendor-synthetic API-error message (`is_api_error_message:true`) is not acceptance evidence. Auth and unknown-model failures settle consistently on all three harnesses (AD5). |
| K6 | Claude never-ask stays on `--permission-prompts none`. |
| K7 | Steer is native on Codex and `unsupported` on Claude and OpenCode, because both merge busy input into the running turn. |
| K8 | OpenCode `output_schema` is `unsupported` on 1.18.32 (the `format` read defect). Revisit when a release containing the upstream fix passes a cheap live check (OD4). |
| K9 | HTTP 401 or 403 in a vendor error → class hint `auth` on every harness. |
| K10 | Turn usage is the sum of per-model-call usage (Codex `last` values; OpenCode assistant messages). OpenCode child-session tokens fall outside the parent's totals, and this is recorded. |
| K11 | Codex final text comes only from `phase:"final_answer"` agent messages. |
| K12 | `action.denied` is best-effort. No capability promises complete denial reporting. |
| K13 | Owner decisions OD1, OD2 (including its default), OD4 and OD5 (§1.3). |
| K14 | Core validates structured output against the frozen schema with a JSON Schema validator dependency (owner-approved; C1 Q2). |
| K15 | Invariant 2 changes for OD5c (owner-approved). The replacement wording is in AD12; the coordinator applies it in S-SPEC. |
| K16 | OD3 (owner): processes a coding agent starts are the agent's responsibility. Soft stop is the vendor's own exit message; hard stop is S1's own-group stop, unchanged; a crash is observed. VIA never kills or manages leftovers; it reports them to the caller (AD20). A kill-or-keep option may come later. |
| K17 | Experimental vendor features are used where they help, each confirmed by the per-version live check, with `Uncertain` as the fallback when missing (owner, general policy). None is in a default path now. |

### 1.2 Decisions this design makes (Proposed)

**Kept from S1 or C2, no new mechanism.**
- **Connection admission.** Kept, applied per connection (AD16).
- **Terminal retained in the turn's end result**, as in S1 and `[s1c.r2]` (AD4).
- **Stop orders.** S1 stop orders (`force_at`, `close_by`) remain the
  pre-acknowledgement control bound.
- **Session observation channel.** C2's channel, attached at `open_session`, is
  kept (AD3).
- **Late durable events.** C1 §6.1 late durable events and tombstones are kept (AD4).
- **Recovery.** C2 `recover` is kept (AD3), fed by S1's Host anchor facts.
- **Core's C1 checks**, and the daemon's single read of `daemon.json` at startup.

**New, each tied to evidence.**

| # | Decision | Section |
|---|---|---|
| P1 | The turn lane is S1's single `run_turn` call. Acceptance is an observation. The end result carries the retained vendor terminal and the cleanup. Steer and close are driver methods. `open_session` is logical only, so vendor session creation happens in the first `run_turn`. | AD3 |
| P2 | Codex's post-acknowledgement cleanup window, `min(ack + 60 s, wall)`, is applied by the driver from a Core-supplied grace. It is separate from the stop order's pre-acknowledgement bound. | AD4 |
| P3 | Cleanup keeps its S1/C1 meaning (the agent's own group, or vendor-reported items on server routes). Leftovers are a separate best-effort report, delivered only where an existing surface ends the connection synchronously; detection is the report-only marker scan (owner chose A, conflict 4). | AD9, AD20 |
| P11 | The wall deadline keeps S1's path unchanged (Route `Deadline` failure, Core `dispose`); only the cleanup step after the failure is per route, and the failure carries two new facts, `acknowledged` and `shared`. | AD4 |
| P4 | Vendor version comes from the running instance's handshake. There is no separate version-probe process. | AD7 |
| P5 | The fake is an ordinary `Adapter::Fake` variant, reachable only with the explicit fixture configuration. It owns its own version policy. | §5.5 |
| P6 | The harness string resolves inside `via-adapters`. `SpawnParams.harness` becomes optional. | §5.2 |
| P7 | Per-harness configuration, including OD2's switches, is an opaque `AdapterConfig` read at daemon start. | §5.4 |
| P8 | Recorded-vendor replay is a new, bounded, test-only mode of `via-fake-agent`, keyed by `argv[0]`. | §8 |
| P9 | One auto-decline deadline, 5 s, for every adapter (C2 A6). The Codex and OpenCode packets' 1 s is withdrawn. | AD17 |
| P10 | Effort is refused before a receipt when the plan can judge it, and at submission (no vendor turn) when only a discovered catalog can. | AD18 |

### 1.3 Owner decisions

| # | Status | Decision and how it is applied |
|---|---|---|
| OD1 versions | **decided** | Details below the table. Applied in AD7, AC1 and §5.5. |
| OD2 personal setup | **decided**, default **approved** | Easy per-harness, per-category switches in config (hooks, MCP servers, plugins, skills, agents, instruction files) that the owner can flip. Default: hooks and MCP off, the rest on. A category with no verified switch is declared "not switchable" or "unverified until qualification"; a requested state VIA cannot apply or verify, in either direction, is warned with the effective state recorded; VIA never claims a suppression it has not qualified. Applied in AD13, AC7, §5.4 and table §5.4.1. |
| OD3 tool descendants | **decided** (owner redirect after the research) | K16. Soft stop per harness (AD19); hard stop S1's own-group stop; leftovers reported, never killed (AD20). The run-end kill (former OD3a) is dropped: report only. |
| OD4 OpenCode `format` defect | **resolved** | Real, and already reported upstream (sst/opencode#26929, #40169; fix PR #37541 open). Track it; file nothing. K8 stands with its revisit condition. VO2 records the sharper failure rule. |
| OD5a routine addition | **decided** | Adapter paths plus one enum variant, then a VIA rebuild and release (§6). |
| OD5b ACP | **decided** | Later. Dropping the unused `agent-client-protocol` dependency now is owner-approved (S-CORE). |
| OD5c adapter versions | **decided** | Resume is refused unless the adapter declares compatibility. Adapters normally declare compatibility with their previous version (AD12). |

**OD1 detail.**
- **Rule.** Every vendor version is supported by default. VIA refuses only when
  a startup or handshake check shows that something VIA relies on is actually
  broken. There is no range or series logic.
- **New releases.** For each new vendor release, the maintainers run the cheap
  live check (§8.4). VIA never runs checks on users' accounts.
- **Warning.** Until that check has run, results carry `vendor_version_untested`
  ("version not yet checked").
- **Recording.** The actual running version is recorded per turn.
- **Accepted risk.** Proceeding on unchecked versions is the owner's accepted
  risk. It includes limited bounds such as Codex read-only once `via-5lr.3.4`
  qualifies them (AD7 lists what a handshake cannot prove).

OD2 per-category switches are in §5.4.1.

### 1.4 Amendment list

| ID | Target | Change |
|---|---|---|
| AD1 | C2 §1 rule 1, §2 enum | Core names no harness. `Adapter { Claude, Codex, OpenCode, Fake }`. The literal guard backs this. |
| AD2 | C2 §2 types | Adapter surface: `Capabilities` with `Partial`, `RoutePlan` with harness and model, `Refusal` kinds, `ClassHint`, `StopReason`, `check_turn`, `models` and catalog sources. |
| AD3 | C2 §2 sketch and contract points | S1-shaped turn lane. Logical `open_session`. Session observation channel and `recover` kept. Optional expected identity. |
| AD4 | C2 §2 Interrupt, §4, §7 items 6 and 10 | The end result retains the terminal. Return conditions per route kind. Pre-acknowledgement and post-acknowledgement deadlines. `tool.quiescent` folded into the end result. Late durable observations kept. |
| AD5 | C2 §2 submission boundary, §7 item 5 | Synthetic messages are not evidence. A pre-model failure after acceptance is a `Failed` terminal. |
| AD6 | C2 §4 `progress`, `vendor_terminal`; §5; §6.2 usage | Per-call samples or a turn aggregate. Turn-wide ledger. Cost and vendor data. |
| AD7 | C2 A2, §5, §7 item 14 | OD1 version rule. Instance version per turn. What a handshake cannot prove. |
| AD8 | C2 §7 item 9, §6.2 | Denials are best-effort. A decline-caused denial is suppressed. |
| AD9 | C2 §2 Interrupt, Recover; §7 items 10, 11, 15 | Cleanup keeps the approved meaning; `Pending` only before its deadline; leftovers are outside cleanup. |
| AD10 | C2 A3, §6.2 Steer | OpenCode steer unsupported. |
| AD11 | C2 §6.2 class hints | 401/403 → `auth`; the observed codes. |
| AD12 | C2 §1 rule 2, RoutePlan; invariant 2 | Per-adapter version constant, compatibility declaration, the persisted identity; replacement wording for invariant 2. |
| AD13 | C2 §6.2 new row | Inherited-configuration switches (OD2); warn whenever the requested state cannot be applied or verified, in either direction. |
| AD14 | C2 A1, §4 | Remove the stale raw-log wording. |
| AD15 | C2 §6.2 Observations, Interrupt, Close | The observed Codex, OpenCode and Claude rows. |
| AD16 | C2 §3 and runtime §8 | Connection capacity per live connection, not per turn. |
| AD17 | C2 A6; PK-CX §4; PK-OC §5 | One 5 s auto-decline deadline. |
| AD19 | C2 §6.2 Interrupt and Close rows | Soft stop per harness; hard stop is S1's own-group stop. |
| AD20 | C2 §2, §4 new result fact | Best-effort leftover report on per-turn ends, server-stopping closes and `server_lost` turns; detection by the report-only marker scan (owner chose A, conflict 4). |
| AD18 | C2 §5 validation | Effort is validated in pure planning against the route's compiled values; discovered model-specific constraints are applied before vendor submission as a definite rejection. |
| AC1–AC10 | C1 | Version rule and `allow_untested`; cleanup meaning; adapter-version refusal; usage note; class rule and `submit_failed` scope; `fake`; warning `config_switch_unverified`; effort refusal at submission; `failure.data`; `leftovers`. |
| AR1–AR6 | runtime | Bootstrap environment; report-only marker scan (§5, §8, §10; option A); fake config ownership; per-connection admission; DTO placement; non-turn anchor owners for servers. |
| VC1–VC12, VX1–VX17, VO1–VO18 | vendor packets | §3.6. |
| §3.7 | every normative occurrence | The audit of text and tests that the amendments affect. |

### 1.5 Conflicts with recorded decisions (each flagged where amended)

1. **Version gate.** C2 A2; C1 P13 (summary row, line 73, and the recorded
   owner-approval row, line 846), §3.1, §3.2 and §8.1's "version refused
   (P13)"; PK-CC §1 and §3; PK-CX §1; PK-OC §3 have exact version sets,
   `allow_untested` and frozen executable identity. They are replaced by OD1
   (owner-decided): AD7, AC1, VC1, VX1, VO15.
2. **Invariant 2** ("the adapter version stays fixed" for a session). OD5c
   (owner-approved) allows a compatible newer adapter. AD12 gives the
   replacement wording; the coordinator applies it in S-SPEC.
3. **C2 §2's StartTurn reply lane and `tool.quiescent`** are resolved toward the
   approved S1 shape (AD3, AD4). The terminal stays in the end result, so
   `[s1c.r2]` is unchanged.
4. **No environment marker scan (owner chose A, 2026-10-01).** AR2's
   runtime targets and the §3.7 rows marked "conflict 4" forbid reading
   vendor environments or credentials, and `/proc/<pid>/environ` can hold
   credential values, so a scan touches invariant 1 through a transient
   buffer. The owner chooses: **A**, AD20's scan with AR2's narrowing; **B**,
   anchor-subreaper detection (LM §2), which reads no environment but changes
   the anchor and needs its own design (Sol r5 #1–#3); **C**, no report this
   release (every `leftovers` is `null`). Only AD20's option-A rows and tests
   (6)–(13), AR2, the three `x.3.4` marker-qualification clauses (§7), S-LEFTOVER's
   option-A start-bound ownership (`anchor.rs`, `protocol.rs`), this entry
   and those rows differ between them; delivery, shape and persistence
   do not depend on detection. Platform
   lines 269 and 328 stay unchanged: anchor marker proof stays control-based,
   and no persisted marker becomes liveness evidence (nothing from the scan
   is persisted).
5. **Runtime §6.1** "without copying the client environment" → AR1.
6. **PK-OC §7** ambiguous 401/403 → `vendor_error` is overridden by K9. A 403
   `FreeTierError` becomes `auth`.
7. **PK-CX §7** "never sum" and its `codex_usage_snapshot` test → K10 (VX4).
   PK-CX §5 "each completed agentMessage" → K11.
8. **PK-OC §3** native `output_schema` → K8. The PK-OC §4 terminal rule is
   rewritten (VO1).
9. **C2 §2's enum** has `Acp` → `Fake` for R1.
10. **The release build contains the fake** (§5.5). This is needed by
    `scripts/check-release-features.py`.
11. **PK-CX §4's and PK-OC §5's 1 s decline deadlines** are replaced by C2
    A6's 5 s (AD17).
12. **Runtime §2** places C1 DTOs in Core. Adapter-produced DTOs move to
    `via-adapters` (AR5).
13. **Runtime §8 and the S1 dispatch** reserve a connection slot per turn. It
    becomes per connection (AD16, AR4).
14. **C2 conformance item 4**: `open_session` returns identity. Codex and
    OpenCode now confirm identity in the first `run_turn` (AD3).
15. **Code, not a contract.** `adapter_version = CARGO_PKG_VERSION`
    (`api.rs:1840`) (AD12). Also code: `stop_outcome` (`engine/stop.rs:676`)
    gains `acknowledged`, and `stopped` (`engine/terminal.rs:320`) gives
    outcome `unknown` on a shared connection (AD4).
17. **PK-CX §1 "no experimental capabilities"** and `codex_pin_handshake`
    ("no experimental flag") are replaced by the owner's experimental-feature
    policy (K17); no experimental feature is in a default path now.
16. **C1 §4 `effort` row** ("unknown values refused"). A vendor effort value
    that only catalog discovery can judge is refused at submission as
    `failed(submit_failed)` with `failure.data.field:"effort"`, not as a
    pre-receipt `invalid_params` (AD18, AC8). No vendor turn starts. This also
    extends C1 §5's `failure` shape with `data` and C1 §8.2's `submit_failed`
    beyond vendor rejection (AC5, AC9).

## 2. Comparison matrix (C2 item × harness)

"n.p." means not probed.

| C2 item | Claude Code 2.1.285 (`claude-cli`) | Codex 0.159.2 (`codex-app-server`) | OpenCode 1.18.32 (`opencode-serve`) |
|---|---|---|---|
| describe / models | No vendor catalog. VIA ships the packet's bundled versioned catalog (`source: bundled`, PK-CC §2). Aliases resolve at init (CC P3). | Catalog only from `model/list` on a live server; efforts per model; default now `gpt-6.1-sol` (CX §2) | `GET /provider` on an owned server (6.5 MB) with per-model `variants`; the remote catalog changed with no binary change (OC §1–2) |
| version | Init `claude_code_version`; auto-updated 2.1.283 → 2.1.285 (CC §1, D1) | `initialize.userAgent` carried `0.159.2` (probe, `out/c1`; the parse rule is to be qualified); four releases in four days; v2 schema sha changed: additions **and** removal of plugin definitions and fields (CX §1, §3) | Health reports the version; binary and `/doc` identical to the pin with autoupdate off (OC §1) |
| open_session | Logical; `--session-id UUID` at the first launch (CC §2) | `thread/start` echoes `never`, `approvalsReviewer:"user"`, sandbox and cwd; `reasoningEffort` echoes the model default (CX §2 c6) | `POST /session?directory=` with the 4-rule array; the readback is identical (OC §2) |
| identity | Init and result `session_id` match in 18 runs (CC §2) | `thread.id`, then `thread/started`; `sessionId == id` (CX §2) | Returned synchronously; present on every event (OC §2) |
| resume.mismatch | n.p.: no mismatch occurred; a missing session is a pre-init error (CC §2 c0_invalid_resume) | n.p.: resume returned the exact ID; a bogus ID gives -32600 (CX §2 c5) | n.p.: the readback matched (OC §2 t17) |
| StartTurn / acceptance | One `user` line; init → assistant → result; no `msg_lifecycle` receipt (CC §2) | Response `{turn:{id,status:"inProgress"}}` in the same read as `turn/started` (CX §2 c1) | 204 in 7–19 ms; caller `messageID` = user message; assistants carry it as `parentID` (OC §2) |
| definite rejection | Pre-init result `error_during_execution`, `num_turns:0`, exit 1 (CC §2) | Every refusal is -32600 with free text, even an unknown method (CX §2 c7) | 404 `NotFoundError`, 400 `BadRequest` or malformed `messageID`; no message created (OC §2) |
| uncertain submission | n.p. | n.p. | n.p. The crash case (OC §2 t16) is an accepted turn left **unresolved**: the tool part `running` forever and status idle. It is not an induced submission boundary. |
| Steer | Merged into one result (CC §2 c3_queue) → unsupported (K7) | `turn/steer` → `{turnId}`; stale or idle → -32600 (CX §2 c2) | A v1 busy prompt merges; v2 `delivery:"steer"` is a separate loop (OC §2 t06, t07) → unsupported |
| Interrupt | Nested receipt 216 ms after `tool_use`, then `aborted_tools`, exit 1 (CC §2 c7) | `{}` and `turn/completed interrupted` in one 2 ms read (CX §2 c3, c7) | `/abort` 200 `true` in 15 ms, also for an unknown session; `MessageAbortedError`; v2 interrupt does not stop v1 (OC §2 t05, t08) |
| cleanup (process group vs tools) | Bash is its own session leader and `sleep` its own group leader; found only by ppid walk (CC §4.5 c7) | `sleep` alive at 65 s with no `item/completed`; code-mode host and bwrap in their own groups; stdin close kills all within 5 s (CX §1, §2 c0, c3) | Abort killed the child; a SIGKILL of the server group left the tool alive, reparented (OC §2 t16) |
| stop and crash (LH) | Interrupt or SIGTERM kills its parent tree, not double forks; EOF stops nothing; SIGKILL leaves everything | Sandboxed: any sandbox death kills all; interrupt alone leaves background terminals, clean stops them; full access: SIGKILL leaves grandchildren | `/abort` and dispose kill the tool's group only; SIGTERM, SIGKILL leave everything |
| cleanup observation | C2 `tool.quiescent` is replaced by the end result's cleanup (AD4). Tool evidence: `tool_result` per `tool_use` | `item/completed` per `commandExecution`; fast denials have no item (CX §4.1) | Tool part status; an aborted part reports `completed` (OC §3.12) |
| Close | EOF after the terminal → exit 0; early EOF with no active tool completed the turn (CC §2 c10); EOF during an active tool did not stop it (LH E2) | `thread/unsubscribe` → `unsubscribed`; other threads unaffected (CX §2 c4) | No close verb; `DELETE` unused (OC §2) |
| session.vendor_closed | Per-turn process: exit is not a session close | `thread/closed` exists in the schema; n.p. | n.p. |
| session and late attribution | Per-turn process; no late traffic possible after exit | Per-thread events separated on a shared server (CX §2 c4); c4's interrupted item never completed, so no late event was observed | Abort: idle arrives before the final part update, which is still within the turn (OC §2 t05) |
| recover / resume | `--resume UUID` keeps the UUID and history (CC §2 c1b, c9b); recover n.p. | `thread/resume {excludeTurns:true}` returns the exact ID; turn overrides persist as thread defaults (CX §2 c5) | Continuity across a server restart on the same DB (OC §2 t17) |
| progress | Assistant blocks, `tool_result`, `thinking_tokens`, `task_*`, `rate_limit_event` (CC §2) | Items and deltas; `processId` opaque (CX §2) | `message.part.updated` and `delta` (OC §2) |
| final text | `result.result`; `structured_output` with `stop_reason:"tool_use"` (CC §2) | `agentMessage.phase` is commentary or `final_answer` (CX §4.3) | Text of the terminal assistant; `info.structured` (OC §2) |
| vendor terminal | `result`: `subtype`, `is_error`, `terminal_reason`, `api_error_status` (CC P4) | `turn/completed` plus `turn.error` (CX §2) | Idle plus reconciliation; shapes in VO1 (OC §2, §3.1) |
| usage | `result.usage` per turn is authoritative; assistant snapshots are partial (c9a output 6 vs 177; c1a 3 vs 156); cumulative `total_cost_usd`; `fallback_credit` (CC §2) | One `tokenUsage/updated` per model request; sum of `last` = Δ`total` (CX §2 c1) | One assistant per call; sum; children excluded; cost 0 (OC §2 t03, t23) |
| denials | `none` recipe: `permission_denied` plus `permission_denials`. stdio recipe: no event; the list entry is caused by VIA's decline (CC §2 c4, c11b) | Slow denial: a failed item; fast denial: no item (CX §4.1) | `permission.asked` → reject → tool error (OC §2 t13) |
| declines | `can_use_tool` only with `--permission-prompt-tool stdio`; the error reply was accepted (CC §2 c11b) | Zero requests seen; the bodies validate against 0.159.2 (CX §2) | Reject → 200 `true` in 2 ms; `question: deny` removes the tool (OC §2 t12, t13) |
| warnings | stderr only (CC P2) | `warning` and `configWarning` (CX §2 c7, c8) | None; `session.status retry` (OC §2) |
| errors / classes | Synthetic `authentication_failed` or `model_not_found` (404), `success` + `is_error`; `error_max_turns` (CC §2, P4) | `httpConnectionFailed{401}` after ~15 s of retries; 400 `other`; new `tooManyDenials`, `flexUnavailable` (CX §2, §3) | `APIError 401`; uncorrelated `UnknownError` for a bad model; 429 with 5 silent retries (OC §2 t14, t18, t19) |
| config inheritance | §5.4.1 | §5.4.1 | §5.4.1 |
| cold start | n.p. | First `initialize` took 38.1 s with a fresh SQLite home (single observation) | Healthy in 0.76 s |

## 3. The common interface

### 3.1 What C2 keeps unchanged

These stay as written:
- rules 1 and 3–7 (rule 1 gains AD1's sentence; rule 2 is amended by AD12);
- A1's limits (except AD14's wording), A3, A4, A7 and A8;
- delayed identity and the `session.vendor_identity_confirmed` rules;
- **observations before turns**, through the session channel attached at
  `open_session` or `recover`, with C2's tombstones and attribution;
- `recover` returning `Resumed`, `Unknown` or `Dead`;
- deadlines handed down as absolute instants;
- reopen verification;
- the division of responsibility (§3);
- reserved keys (§6.1);
- the bound rows;
- Codex ingress lanes;
- conformance items 1–3, 7–8, 12–13 and 16–17;
- the `[s1c.r2]` rules.

### 3.2 Public surface of `via-adapters` (Proposed; names indicative)

```rust
pub struct AdapterConfig { /* opaque: per-harness settings and switches, fake fixture */ }
impl AdapterConfig { pub fn load(env: BootstrapEnv, harnesses: Option<&RawValue>) -> Result<Self, ConfigError>; }
pub const BOOTSTRAP_ENV: &[&str];
pub fn harness_names() -> impl Iterator<Item = &'static str>;

pub struct AdapterSet { /* Route runtime + one Adapter per configured harness + instance cache */ }
impl AdapterSet {
    pub fn new(config: AdapterConfig, runtime: RuntimeConfig, resources: RuntimeResources) -> Result<Self, AdapterError>;
    pub fn plan(&self, req: &DescribeRequest) -> Result<RoutePlan, Refusal>;                 // pure; no I/O
    pub fn check_turn(&self, session: &SessionRef, turn: &TurnParams) -> Result<(), Refusal>; // pure
    pub fn models(&self, harness: Option<&str>) -> Vec<ModelEntry>;                           // bundled + discovered
    pub fn open_session(&self, session: &SessionRef, spec: SessionSpec, cx: SessionCx) -> SessionDriver; // logical
    pub async fn recover(&self, session: &SessionRef, facts: &[AnchorRecovery], cx: SessionCx) -> Recovery;
    // S1 Host-fact operations, renamed only: recover_page, recover_cohort_page, hold_capacity,
    // reprobe_held, held_unproven, live_armed, pending_cleanup, watch_force,
    // shutdown -> AdapterShutdown (FakeShutdown), AnchorRecovery (FakeRecovery, FakeTurnRecovery).
}
pub struct SessionCx { pub observations: ObservationSink /* C2 A1: 1024 items, 4 MiB, per session */,
                       pub tracker: TaskTracker, pub cancel: CancellationToken }
impl SessionDriver {
    pub fn prepare(&self) -> Prepared;   // pins a live connection, or reports that a new one is needed (AD16)
    pub async fn run_turn(&self, spec: TurnSpec, cx: TurnCx) -> TurnEnd;
    pub async fn steer(&self, input: SteerInput) -> Result<SteerDelivery, SteerError>;
    pub async fn close(&self, mode: CloseMode, deadline: Deadline) -> CloseReport;
    pub fn health(&self) -> watch::Receiver<DriverHealth>;
}
pub enum Prepared { Pinned(ConnectionPin), NeedsConnection }
pub struct TurnCx { pub turn: TurnNumber, pub prepared: Prepared, pub capacity: Option<CapacityToken>,
    pub activity: TurnActivity, pub wall: Deadline, pub tool_grace: Duration /* C1 P7: 60 s */,
    pub stop: StopWatch, pub force: ForceWatch }
pub enum Observation {              // each item carries `at` and `vendor_turn: Option<VendorTurnId>`
    Accepted(Acceptance), IdentityConfirmed(Identity /* id, connection, transcript? */),
    Progress(ProgressMarks), FinalText(String), ActionDenied(Denial), RequestDeclined(Decline),
    SteerDelivered(SteerDelivery), Warning(C1WarningCode, String), VendorClosed(String),
    ResumeMismatch { requested: String, returned: String },
    LateTerminal(VendorTerminal) }  // only for a turn whose end result carried no terminal
pub struct TurnEnd { pub terminal: Option<VendorTerminal>,
    pub instance: Option<InstanceReport> /* set once the handshake was read, on every outcome (AD7) */,
    pub leftovers: Option<LeftoverReport> /* per-turn routes on every outcome, and `ServerLost` (AD20) */,
    pub outcome: Result<TurnEvidence, AdapterError> }
pub struct CloseReport { /* S1's close fields */ pub leftovers: Option<LeftoverReport> /* only when this close stopped the server (AD20) */ }
pub struct LeftoverReport { pub scope: LeftoverScope /* turn | server */, pub processes: Vec<LeftoverProcess> /* ≤ 16, oldest first */,
    pub total: u32, pub incomplete: bool }   // LeftoverProcess { pid, comm, started_at }; semantics in AD20
pub struct InstanceReport { pub vendor_version: Option<String>, pub version_status: VersionStatus /* tested | untested */ }
pub struct TurnEvidence { pub exit: Option<ExitReport>, pub cleanup: Cleanup, pub journal_uncertain: bool }
pub struct VendorTerminal { pub at: Instant, pub status: VendorTerminalStatus, pub stop_reason: StopReason,
    pub vendor_stop_reason: String, pub vendor_code: Option<String>, pub class_hint: Option<ClassHint>,
    pub detail: Option<String>, pub structured_output: Option<Box<RawValue>>, pub steps: Option<u64>,
    pub usage: Option<UsageSample> /* turn aggregate */, pub cost: Option<CostReport>,
    pub vendor: Option<Box<RawValue>> /* bounded 16 KiB */ }
pub struct UsageSample { pub key: Option<String>, pub input: Option<u64>, pub cached_input: Option<u64>,
    pub output: Option<u64>, pub reasoning_output: Option<u64>, pub total: Option<u64> }
pub enum ClassHint { Auth, RateLimit, ContextExceeded, BudgetExceeded, VendorError, Protocol, ResumeMismatch }
pub enum StopReason { EndTurn, MaxSteps, Budget, Refusal, Interrupted, Error, Other }
```

Division of ownership:
- **Core** keeps the C1 request parameters, `Effective`, the envelope, events,
  `FailureClass`, the error mapping, deadlines, queues, connection admission and
  commits.
- **`Capabilities` and `RoutePlan`** serialize to the C1 §4.1 and §3.1 shapes.
- **`AdapterError`** keeps S1's `Route(RouteFailure)` causes (deadline, force
  stop, overflow, protocol, process exit, unknown submission) and gains
  `Rejected(StartRejected)`, `ServerLost` (Host-confirmed death of a persistent
  server) and `TransportLost` (connection lost, server alive or unconfirmed).

### 3.3 C2 amendments

**AD1. Core names no harness (C2 §1 rule 1, §2 enum).**
- Add to rule 1:
  > Core names no harness, route, vendor model or vendor term. It passes the
  > caller's `harness` string to `via-adapters` and stores the returned
  > canonical name as opaque data.
- Replace the enum with `Adapter { Claude, Codex, OpenCode, Fake }`. `Fake` is
  reachable only through runtime §11.1's fixture configuration. ACP joins with
  its adapter.
- Tests: the literal guard's self-test (§5.6); the gate passes on the
  de-faked tree.

**AD2. Adapter surface (C2 §2 types table).**
- `RoutePlan` gains `harness: &'static str` and `model: {requested, resolved}`.
- `Capabilities` is the C1 §4.1 DTO with
  `Support { Native, Partial { semantics }, Unsupported { reason } }`.
- `Refusal.kind` gains `InvalidParam { field }` and `MissingCapability { verb }`,
  and every refusal carries `route`.
- `check_turn` validates a resume turn's values (effort, schema, `max_steps`,
  bound, vendor keys) against the frozen route using bundled or discovered
  metadata only (§5.3).
- `models` returns entries with `source: bundled | discovered`.
- Evidence: `Support` lacks `Partial` (`api.rs:1654`); Claude cancel is partial
  (PK-CC §3).
- Tests: `require cancel:partial` passes and `require cancel` fails against a
  partial fake profile; describe and spawn plans are identical.

**AD3. Turn lane and session context (C2 §2 sketch, "Submission boundary",
"Independent lanes", "Observations before turns", Recover; §7 item 4).**
Replace `StartTurn { spec, reply }`, `StartOutcome` and `ControlCommand` with:

> The data lane is `SessionDriver::run_turn(TurnSpec, TurnCx) -> TurnEnd`, one
> call per submitted turn, made after Core commits `submitted_at`.
> - Acceptance is reported once, as `Observation::Accepted { correlation,
>   vendor_turn_id }`.
> - A definite rejection before acceptance ends with
>   `Err(Rejected(StartRejected))`. An ambiguous submission ends with Route's
>   typed unknown-submission failure. Neither ever resends.
> - Interrupt and close of the running turn are S1 stop orders on `TurnCx.stop`.
>   The daemon force is `TurnCx.force`.
> - `steer` and session-level `close` are driver methods, callable while
>   `run_turn` is pending. `health` stays the sticky lane.
> - `open_session` is logical: it performs no vendor I/O and attaches C2's
>   per-session observation channel (`SessionCx`). Session-level observations
>   (identity, vendor closed, resume mismatch, warnings, late durable items)
>   flow on it at any time. While a turn runs, Core's drive loop drains it
>   (S1's stall and permit rules). Between turns, a session drain commits
>   durable items and drops non-durable ones.
> - Vendor session creation or reopening (Codex `thread/start`/`thread/resume`,
>   OpenCode `POST /session` or readback, Claude `--session-id`/`--resume`)
>   happens in the first `run_turn` of a connection generation. Identity is
>   confirmed by `IdentityConfirmed`, as for Claude today.
>   `VendorIdentity.expected_id` becomes `Option`: only Claude has an expected
>   ID.
> - `recover(session, facts, cx)` keeps C2's `Resumed`/`Unknown`/`Dead`. No R1
>   route declares `recover`, so R1 adapters return `Unknown`, or `Dead` only
>   with Host-confirmed death, and compute cleanup under AD9.

Evidence:
- S1 implements the data lane (`via-adapters/src/runtime.rs:175-260`).
- Claude (a process per turn), Codex (a thread on a shared server) and OpenCode
  (a server per session) each map onto one pending call per turn (§2).
- A logical open lets connection admission stay at dispatch (AD16).

Tests:
- C2 §7 items 5, 10 and 17 on the fake.
- A session-level `VendorClosed` between turns commits with `turn: null`.
- `recover` with Host death facts gives `Dead`.

**AD4. Terminal retention, return conditions and deadlines (C2 §2 Interrupt,
§4 table, §7 items 6 and 10).** Delete the `tool.quiescent` row. Replace the
`turn.vendor_terminal` row and item 6 with:

> **One result per turn.** `run_turn` returns one `TurnEnd`.
> - `terminal` is the decoded vendor terminal, retained there even when its
>   earlier observations could not be delivered. `outcome` carries the process
>   and cleanup facts, or a typed failure (rejection, unknown submission,
>   transport loss, process exit without a terminal, server loss, overflow,
>   deadline, force).
> - At most one vendor terminal exists per turn. A result can carry a terminal
>   together with a failure, which Core disposes under C1 §7.6 and `[s1c.r2]`
>   exactly as in S1.
>
> **When `run_turn` returns.**
> - **Private per-turn process routes** (Claude, fake): after the process exits
>   and Host has reported group cleanup, bounded by the S1 rules (wall
>   deadline, `force_at`, `close_by`, 3 s allowance). A tool that ended never
>   shortens this.
> - **Persistent-server routes** (Codex, OpenCode) after a `Completed` or
>   `Failed` terminal: at once. Cleanup does not apply without a cancel
>   (C1 §7.3).
> - **Persistent-server routes** after an `Interrupted` terminal: when every
>   reported tool item has ended, or at `min(terminal.at + TurnCx.tool_grace,
>   wall)` (C1 P7), whichever comes first. At that bound, cleanup of unresolved
>   tools is `Uncertain` (AD9).
> - **Persistent-server routes** with no terminal, per C1 §7.6:
>   - Host confirms the server died → `Err(ServerLost)` → `failed(server_lost)`;
>   - the connection is lost while the server is alive or its state is
>     unconfirmed → `Err(TransportLost)` → `unknown`;
>   - an ambiguous submission → the typed unknown-submission failure →
>     `unknown`;
>   - the stop order's `force_at` passes without acknowledgement → the S1
>     force-stop failure → `unknown` (C1 "Force deadline, shared server"); a
>     shared server is never killed.
>
> **Wall expiry reuses S1's deadline path.** Unchanged from S1:
> 1. At the wall instant Route's serve loop fails the turn with the typed
>    `Deadline` failure (`via-routes/src/runtime.rs:604-608`).
> 2. Route runs one cleanup step under S1's cleanup bound, 3 s from that
>    failure (`cleanup_deadline()`, `runtime.rs:163-190, 801-803`), then
>    returns the failure with its `cleanup`, `forced` and `exit` facts.
> 3. After the adapter returns, Core commits `cancel.requested` with the wall
>    instant as `requested_at` (`engine/drive.rs:711-730`), and `dispose` gives
>    `failed(deadline_wall)` with `cancel` filled from those facts
>    (`engine/terminal.rs:153-166`).
> 4. An existing stop order keeps precedence: its `force_at` is capped at the
>    wall (`engine/queue.rs:166`), so either its row already applied, or it
>    equals the wall and `dispose` gives the order's row (`by_order`,
>    `terminal.rs`, [r1.9]).
> 5. A terminal decoded before the wall keeps S1's late path
>    (`runtime.rs:197-215`): the natural terminal wins.
>
> **What is per route: only step 2's cleanup step** (recipes in AD19):
> - Private per-turn routes (Claude, fake): Host force close through Wire
>   `close` (`via-wire/src/connection.rs:340`) and Host Stop
>   (`via-host/src/host.rs:1943`), exactly as in S1. This is a request: the
>   outcome is `forced` and cleanup `quiescent` only with Host's `GroupAbsent`
>   evidence; otherwise `requested`/`uncertain` (C1 §7.6 private-process row).
> - Codex: `turn/interrupt`; the shared server is never closed.
> - OpenCode: `POST /session/{id}/abort`, with VO1 step (0) as the
>   acknowledgement.
>
> **One wall cutoff:** S1's cleanup bound (3 s from the wall failure). Vendor
> acknowledgement or cleanup evidence after it is a late observation only.
> The stop order's `force_at` stays the cutoff for orders; the wall creates no
> order, so the two never meet.
>
> **Core handoff:** the returned failure carries S1's `cleanup`, `forced`,
> `exit` and `launched`, plus two facts: `acknowledged` (the vendor
> acknowledged the stop within the cutoff) and `shared` (the connection is a
> persistent server, from the route plan). Core's two changes, both generic:
> - `stop_outcome(quiescent, forced)` (`engine/stop.rs:676`) also takes
>   `acknowledged` and gives `forced`, else `acknowledged`, else `requested`;
> - in `stopped` (`engine/terminal.rs:243`), the launched, unforced branch
>   that today gives `unknown` with outcome `requested` (`terminal.rs:320`)
>   gives outcome `unknown` when `shared` (C1 §7.6 "Force deadline, shared
>   server"); private routes keep `requested`.
> Without an earlier order the result is always `failed(deadline_wall)` with
> `cancel` filled (C1 §7.6 "Core deadline"); an unproven stop shows as
> `cancel.outcome: requested` and `cancel.cleanup: uncertain`, as S1 already
> does for a private route. The wall has passed, so P7 settles cleanup at once
> (AD9). With an earlier order, the order's row applies (C1 §7.6 force rows,
> including `unknown` on a shared server).
>
> **Two deadlines.** The stop order's `force_at` and `close_by` bound the wait
> for acknowledgement. They do not apply after acknowledgement on server
> routes; the P7 window does. An adapter keeps the acknowledgement instant
> separately from any cleanup reconciliation (VO1).
>
> **Late observations.** Observations of a turn after its `TurnEnd` keep their
> vendor turn attribution (tombstones). Durable ones (denials, declines) are
> committed `late: true`. A `LateTerminal` for a turn that ended without a
> terminal revises `unknown` under C1 §7.6. Non-durable ones are dropped.

Evidence:
- `[s1c.r2]` and the adapter's retained Route result
  (`runtime.rs:~200-258`).
- S1 `close_by = requested_at + force_after + 3 s` (`engine/queue.rs:165-170`),
  which is about 13 s and cannot express `ack + 60 s`.
- Codex's tool was alive past acknowledgement (CX §2 c3).

Tests:
- A terminal whose observations are stalled at wall expiry is still disposed
  from the retained terminal.
- Saturated queue, latch and force cases, with the S1 scenario suite as
  characterization.
- A reported tool ending 20 s after acknowledgement (past 13 s, before 60 s)
  settles `quiescent`.
- No acknowledgement by `force_at` on a server route gives `unknown` with no
  kill.
- Host-confirmed server death with no terminal gives `failed(server_lost)`; a
  lost connection to a live server gives `unknown`.
- Wall, no earlier order, persistent fake profile: the profile's interrupt is
  sent before `run_turn` returns; acknowledged within the cutoff →
  `failed(deadline_wall)`, `cancel {outcome: acknowledged, cleanup per AD9}`;
  not acknowledged → `failed(deadline_wall)`, `cancel {outcome: requested,
  cleanup: uncertain}`; an acknowledgement after the cutoff changes nothing.
- Wall with an earlier cancel whose `force_at` passed first: the order's row,
  unchanged by the wall.
- Wall with an earlier cancel capped at the wall (`force_at == wall`): the
  order's row (`by_order`), `cancel.cause` kept; on the persistent profile,
  no acknowledgement → state `unknown` and `cancel.outcome: unknown`; on the
  per-turn fake, S1's `unknown` with `cancel.outcome: requested` is unchanged.
- Per-turn fake: every S1 wall assertion unchanged.
- A terminal decoded before the wall wins (S1 late path).
- Codex fixture: wall expiry writes `turn/interrupt`.
- OpenCode fixture: wall expiry sends `POST /abort` for the session.
- OpenCode fixtures, cancel racing a natural end: the assistant completes with
  `finish:"stop"` and no abort error → `completed`, `cancel.outcome:
  requested`; the assistant ends with an `APIError` and no abort error →
  `failed` with its class, `cancel.outcome: requested`.
- OpenCode fixture: abort error and idle arrive, then the final assistant and
  tool updates are delayed past `force_at`; the turn is `Interrupted` at the
  acknowledgement instant (not `unknown`), and cleanup follows P7.
- A late denial after settlement commits `late: true`.

**AD5. Acceptance and pre-model vendor failures (C2 §2 "Submission boundary",
§7 item 5).** Add:

> A vendor-synthetic API-error message (Claude `is_api_error_message:true`,
> `model:"<synthetic>"`) is never acceptance, progress or final text. On Claude,
> a post-init result for the prompt line is acceptance evidence. A vendor
> failure after acceptance and before model output is a `Failed` terminal with
> vendor code, class hint and `detail`. `submit_failed` is only for a definite
> rejection before acceptance.

Evidence: CC §2 (auth and bad model after init); CX §2 c7, c8; OC §2 t14, t18.

Tests:
- Claude c0_isolated → `failed(auth)`.
- Claude c0_bad_model → `failed(vendor_error)`, `vendor_code:"model_not_found"`.
- Both with `accepted_at` set and no progress `model` mark.
- The same outcome for Codex c8/c7 and OpenCode t18/t14.

**AD6. Usage, cost and vendor data (C2 §4 `progress` and `vendor_terminal`,
§5, §6.2 usage row).**

> **Two usage sources.** Usage is reported one of two ways:
> - per model call, as `progress.usage: UsageSample`, where a keyed sample
>   supersedes an earlier sample with the same key and a keyless sample adds;
> - as a turn aggregate in `VendorTerminal.usage`, which supersedes every call
>   sample of the turn for the envelope.
>
> Each route declares which it uses.
>
> **Turn-wide ledger.** Core keeps a turn-wide usage ledger separate from step
> accounting.
> - It holds up to 1,024 keys per turn. Further new keys add as keyless.
> - Once it overflows, the envelope reports scope `vendor_interval` with
>   `usage_interval_unverified`.
> - A component is `null` if any contributing sample lacks it.
>
> Step rows keep their existing per-step rule for `status`.
>
> **Cost and vendor data.** `VendorTerminal.cost` gives `{usd, scope}`.
> `VendorTerminal.vendor` is bounded vendor data for the envelope's `vendor`
> member. `IdentityConfirmed.transcript` fills `evidence.transcript`.

Evidence:
- Codex: keyless per-request samples summed equal Δ`total` (CX §2 c1).
- OpenCode: keyed assistant snapshots repeat (OC §2).
- Claude snapshots are partial: c9a output 6 vs 177, c1a 3 vs 156, so Claude
  uses the `result.usage` aggregate.
- `engine/progress.rs:20-21, 186-258` clears keys at step boundaries and caps 16
  per step.
- `api.rs:1975` carries only the total.

Tests:
- Key `a` repeated after a step boundary is counted once.
- Key 1,025 overflows as stated.
- A missing `cached_input` gives `null`.
- A Claude turn aggregate supersedes samples.
- Envelope `cost` and `vendor` are populated from the terminal.

**AD7. Version rule (C2 A2, §5, §7 item 14; OD1 decided).** Replace with:

> Every vendor version is supported by default.
> - Each adapter compiles in a `checked` set: versions its maintainers' cheap
>   live check passed.
> - The instance that runs a turn reports its version from its own handshake
>   (Claude init, Codex `initialize`, OpenCode health). `TurnEnd.instance`
>   carries it on every outcome, success or failure, once the handshake has
>   been read. The envelope reports it with `version_status` `tested` (checked)
>   or `untested` (not yet checked, warning `vendor_version_untested`). A turn
>   that failed before any handshake reports `vendor_version: null`; Core never
>   substitutes a cached version from another instance.
> - `refused` only when a startup or handshake check fails on something VIA
>   relies on.
>   - Before submission (Codex, OpenCode), the turn fails `submit_failed` with
>     `failure.data.reason:"handshake_refused"` (AC9); `vendor_code` stays for
>     vendor codes only.
>   - After Claude's prompt line, it fails `protocol` with no resend.
> - **Refusal cache.** Only a demonstrated incompatibility is cached: a
>   relied-on feature absent from the handshake, or a readback that differs from
>   the value VIA sent. The key is the binary identity (device, inode, size,
>   mtime of the resolved target) plus the route's recipe digest (launch
>   arguments, category switches, bound and policy inputs). While an entry is
>   live, plans with the same key refuse `harness_unavailable`
>   (`data.reason:"handshake_refused"`). Spawn failures, timeouts, transport
>   loss, auth, quota and rate-limit failures are never cached. An entry
>   expires 10 minutes after it was written, and at daemon restart. The next
>   turn after expiry launches and re-checks, so a fixed environment recovers
>   without a binary change.
> - `describe` and receipts report the last version seen for that binary
>   identity, or `null`/`untested`, and start nothing.
> - Capabilities belong to the adapter version, not the vendor version. A
>   handshake check that exposes a relied-on feature (Claude
>   `interrupt_receipt_v1`, permission-mode echo and tool list; Codex policy and
>   sandbox echo; OpenCode permission readback) refuses the instance when that
>   feature is missing.
> - **A handshake cannot prove:** never-ask behaviour beyond the echoed setting;
>   effective effort (Claude ignores unknown effort, CC P2; OpenCode accepts any
>   variant, OC §2); hidden execution surfaces (Codex code-mode `exec` with no
>   item, CX §4.1; hooks and plugins); complete cleanup; bound enforcement
>   semantics, including Codex read-only once `via-5lr.3.4` enables it; unchanged
>   usage or terminal semantics.
> - Proceeding on unchecked versions is the owner's accepted risk. The warning
>   stays visible in every receipt, status and envelope of such a turn.
>   `allow_untested` is accepted and stored for C1 compatibility but has no
>   effect (AC1).
> - A persistent server keeps the version it reported at its own handshake, even
>   after the executable changes on disk. A new server key follows only for new
>   connections.

Evidence: CC §1 D1; CX §1, §3.

Tests:
- A version outside the `checked` set produces a warning, not a refusal.
- A failed permission-mode echo is refused and cached for that recipe digest
  only; another recipe on the same binary still launches.
- A binary identity change, or expiry, clears a cached refusal.
- A startup timeout is not cached.
- An observed version followed by a protocol failure, overflow, transport loss
  or force stop still reports that version in the envelope.
- A server started before an update reports its own version.
- The fake's policy is in §5.5.

**AD8. Denials (C2 §7 item 9, §6.2 auto-decline row).** Replace item 9 with:

> Denials the vendor reports in structured form produce `action.denied`. No
> route promises complete denial reporting. An action denied because VIA
> declined the vendor's request produces only `vendor.request_declined`. The
> adapter correlates the decline with the tool call (Claude `tool_use_id`,
> OpenCode `callID`) and suppresses the matching denial-list entry.

Claude's `none` recipe derives denials from `permission_denials`, deduplicated
against a live `permission_denied` by `tool_use_id`. Codex reports none.

Tests:
- c4: one denial.
- c11b: one decline and zero denials.
- OpenCode t13: one decline and zero denials.

**AD9. Cleanup (C2 §2 Interrupt and Recover; §7 items 10, 11, 15; OD3
decided).** Cleanup keeps its approved meaning: the agent's own process group,
or the vendor's reported tool items on a server route.

| Case | `Quiescent` when (otherwise `Uncertain`) |
|---|---|
| Private per-turn route (Claude, fake) | `GroupAbsent` for the agent's own group (runtime §5.2; S1) |
| Server route (Codex, OpenCode), cancel while the server lives | every reported tool item of the turn ended, within P7's window (C1 §3.5, P7) |
| Server close or crash | `GroupAbsent` for the server's own group |
| Recovery after a daemon restart | `GroupAbsent` (C1 §7.5) |
| No launch (a force accepted before any vendor launch) | Host's journal is complete (C1 §7.4) |

`Pending` exists only while a wait is running and its deadline (the P7 window,
or the S1 process bound) has not passed; a settled result never carries
`Pending` (C1 §3.5). OS group-absence evidence covers only the agent's own
group: descendants outside it that no vendor item tracks are not part of
cleanup; they are the agent's responsibility, and VIA reports them (AD20). On
server routes a reported tool item still counts until it ends or P7's bound
passes, wherever its process runs. C1 P7, §7.3, §7.4's no-launch rule and §7.6
are unchanged.

Tests: the S1 cleanup suite unchanged; an open tool at the P7 bound →
`uncertain`, and no settled result carries `pending`.

**AD19. Stop recipes (C2 §6.2 Interrupt and Close rows; OD3 decided).** A soft
stop is the vendor's own message, and the agent decides; the hard stop is S1's
own-group stop through the anchor, unchanged. VIA sends nothing else.

| Route | Soft stop (cancel; wall, AD4) | Close |
|---|---|---|
| Claude (per turn) | `control_request interrupt` (it stops what is still in its parent tree, LH E1), then stdin EOF; EOF alone does not stop an active tool (LH E2). Then S1's close: graceful, then the hard stop at the stop order's bound | per turn: EOF after the result, then S1's close |
| Codex (shared server) | `turn/interrupt`; background terminals keep running (LH E1) and remain the server's | session: `thread/unsubscribe`. Server (idle retirement, AD16): stdin close, which stopped every tool under the sandbox (LH E2c; untested under full access, where grandchildren survived SIGKILL, LH E4), then S1's hard stop |
| OpenCode (server per session) | `POST /session/{id}/abort` (the tool's own group only, LH E1) | `POST /instance/dispose` (LH E2i), then S1's hard stop (the server has no SIGTERM handler, LH E3) |
| Fake | S1 | S1 |

Tests: fixture wire order per cell; the hard stop is S1's, unchanged.

**AD20. Leftover report (C2 §2, §4 new result fact; OD3 decided; detection
option A, conflict 4).** Processes the agent started that are observed after
its own process exited are reported to the caller as left over by the coding
agent. VIA never signals or manages them.

| Aspect | Rule |
|---|---|
| Destinations | Only where an existing surface ends the connection synchronously. (1) Per-turn routes (Claude, fake): the turn envelope; the scan completes before the terminal commits. (2) A C1 `close` that stops the server (OpenCode's dispose): that close result and `session.closed`, persisted atomically with the close by Store `commit_closed` (in the event, `close_result` and the operation result), so a keyed replay returns the same report. A Codex C1 close only unsubscribes: `null`. (3) Server lost with turns in flight: one scan, completed before the loss reaches any turn; the same snapshot (`scope: server`; on Codex it may list other sessions' processes) goes on every `server_lost` turn. One report per connection generation; a turn spans at most one (AD16 item 4). |
| Not reported (limitation) | Idle retirement (Codex's normal server end; OpenCode's idle policy); a server crash with no turn in flight; daemon shutdown; daemon-crash recovery (recovered turns carry `leftovers: null`). Codex's normal case has nothing to report: a sandboxed stdin close left no tools (LH E2c). Where no destination exists, nothing is scanned or logged (via-jm4.24). |
| Carried by | Host `CloseReport.leftovers` (`via-host/src/host.rs:749`) → Wire `WireCloseReport` (`via-wire/src/connection.rs:341`) → Route result (`via-routes/src/runtime.rs:872` and each server route's close and loss paths) → Adapter `TurnEnd.leftovers` or driver `CloseReport.leftovers` (§3.2) → Core envelope, close result and `session.closed` (Store `commit_closed`, `via-store/src/runtime/sql.rs:1625`). Recovery (`via-adapters/src/runtime.rs:605`) carries none. |
| Trigger | After Host's close of the connection completes, within its existing bound (unchanged); the report is ready before its destination commits (Destinations). |
| Scan bound (option A) | Absolute deadline `min(close_by, scan_started + 1 s)`; with no budget left the report is `incomplete` and nothing is read. One scanner task per report, cancelled at the deadline. |
| How (option A) | Host already sets a random `VIA_PROCESS_MARKER` in every vendor environment (`via-host/src/host.rs:2029-2032`); children inherit it. It stays in memory only. **Start bound:** the anchor reads the vendor's start ticks from `/proc/<vendor>/stat` right after spawn, while it still holds the unreaped child (before `Spawned` is sent, `via-host/src/anchor.rs:321`, and before its wait at `:261`), and carries them in `Spawned {pid, start_ticks}` (`protocol.rs:108`, `host.rs:1443`); Host keeps them in memory only. This is non-environment procfs metadata reported with the anchor's vendor child facts (runtime §5.1 step 3, line 454); step 4's durable commit of vendor facts (line 458) is unchanged because the start ticks stay in memory (AR2). If the bound is unavailable, the report is `incomplete` and no candidate environment is opened. **Per `/proc` entry:** open the `/proc/<pid>` directory, then `openat` `stat`, `status`, `environ` and `comm` through that descriptor, so every read is bound to one process instance. Keep an entry only if its real uid (`status`) equals the daemon's real uid, its start ticks are at or after the bound, the environment holds the exact marker entry and fits the cap, and a final re-read through the same descriptor shows the same start ticks and a state other than zombie or dead (`stat`) and the same real uid (`status`). The environment is read as a stream of at most 256 KiB plus one lookahead byte: reaching the lookahead byte means over the cap, and a match counts only once end of file is reached within the cap. It is compared in memory and dropped. Tick granularity only widens which environments are read; attribution is still the exact marker match. Entries mean "observed during the scan", not "alive". |
| `incomplete` (option A) | Set when `/proc` enumeration fails, the start bound is unavailable, a `stat` or `status` read fails before eligibility is settled, any required read of an eligible entry (`environ`, `comm`, or the final `stat` or `status` re-read) is denied or fails, `/proc` is mounted with `hidepid=4` or its spelling `hidepid=ptraceable` (which hides same-uid processes the daemon cannot trace), an environment is over the cap (counted as no match), or the deadline passes. A process that disappears between reads (`ENOENT`, `ESRCH` or an empty read) is dropped, not a failure. |
| Shape (AC10) | `leftovers: {scope: "turn" \| "server", processes: [{pid, comm, started_at}], total, incomplete, best_effort: true} \| null`. `processes`: at most 16, oldest first (start ticks, then pid). `total`: the matches found; exact when not `incomplete`, a lower bound otherwise; `total > processes.len()` is the only truncation signal, distinct from `incomplete`. `started_at`: RFC 3339 UTC, boot time (`/proc/stat` `btime`, whole seconds) plus start ticks, so accurate to about 1 s and emitted with second precision. `comm`: the kernel's name (at most 15 bytes), lossy UTF-8. `null` when no scan ran. |
| Privacy | Nothing from the scan is persisted except the report. The guarantee covers the fields VIA reads and emits: no environment byte, marker value or command line enters the report, errors (pid and errno only), logs or `Debug` output (coding-style §8). `comm` is process-controlled (a process can name itself anything); AC10 documents that. |
| Limits (option A) | Best effort: missed are processes whose procfs-visible environment lacks the marker, processes that changed uid, processes outside the daemon's pid namespace, and work handed to outside services (tmux server, systemd, docker, ssh, cron, WSL `.exe`). Each `x.3.4` qualifies that the harness's tool processes carry the marker (option A). |
| Future (not built) | A kill-or-keep option; Codex `thread/backgroundTerminals/clean` (experimental, LH E1t) is its Codex mechanism. |

Tests use controlled survivor fixtures (a fake vendor that starts known
processes). **Failure-first, delivery** (red today: no report or field
exists): (1) a setsid and a double-forked survivor are listed oldest first and
still alive afterwards; (2) 17 survivors → 16 entries, `total: 17`,
`incomplete: false`; (3) two turns failed by one lost persistent fake server
carry the identical snapshot, and neither commits before the scan ends, also
with observation delivery stalled; (4) a server-stopping close returns the
report, `session.closed` holds it, and a keyed replay returns it unchanged;
(5) idle retirement, a Codex-profile close (unsubscribe only) and
daemon-crash recovery give `leftovers: null`. **Failure-first, option A**
(each asserts a non-null report, which is red today; (6), (7) and (9)–(12)
also list a marked control survivor, while the no-read cases of (8) and (13)
expect `incomplete` with no processes): (6) pid reuse between the `stat` and `environ` reads is not
listed, and an exit between any two reads drops the entry without
`incomplete`; (7) a non-dumpable same-uid survivor (denied `environ`, no
privileges needed) and an injected `stat` read failure each give
`incomplete`; (8) zero budget → `incomplete` with no `/proc` open; a slow
scan → `incomplete` at 1 s; (9) an environment of exactly 256 KiB holding the
marker is listed, and one of 256 KiB plus one byte is not and sets
`incomplete`; (10) a process started before the vendor and planted with the
marker has no `environ` opened; (11) a survivor exec'd with an environment
that omits the marker, verified by the test, is not listed; (12) synthetic
sentinels in the vendor environment never reach the report, a scan error, the
log or `Debug` output; (13) a vendor that exits at once after starting a
survivor still yields the anchor's bound and the survivor is listed; with the
bound unavailable (seam), the report is `incomplete` and no `environ` is
opened. **Characterization** (green today): S1's own-group stop and anchor
suites; nothing outside the group is signalled.

**AD10. Steer (C2 A3, §6.2 Steer row).** Replace the OpenCode cell with:

> `unsupported`: a v1 busy prompt merges into the running turn, and v2
> `delivery:"steer"` runs a separate conversation (A3's reasoning).

Evidence: OC §2 t06, t07. Test: `require steer` → `missing_capability` naming
`opencode-serve`.

**AD11. Class hints (C2 §6.2 row).** Add to every column:

> An HTTP 401 or 403 in a vendor error → `auth`.

| Route | Mappings |
|---|---|
| Codex | `httpConnectionFailed{401\|403}` → `auth`; `tooManyDenials`, `flexUnavailable` → `vendor_error` |
| OpenCode | `APIError 401\|403` → `auth`; `APIError 429` → `rate_limit`; `UnknownError` / `ProviderModelNotFoundError` → `vendor_error`; `MessageAbortedError` after VIA abort → `Interrupted` |
| Claude | Classify on `is_error`, `terminal_reason`, `api_error_status` and the synthetic `error` code, never on `subtype`. `authentication_failed` or 401/403 → `auth`; `error_max_turns` → `budget_exceeded` with `StopReason::MaxSteps` |

Core maps `class_hint` to `FailureClass` and keeps the adapter's `StopReason`
for failed terminals.

Tests: CC c0_isolated, CX c8, OC t18, t19, and the Claude max-turns fixture
(`failed`, `stop_reason:"max_steps"`).

**AD12. Adapter version (C2 §1 rule 2, RoutePlan; OD5c decided).**

> - `adapter_version` is a per-adapter constant, changed when stored session
>   state or the vendor recipe changes.
> - An adapter lists the stored versions it is compatible with, normally its
>   previous version. Resume or reopen of an incompatible version is refused
>   `harness_unavailable` (`data.reason:"adapter_version"`).
> - On a compatible resume, the session's persisted `adapter_version` advances
>   to the running adapter's version at that turn's `turn.started` commit. Each
>   envelope reports the adapter version that ran the turn. Later adapters
>   therefore check compatibility against the latest version.
> - C2 field additions are additive.

Replacement wording for `.repo-context/invariants.md` rule 2 (owner-approved
change; the coordinator applies it in S-SPEC):

> 2. **One route per session.** The route chosen at spawn serves every later
>    turn and verb in that session. A session's stored state is used only by an
>    adapter version that declares that state compatible: resume or reopen under
>    an incompatible adapter version is refused, and a compatible resume
>    advances the session's recorded adapter version. Each turn records the
>    adapter version that ran it. Source: `docs/brainstorms/README.md` §15
>    (D5); owner decision OD5c, 2026-09-30
>    (`docs/workstreams/rust-foundation/adapters/design.md` AD12).

Evidence: `api.rs:1840`.

Tests:
- An incompatible version is refused.
- A compatible chain v1 → v2 → v3 resumes, each version declaring only its
  predecessor.
- Status shows the advanced version.

Conflict 2 (owner-approved).

**AD13. Inherited configuration (C2 §6.2, new row; OD2 decided).**

> Each route declares, per category (hooks, MCP servers, plugins, skills,
> agents, instruction files):
> - whether its vendor loads that category by default;
> - for each direction (on and off), whether VIA can apply it, and whether that
>   is verified, unverified or unavailable;
> - what inventory of the inherited set it can record.
>
> Settings come from `AdapterConfig` (§5.4) and are frozen per session at
> spawn. For each category the route records the **effective state**: `on` or
> `off` only when verified (a verified switch, the private profile, or an
> inventory that lists or omits the category), else `unknown`. Whenever the
> effective state is not the verified requested state, for any reason (the
> request cannot be applied, the switch is unverified, or no switch is applied
> and the vendor default is unverified), the spawn receipt and every turn
> envelope carry one warning `config_switch_unverified` with
> `data.categories: [{category, requested, effective}]`. The
> effective states are part of the frozen session parameters (visible in
> status). VIA never claims a suppression or an inheritance it has not
> verified. Switches in effect enter the route's launch recipe and server key.

Evidence: §5.4.1.

Tests:
- Fixture argv per setting.
- An off setting on a not-switchable category warns, effective `on` or
  `unknown`.
- An on setting that cannot be applied (OpenCode plugins) warns, effective
  `off`.
- An on setting with no switch applied and an unverified vendor default
  (Codex skills) warns, effective `unknown`.
- A verified on setting (Claude skills listed in init) does not warn.
- Live qualification of each switch in the `x.3.4` beads.

**AD14. Raw-log wording (C2 A1, §4).**
- Delete "else drop-and-count with `raw_log_incomplete`".
- Replace "still read and raw-logged … actually lost" with "still read and
  counted; normal observations stop".

Text only.

**AD15. §6.2 mapping rows.**
- **Codex Observations:** final text comes only from `agentMessage` items with
  `phase:"final_answer"`.
- **OpenCode Observations:** VO1.
- **OpenCode Interrupt:** "`/abort` 200 is command acknowledgement only.
  `Acknowledged` once both the session's `MessageAbortedError` and idle have
  been seen, independent of assistant completion; the adapter keeps that
  instant. Cleanup reconciliation then runs within `min(ack + tool_grace,
  wall)` (AD4, VO1), and AD9 decides cleanup."
- **Claude Close:** "stdin EOF does not cancel the running turn: with no
  active tool it completed the turn (CC §2 c10), and it did not stop an
  active tool (LH E2). Interrupt must precede EOF; S1's close bound and hard
  stop are the fallback."

Tests: Codex c1, OpenCode t05, Claude c10.

**AD16. Connection admission per connection (C2 §3 Backpressure, "Host"
column; runtime §8).**

> A daemon connection slot (runtime §8: four) is held by each live connection:
> a per-turn process, a Codex shared server, or an OpenCode server. It is not
> held per turn.
>
> **At dispatch, before the grant:**
> 1. Core calls `driver.prepare()`.
> 2. `Pinned` means a live connection is pinned against idle retirement until
>    the turn ends, and needs no slot.
> 3. `NeedsConnection` means Core reserves a slot exactly as S1 does, and Host
>    takes it for the new connection's life.
> 4. A pinned connection that dies before submission ends the turn with a
>    definite rejection (nothing was sent) and no retry.
>
> Idle retirement releases the slot. Codex: the last lease released. OpenCode:
> the route's idle policy, defined in `via-4sw.3.2` within runtime §8.

Evidence: `engine/drive.rs:364-371` reserves before every dispatch. Runtime §8
counts servers against the four slots.

Tests:
- Four sessions with live servers each resume without waiting for a slot.
- A fifth session's first turn waits until one server retires.
- Per-turn routes behave exactly as S1.

**AD17. Auto-decline deadline (C2 A6; PK-CX §4; PK-OC §5).** One value, **5 s**, on the
control path, for every adapter.

Reasons:
- A6 is the recorded, owner-approved C2 value.
- No vendor showed a shorter timeout.
- Replies are immediate in practice (OpenCode reject 2 ms, OC §2).
- One Core-configured value avoids per-adapter drift.

PK-CX §4's 1 s default and its `codex_never_ask` expectation, and PK-OC §5's
"minimum of remaining operation budget and 1 s" (`opencode.md:546`), change to
5 s, capped by the remaining operation budget as before.

**AD18. Effort validation (C2 §5; C1 §4 `effort` row).**

> - `plan` and `check_turn` validate effort purely: a canonical C1 value must
>   have a mapping in the route's compiled table, and any value the route knows
>   to be invalid (Claude outside `{low, medium, high, xhigh, max}`, VC2; empty
>   strings) is refused `invalid_params` (`field: effort`) before a receipt.
> - A value that can be judged only against a discovered catalog (a Codex
>   model's advertised efforts, an OpenCode model's `variants`) is checked
>   inside `run_turn` after discovery and before vendor submission (`turn/start`,
>   `prompt_async`). A mismatch ends with `Err(Rejected(InvalidParam
>   {field: "effort"}))` → `failed(submit_failed)` with `failure.data.field:"effort"`.
>   No vendor turn starts and nothing is resent.
> - Once the catalog is cached, `check_turn` applies it, so later turns get the
>   pre-receipt `invalid_params`.

Evidence: the Codex v2 schema's `ReasoningEffort` is a free non-empty string
"advertised by the model", so no wire-level enum exists to check before
discovery; OpenCode accepts any `variant` silently (OC §2); C1 §4 requires
unknown values to be refused.

Tests:
- Claude `effort:"extreme"` → `invalid_params` with no launch.
- Codex fixture: an effort absent from `model/list` → `failed(submit_failed)`,
  `failure.data.field:"effort"`, and no `turn/start` written.
- The same value on the next turn → `invalid_params` from the cache.

### 3.4 C1 amendments

| ID | Target | Change |
|---|---|---|
| AC1 | P13 summary row (line 73) and the recorded owner-approval row (line 846) with its prose (line 838); §3.1; §3.2's `allow_untested` paragraph; §4 `allow_untested` row; §8.1 "version refused (P13)" | Replace with AD7's rule. The two P13 rows become "Version rule: every vendor version is supported; refused only on demonstrated handshake breakage; `untested` warns until the maintainers' check (owner OD1, 2026-09-30, superseding the 2026-09-26 P13 approval)". `allow_untested` is accepted, stored and has no effect: every version is supported unless refused for breakage, which it cannot waive. §8.1 reads "handshake check refused (AD7)". |
| AC2 | §3.5 `quiescent`/`pending` definitions | Add: "`pending` exists only before the cleanup deadline; a settled result is `quiescent` or `uncertain`. OS group-absence evidence covers the agent's own group; descendants outside it that no vendor item tracks are not part of cleanup and are reported in `leftovers` (§5)." Reported tool items on server routes still count (P7), and §7.4's no-launch rule, §7.5, §7.3 and §7.6 are unchanged. |
| AC3 | §8.1 `harness_unavailable` | Add: "or the stored adapter version is not compatible (`data.reason: adapter_version`)". |
| AC4 | §5 `usage` row | Add: "`turn` sums the model calls of the vendor session the turn ran in; delegated sub-agent sessions may be excluded (OpenCode task sessions are)". |
| AC5 | §8.2; §7.2 transition row `running/submitting` → `failed` (`submit_failed`) (line 660) | §7.2's cause "vendor rejected the submission with a definite error" becomes "the submission was rejected before acceptance, by the vendor with a definite error or by the adapter before any vendor submission". §8.2: add: "A vendor failure after acceptance is `failed` with the vendor's class; `submit_failed` is only before acceptance; HTTP 401/403 → `auth`". Replace the `submit_failed` row's meaning ("vendor rejected the submission before acceptance") with: "the submission was rejected before acceptance, either by the vendor or by the adapter before any vendor submission (a failed handshake check, AD7, or a parameter the discovered catalog rejects, AD18); `failure.data` names the adapter-side reason" and its "Set by" with "Core (from adapter rejection or observation)". |
| AC6 | §4 `harness` row | Add: "`fake` is a test double, available only with runtime §11.1's fixture configuration". |
| AC7 | §5 `warnings` list | Add `config_switch_unverified`: one warning per receipt or envelope listing every category whose requested inheritance setting VIA could not apply or could not verify, `data.categories: [{category, requested, effective}]` (AD13). |
| AC8 | §4 `effort` row | Add: "a vendor value that can be judged only against a discovered catalog is refused at submission, `failed(submit_failed)` with `failure.data.field:"effort"`; no vendor turn starts" (AD18). |
| AC10 | §5 envelope; §3.6 `close` result; §6 `session.closed` data | Add `leftovers` with AD20's shape and field semantics (`comm` is process-controlled; `total` is a lower bound when `incomplete`). Present on per-turn envelopes, on `server_lost` envelopes (one shared snapshot) and on a close that stopped the server, where a keyed replay returns the stored report; `null` elsewhere. |
| AC9 | §5 `state`, `failure` row (line 573) | `failure` = `{class, message, vendor_code?, retryable, data?}`. `data` is present only for an adapter-side `submit_failed` and has `reason` (`"invalid_param"` or `"handshake_refused"`) and, with `invalid_param`, `field` (the C1 parameter name). It is bounded to 256 bytes, never holds vendor text, and follows §8.1's `data.field` naming. `vendor_code` keeps only vendor codes. Core DTO: `Failure` (`api.rs:1938-1946`) gains `data`. |

### 3.5 Runtime-contract amendments

| ID | Target | Change |
|---|---|---|
| AR1 | §6.1 auto-start | Add: "Auto-start also forwards each name in `via-adapters`' fixed `BOOTSTRAP_ENV` that is set in the client (`HOME`, `PATH`, `LANG`, `USER`, `LOGNAME`, `XDG_RUNTIME_DIR` and the three `VIA_FAKE_*` names). The values are read once by `AdapterConfig`, never logged or stored, and reach a vendor only through that adapter's allow-list. No credential variable is listed." Evidence: `client.rs:239-260` clears the environment. Test: the daemon's environment names (read from `/proc`, not values) equal the set names. |
| AR2 | §5 Host (report-only scan); §5.1 step 4 (line 458) and lines 485–486; §8 bounds; §10 rows for C1 §7.5 (line 1132) and the coding standard (line 1137); §11 F22 test (line 1313) | Option A only (conflict 4). "A report-only leftover scan (C2 AD20) may read the environment of a same-uid process started at or after the vendor, through one `/proc/<pid>` descriptor, solely to match the exact `VIA_PROCESS_MARKER` entry; nothing from it is kept except the report, and the marker never authorizes a signal or proves ownership or liveness." §5.1's anchor verification still reads no environment; §5.1 step 4 (line 458) still commits the durable vendor facts, while the option-A start ticks travel in `Spawned` and stay memory-only. §8 gains the scan bounds (1 s or `close_by`, one task, 256 KiB per environment). F22 becomes "marker checks never authorize a signal". The Store `anchors` table is unchanged. |
| AR3 | §11.1 | Fake fixture configuration is read by `via-adapters`' `AdapterConfig`. The names and rules are unchanged. |
| AR4 | §8 "Active private connections" row | "Four live connections daemon-wide (per-turn process or persistent server, each with its anchor); a slot is reserved only for a new connection (C2 AD16)." |
| AR5 | §2 (lines 52 and 74) | "Types an adapter produces (capabilities, route plan, refusal, observations, class hints) are `via-adapters` DTOs; Core keeps C1 request, envelope and event DTOs." |
| AR6 | §5 `ProcessOwner` and the Store `anchors` table | A persistent server's anchor has no turn owner. The Codex and OpenCode slices design a non-turn owner (server or session) with its Store schema, evidence folder, admission, recovery and shutdown, in `crates/via-store`, `crates/via-host` and `crates/via-wire`. Evidence: `ProcessOwner {session_id, turn}` (`via-host/src/lib.rs:22-27`); `anchors` references `turns`. |

### 3.6 Vendor-packet amendments

**Claude Code packet (PK-CC).**

| ID | Section | Change |
|---|---|---|
| VC1 | PK-CC §1, §3 | 2.1.285 was observed. The version rule is AD7: drop the exact set, `allow_untested` refusals and the frozen executable identity. Check init `claude_code_version` per launch. |
| VC2 | PK-CC §4 effort | Unknown effort is ignored with a stderr warning. VIA validates against `{low, medium, high, xhigh, max}` (help 2.1.285) before launch (`invalid_params`). Effort is not observable. |
| VC3 | PK-CC §4 model | Init `model` is resolved only for aliases. |
| VC4 | PK-CC §5 terminal | AD11 classification; AD5 synthetic messages. |
| VC5 | PK-CC §5, §6 | AD8: denials from `permission_denials`; decline-correlated entries are suppressed. The stdio decline reply is live-verified (c11b) but unused (K6). |
| VC6 | PK-CC §5 usage, row 236 | `result.usage` is the turn aggregate (AD6). Remove "`message.usage` → progress keyed by message ID": the snapshots are partial (c9a 6 vs 177). `total_cost_usd` → `cost {scope: session_cumulative}`. `fallback_credit`, `costBasis` → `vendor`. |
| VC7 | PK-CC §7 | Early EOF with no active tool completes the turn (c10); EOF does not stop an active tool (LH E2), so it never replaces the interrupt (AD15). |
| VC8 | PK-CC §4, §8 config | §5.4.1 Claude row; qualify the hook, plugin, skill and agent switches in `via-p98.3.4`. |
| VC9 | PK-CC §7 cleanup | Bash has its own session and `sleep` its own group (c7). AD9. |
| VC10 | PK-CC §2 describe/models | The instance version comes from init (AD7). The bundled catalog is kept. The HostProbe `--version` discovery is dropped. |
| VC12 | PK-CC §7 cancel and close | AD19's Claude row: interrupt, then EOF (EOF does not stop an active tool, LH E2), then S1's close and hard stop. SIGKILL of Claude leaves every tool (LH E4); those are leftovers (AD20). |
| VC11 | PK-CC §9 tests | Amend `claude_preflight_pure_version`, `claude_normalizer_accounting` and `claude_never_ask` per §3.7. |

**Codex packet (PK-CX).**

| ID | Section | Change |
|---|---|---|
| VX1 | PK-CX §1 | 0.159.2 was observed. The schema sha becomes a record of the maintainers' check, not a runtime refusal. The version rule is AD7. The instance version is parsed from `initialize.userAgent`; the parse rule is qualified in `via-5lr.3.1`. |
| VX2 | PK-CX §1 | C4/C4b correction: the model attempted the write through code-mode `exec`, and no item appeared. |
| VX3 | PK-CX §5 | Final text is `final_answer` only (K11). |
| VX4 | PK-CX §7 | Keyless per-request `last` samples add; scope `turn`; `total`, `cacheWriteInputTokens` and `modelContextWindow` → `vendor`. |
| VX5 | PK-CX §5 errors | Every refusal is -32600. The `SteerError` mapping comes from fixture text only. An unknown method is -32600. |
| VX6 | PK-CX §5 classes | AD11. |
| VX7 | PK-CX §5, §6 | Fast denials emit no item (K12). "Complete history and no open items → quiescent" is scoped to reported items (AD9). |
| VX8 | PK-CX §4 environment | `--disable hooks` when hooks are off (verified). MCP suppression is unverified (§5.4.1). Both enter `config_hash`. |
| VX9 | PK-CX §2 | The first `initialize` took 38 s on a fresh SQLite home. The handshake deadline is the turn's remaining wall time. |
| VX10 | PK-CX §3 | `approvalsReviewer` and `excludeTurns` are live-covered. Overrides persist as thread defaults, so send the frozen values every turn. |
| VX11 | PK-CX §3 model | Catalog via `model/list` on an owned live server (§5.3). Bad model and auth fail after acceptance (AD5). Live checks use `gpt-6-luna` (K4). |
| VX12 | PK-CX §5 | `warning`/`configWarning` are activity only. `instant_interrupt` is a watch item for P7. `write_stdin_approval` (now stable and on) produces approval requests only under an approval policy other than `never`; under `never` any such request is declined like the others (no change; qualify in `via-5lr.3.3`). The `guardianv2.thread_context` removal is unused, so no change. |
| VX13 | PK-CX §4, §8 | AD17: the decline deadline is 5 s. |
| VX15 | PK-CX §3 effort | AD18: canonical efforts checked in `plan`; model-advertised efforts checked from `model/list` before `turn/start`. |
| VX16 | PK-CX §1 ("no experimental capabilities"), §6 cancel | K17 replaces the blanket rule; no experimental capability is sent by default. `turn/interrupt` alone and `thread/unsubscribe` leave background terminals running (LH E1, E2u); they stay the server's until it closes. `thread/backgroundTerminals/clean` is recorded as the Codex mechanism of a future kill option (AD20). |
| VX17 | PK-CX §6 server close | Server close (idle retirement, AD16) is stdin close, which stopped every tool under the sandbox (LH E2c); under full access graceful close is untested and grandchildren survived SIGKILL (LH E4). Then S1's hard stop. Idle retirement is not reported (AD20 limitation). |
| VX14 | PK-CX §8 tests | Amend `codex_pin_handshake`, `codex_usage_snapshot`, `codex_never_ask` and `codex_cleanup_60s` per §3.7; add server-close cases (VX17). |

**OpenCode packet (PK-OC).**

| ID | Section | Change |
|---|---|---|
| VO1 | PK-OC §4 terminal | **Three separate steps: acknowledgement, terminal, cleanup.** (0) *Acknowledgement* (cancel only). After VIA's `POST /abort` for the in-flight turn, the first `session.error` with `MessageAbortedError` for the session (attributed to the single in-flight turn, VO4) together with the session's idle is the acknowledgement. The adapter records that instant when both have been seen, whatever the assistant or tool state, and ends the turn `Interrupted` at once; P7 applies from that instant (AD4). Later assistant and tool updates for the turn are cleanup evidence or late observations, never a precondition. (1) *Terminal* (always, including while step (0) waits for an acknowledgement). After the session returns to idle following this turn's acceptance, the adapter reconciles by repeated reads (200 ms apart, cursored pages per VO2) until every assistant whose `parentID` is the turn's user message is completed. Tool-part state is not part of terminal evidence. Shapes: (a) `finish:"stop"` → `Completed`, `EndTurn`. (b) `finish:"tool-calls"` with a tool part in `status:error` from a VIA permission reject, then idle → `Completed`, `Other`, with the decline recorded (AD8). (c) An assistant `error` → `Failed`, class per AD11. (d) No assistant created, with an uncorrelated `session.error` (t14, t15) → `Failed`, `vendor_code` from the error name or stack, `vendor_error`. **Races with a cancel:** whichever of (0) and (1) completes first decides. A natural `Completed` or `Failed` terminal completed with no `MessageAbortedError` seen is retained as that terminal, and Core disposes it as S1 does (the vendor ignored the order: `completed`/`failed`, `cancel.outcome: requested`; C1 §7.6 natural rows, `engine/terminal.rs:202`). Recognition is bounded by the wall deadline; at the wall, AD4's path applies (the driver sends `POST /abort` within S1's cleanup bound). No text is invented. (2) *Cleanup,* only after `Interrupted`: reads continue until no tool part of the turn is `pending` or `running`, bounded by `min(ack + tool_grace, wall)` (AD4), not by `close_by`; unresolved at the bound → `Uncertain` (AD9). Structured output is unsupported (K8). Evidence for (0): in OC runA the abort error, then idle, arrive before the tool's and the assistant's final updates (`sse1.raw` lines 788, 791, 797, 800). |
| VO2 | PK-OC §3, §4 | K8. A list read fails when its page includes a format-bearing message **and** has no next cursor. `format:{type:"text"}` fails the same way. Plain turns therefore omit `format`, and reconciliation reads use cursored pages or single-message GETs of assistants. Track sst/opencode#26929, #40169 and PR #37541. |
| VO3 | PK-OC §7 | `APIError 401/403` → `auth` (K9). 429: 5 internal retries (activity only), then `rate_limit`. |
| VO4 | PK-OC §4 | A 204 does not validate the model. An uncorrelated `session.error` is attributed to the single in-flight turn, which is guaranteed by the one-active-turn gate and no v2 use. |
| VO5 | PK-OC §3 | `variant` is accepted silently. Validate against the server's `/provider` readback inside `run_turn` before `prompt_async`. A mismatch is `Rejected(InvalidParam("effort"))`. |
| VO6 | PK-OC §7 | Keyed assistant samples sum; `input` excludes cache-read; children are excluded (AC4); scope `turn`. |
| VO7 | PK-OC §6 | Tools run in their own group and survived a server-group SIGKILL. An aborted part reports `completed`. AD9. |
| VO8 | PK-OC §5 | Add the observed SSE types as known-ignored or mapped: `plugin.added`, `catalog.updated`, `reference.updated`, `integration.updated`, `file.watcher.updated`, `session.diff`, `permission.*`, `session.error`, `session.status retry`, `session.next.*`. |
| VO9 | PK-OC §2 config | The worktree resolves to the git root. `.claude/skills` from there load despite `--pure` and add `external_directory: allow`, and cannot be switched off (§5.4.1). Record the resolved worktree and `GET /skill` inventory per session. Instruction-file absence is inference only, so qualify it in `via-4sw.3.4`. Project `opencode.json` is ignored (verified). |
| VO10 | PK-OC §3 | AD10. |
| VO11 | PK-OC §6 | Server death observed while the original daemon supervises is `server_lost` (AD4). After a daemon crash, recovery follows P12: the turn stays `unknown`, with no vendor-state reliance and no known outcome fabricated from a later-dead server (PK-OC lines 613–619). Restart continuity is proven. |
| VO12 | PK-OC §2 | The free catalog changes remotely, so the live check re-verifies the model. |
| VO13 | PK-OC §3 | The task child's permission array lacks the parent's `*: allow` (inference: allow comes from the global config). Keep the generated global config never-ask-safe. |
| VO18 | PK-OC §6 close | Server close is `POST /instance/dispose`, then S1's hard stop; the server has no SIGTERM handler (LH E3). `/abort` and dispose kill only the tool's own group; setsid and double-fork descendants survive (LH E1, E2i) and are reported by a C1 close that disposes the server; idle retirement does not report (AD20). |
| VO14 | PK-OC §8 tests | Amend OC01, OC05, OC07, OC08, OC11 per §3.7. |
| VO15 | PK-OC §3 version gate (lines 337–347) | Replace "Pin tested range to exactly 1.18.32 … actual startup health must match the pin. A2/P13 use `untested` plus explicit `allow_untested` … handshake/schema failure is `harness_unavailable`" with AD7: health reports the version; `untested` warns; only a demonstrated handshake breakage refuses, cached per AD7. The server key keeps `exact_vendor_version` (a new version starts a new server). `describe` stays process-free. |
| VO16 | PK-OC §5 decline (line 546) | AD17: "Reply within the minimum of remaining operation budget and 5 s". |
| VO17 | PK-OC §3 effort row | AD18: `variant` checked against the server's `/provider` readback before `prompt_async`; canonical values without a mapping refused in `plan`. |

### 3.7 Normative occurrences to amend (S-SPEC audit)

S-SPEC applies every row to the text; a code owner is named where the row needs code.

| File | Location | Amended by |
|---|---|---|
| C2 | §1 rules 1 and 2; A2; A6; §2 enum, sketch, types, contract points (Submission boundary, Delayed vendor identity, Observations before turns, Interrupt, Independent lanes, Recover); §4 table; §5; §6.2 rows (Steer, Interrupt, Close, Observations, Usage, Class hints, new config row); §7 items 4, 5, 6, 9, 10, 11, 14, 15; §4 leftover result fact and §2 `TurnEnd`/driver `CloseReport` fields | AD1–AD20 (AD20 code: S-LEFTOVER) |
| C1 | Summary P13 row (line 73); the recorded owner-approval table's P13 row (line 846) and its prose (line 838); P7 row (unchanged); §3.1; §3.2's `allow_untested` paragraph; §3.5 `quiescent`/`pending`; §3.6 close result; §5 `leftovers`; §6 `session.closed` data; §4 `harness`, `allow_untested` and `effort` rows; §5 usage and warnings; §5 `failure` shape (line 573); §7.2 `submit_failed` transition (line 660); §7.5 group-absence sentence; §8.1 (line 770, "version refused (P13)"); §8.2 general text and the `submit_failed` row (line 792) | AC1–AC10 (AC10 code: S-LEFTOVER) |
| runtime | §2 lines 52 and 74; §3 and §4 route and Wire close results (carry the report); §5 `ProcessOwner`; §5 Host; §5.1 lines 485–486; §5.2; §6 Store: `commit_closed`'s `ClosedRecord` carries the report into the `session.closed` event, the stored `close_result` and the close operation's result (existing JSON columns; `anchors` unchanged); §6.2 shutdown and §7 recovery (no report, AD20 limitation); §8 connections row and scan bounds; §10 lines 1132 and 1137; §11 F22 test line 1313; §6.1; §11.1 | AR1–AR6, AD20 (report code: S-LEFTOVER) |
| PK-CC | §1 lines 13–14 and the A2/P13 row (line 30); §2 describe row; §3 lines 127–143; §5 row 236 and the usage paragraph; §9 tests `claude_preflight_pure_version` (exact 2.1.283), `claude_normalizer_accounting` (keyed snapshots), `claude_never_ask` (denial dedup plus decline suppression); §7 cancel and close; §10 items 1–3 (superseded) | VC1–VC12 |
| PK-CX | §1 pin and "no experimental capabilities" (line 23); §3 effort; §4 deadline; §5 final text; §6 cancel and server close; §7 usage; §8 tests `codex_pin_handshake` ("unknown version gated" → warning; "no experimental flag" → K17, none sent by default), `codex_usage_snapshot` ("never sum" → sum keyless `last`), `codex_never_ask` (1 s → 5 s), `codex_cleanup_60s` (driver-applied window per AD4); §9 P7/C2 item 6 texts | VX1–VX17 |
| PK-OC | §3 version-gate paragraph (lines 337–347); §3 output-schema row (line 356: "null means explicit `{type:"text"}`" → omit `format`, VO2), effort and steer rows; §4 terminal paragraph; §6 abort, close and cleanup; §5 decline deadline (line 546, 1 s → 5 s); §7 class hints and usage; §8 OC01 ("bad health/version refused" → bad health refused, unchecked version warns), OC05 (incomplete assistant → repeated reconciliation), OC07 (decline within 5 s under saturation), OC08 (cleanup per AD4 window, AD9 and AD19; server close per VO18), OC11 (`ProviderAuthError`/ambiguous 403 → 401/403 `auth`; format clearing → omit `format`; invalid variants → AD18 submission rejection) | VO1–VO18 |
| invariants | rule 2 "The adapter version also stays fixed" | AD12's replacement wording (owner-approved), applied by the coordinator in S-SPEC |
| invariants | rule 1 "never reads … vendor credentials" | conflict 4: owner choice; under A the scan buffer can transiently hold credential values |
| coding-style | lines 164–165 (no vendor-environment scan), 173–174 (never recovered from vendor environments), 175–176 (never read, copy or log credentials) | conflict 4: AR2's narrowing under A |
| platform packet | §5 lines 202, 207–208, 324–327; P-I3 line 371 ("vendor environment is never read") | conflict 4: AR2's narrowing under A; lines 269 and 328 unchanged |
| C1 | §7.5 lines 710–711 ("no scan of vendor environments"); §9 lines 814–823 (credential prohibition; the OpenCode password exception does not cover the scan) | conflict 4: AR2's narrowing under A; the signal rules unchanged |
| PK-CX | line 205 (never read credential contents) | conflict 4: AR2's narrowing under A (the scan matches only the marker entry; its buffer is transient) |
| PK-OC | lines 223–224 (no environment dumps), OC12 (line 685), the mirrored C1 amendment (lines 740–749) | conflict 4: AR2's narrowing under A, mirrored in both places |

## 4. Disposition of every re-probe finding

Key:
- **AD/AC/AR/V\***: an amendment ID;
- **AI**: adapter-internal;
- **OD**: an owner decision;
- **NC**: no change, with the reason.

| Item | Finding | Disposition |
|---|---|---|
| CC D1 | 2.1.285 outside `{2.1.283}` | OD1 → AD7, VC1 |
| CC D2 | `fallback_credit` | VC6 (`vendor` data) |
| CC D3 | help cosmetics | NC: no mapped flag changed |
| CC D4 | no other difference | NC |
| CC P1 | decline encoding verified; no event on the stdio path | VC5, AD8 |
| CC P2 | unknown effort ignored | VC2; AD7 limits |
| CC P3 | init model unresolved for full names | VC3 |
| CC P4 | `success` + `is_error` | AD11, VC4 |
| CC X1 | version gate vs auto-update | OD1 |
| CC X2 | synthetic messages | AD5 |
| CC X3 | two denial shapes | AD8 |
| CC X4 | A6 decline path unused on `none` | NC: K6; the decline code remains for unknown control requests (c11b fixture) |
| CC X5 | tools leave the group | OD3 decided: leftovers reported (AD20), cleanup unchanged (AD9), VC9, VC12 |
| CC X6 | effort unobservable | VC2; AD7 lists it as unprovable |
| CC X7 | EOF finishes the turn | AD15, VC7 |
| CC new | max-turns, schema replace/clear, B7 environment, strict sandbox | NC: matches the packet |
| CC usage | assistant snapshots partial | AD6, VC6 |
| CX drift 1 | version churn | OD1 → AD7, VX1 |
| CX drift 2 | schema sha | VX1 |
| CX drift 3 | `CodexErrorInfo` additions | AD11, VX6 |
| CX drift 4 | `promax`, `serverName`, cursor forms | NC: unused |
| CX drift 5 | plugin definitions and fields removed | NC: unused by VIA; the matrix records the removal |
| CX drift 6 | `Turn.error` description; `null` on interrupt | AI: treat as optional |
| CX drift 7 | `instant_interrupt`; `write_stdin_approval` stable and on; `guardianv2.thread_context` removed | VX12 (each item dispositioned) |
| CX corr 1 | C4b wrong | VX2 |
| CX corr 2 | owner's hooks ran | OD2 → AD13, VX8 |
| CX X1 | fast denials never on the wire | AD8, AD9, VX7 |
| CX X2 | usage per request | AD6, VX4 |
| CX X3 | commentary in final text | AD15, VX3 |
| CX X4 | -32600 everywhere; auth and model after acceptance | AD5, AD11, VX5, VX11 |
| CX X5 | inherited config | OD2 → AD13 |
| CX X6 | version churn | OD1 |
| CX X7 | cold start 38 s | VX9 |
| CX X8 | experimental cleanup verbs | Recorded as the future kill option's Codex mechanism (AD20, VX16); not used by default |
| CX X9 | steer, interrupt, unsubscribe, resume, stdin loss fit | NC |
| CX dec e | model | K4 (`gpt-6-luna`), VX11 |
| CX run | `bd prime` ran in c6 | NC for the design; a coordinator hygiene item |
| OC drift 1 | `tool-calls` terminals; busy merge | VO1, AD10 |
| OC drift 2 | tools escape the server group | OD3 decided: leftovers reported (AD20), VO7, VO18 |
| OC drift 3 | `format` read defect | K8, OD4, VO2 |
| OC drift 4 | 401 is `APIError` | AD11, VO3 |
| OC drift 5 | 204 does not validate the model | AD5, VO4 |
| OC drift 6 | two steer forms | K7, AD10 |
| OC drift 7 | unlisted SSE types | VO8 |
| OC drift 8 | usage measured; children excluded | AD6, AC4, VO6 |
| OC drift 9 | worktree = git root; skills despite `--pure`; instruction files absent (inference) | VO9, §5.4.1 |
| OC drift 10 | free catalog changed | VO12 |
| OC drift 11 | `/abort` on an unknown session → 200 | AD15 (abort 200 is never evidence) |
| OC drift 12 | aborted part `completed` | VO7; AI: counts as ended |
| OC X terminal, uncorrelated, acceptance, cleanup, schema, steer, usage, config | pressure items | VO1, VO4, AD5, AD9, K8, AD10, AC4, AD13 |
| OC X retries | no retrying observation | NC: activity only; no idle trip observed (5 retries within 600 s) |
| OC closed items | usage, abort, restart, precedence, reject flow, hostile config | NC: confirm the packet (VO6, VO7, VO11) |
| OC t22 | crash turn never reconciled | VO11 |
| OC task perms | child permission array | VO13 |
| OC variant | silently accepted | VO5 |
| OC addendum | real defect; sharper pagination rule; `text` format fails; upstream issues and PR | OD4, VO2 |

## 5. Core behind C2

### 5.1 Inventory of fake- and harness-specific production sites

Scope: production code in `via-core/src` and `via-cli/src` (outside
`#[cfg(test)]` modules and test files), plus the `via-adapters` names that leak
into Core. A comment-only site's replacement is a generic rewording.

| # | Site | Fake-specific | Generic replacement |
|---|---|---|---|
| 1 | `via-core/src/lib.rs:6` | `pub use via_adapters::FakeConfig` | re-export `AdapterConfig`, `BOOTSTRAP_ENV`, `harness_names` |
| 2 | `engine.rs:18, 263-330` | `Engine::open*` take `FakeConfig` | take `AdapterConfig`; build `AdapterSet` |
| 3 | `api.rs:16, 22-24` | "fake session" doc; `harness: String` required | generic doc; `harness: Option<String>` (C1 §4) |
| 4 | `api.rs:38` | cwd doc "the fake's default" | generic |
| 5 | `api.rs:155-170` | `PerTurn` and `Overrides` hold only deadlines ("on the fake route") | `Overrides` gains effort, bound, `output_schema`, `max_steps` and vendor, each `Omitted`/`Null`/`Given` |
| 6 | `api.rs:229-265` | `SpawnParams::check` refuses instructions "on route fake"; `Capabilities::fake().require` (doc 231-232) | plan capabilities and `require` (AD2) |
| 7 | `api.rs:337-477` | `PerTurn::fake_overrides` refuses effort, schema, `max_steps`, bound and vendor on the fake route | Core keeps the C1 shape checks; values go through `plan`/`check_turn`; `Refusal` → `ApiError` with `Named { field, route }` |
| 8 | `api.rs:567` | `SteerParams` doc "fake must refuse" | generic |
| 9 | `api.rs:862-957` | `describe(fake_available)`; `harness != "fake"`, `model != "fake"`; literal JSON; `allow_untested` dead code (878-882) | `DescribeRequest` → `plan`; serialize `RoutePlan` |
| 10 | `api.rs:967-979` | `models`: builtin fake entry | `AdapterSet::models` |
| 11 | `api.rs:1039-1051` | `Named::fake` | `Named::route(field, route)` |
| 12 | `api.rs:1275` | `HARNESS_UNAVAILABLE` doc "the fake route" | generic |
| 13 | `api.rs:1646-1650` | `FAKE_ROUTE`; `DEFAULT_WALL_MS` doc "fake turn" | removed; generic doc |
| 14 | `api.rs:1653-1766` | `Support` without `Partial`; `Verbs`, `ParamSupport`, `UsageSupport`, `Capabilities`, `Capabilities::fake`, `require` "route fake" | moved to `via-adapters` (AD2) |
| 15 | `api.rs:1687` | `FAKE_TOKEN_SCOPE` | the session's stored `capabilities.usage.tokens` |
| 16 | `api.rs:1776-1812` | `Effective` lacks `output_schema` and vendor; `Effective::fake`; `inherit` updates only deadlines | `Effective::first(&plan, &overrides)`; `inherit` applies every per-turn member with C1 P5 omission, null clears for `output_schema`/`max_steps`, and a re-validated bound |
| 17 | `api.rs:1824-1858` | Core `RoutePlan`, `::fake`, "fake agent reports no version"; `CARGO_PKG_VERSION` | `via_adapters::RoutePlan` (AD2, AD12) |
| 18 | `api.rs:1922-1940` | `FailureClass` "for the fake route"; lacks the adapter classes | add C1 §8.2's `auth`, `rate_limit`, `context_exceeded`, `budget_exceeded`, `resume_mismatch`, `server_lost`, `structured_output_invalid` |
| 19 | `api.rs:1962-1985` | `Usage::fake` (total only) | `Usage` from the turn ledger (AD6) |
| 20 | `api.rs:2141` | `expect(dead_code, "fake route reports no denials or declines")` | used |
| 21 | `api.rs:2265-2270` | `Bound::NONE` "fake enforces no bound" | the bound from `Effective` |
| 22 | `engine/receipt.rs:120-138` | an omitted `cwd` defaults to `adapter.fake_cwd()` | Core captures the daemon's startup directory at `Engine::open` |
| 23 | `engine/receipt.rs:257-263` | `harness != "fake"`, `model != "fake"` | `AdapterSet::plan` |
| 24 | `engine/receipt.rs:267, 390` | `Effective::fake`, `fake_overrides` | #5, #7, #16 |
| 25 | `engine/receipt.rs:273-281, 305, 468` | receipt plan, capabilities, frozen params `{"harness":"fake",…}`, turn-receipt warnings | from the plan; frozen `{harness, route, adapter_version, model, cwd, allow_untested, instructions?, vendor?, inherit switches}` |
| 26 | `engine/receipt.rs:546-554` | `steer` always `UNSUPPORTED_VERB` ("fake's unsupported mutation") | the stored capability check; select the active turn (`--expect-turn` → `turn_mismatch`; none → `no_active_turn`; submitting → wait for acceptance); `driver.steer`; `SteerError` → C1 errors; commit `steer.delivered` |
| 27 | `engine/drive.rs:10-12, 173, 1427-1540, 1799, 1980-1990` | `FakeObservation`, `FakeAcceptanceObservation`, `FakeTerminalEvidence` | `Observation`, `TurnEnd` (§3.2) |
| 28 | `engine/drive.rs:364-371` | a connection slot before every dispatch | `driver.prepare()`; a slot only for `NeedsConnection` (AD16) |
| 29 | `engine/drive.rs:621-632` | fake default cwd (and doc) | #22 |
| 30 | `engine/drive.rs:1296-1318` | the nine-argument `adapter.execute` | `driver.run_turn(TurnSpec, TurnCx)`; the driver is held in the session slot, opened at first dispatch or reopen |
| 31 | `engine/drive.rs:1859` | `Usage::fake(turn_tokens)` | #19 |
| 32 | `engine/terminal.rs:5, 149, 335` | `FakeTerminalEvidence` in `dispose`/`classify` | `TurnEnd` with retained terminal (AD4) |
| 33 | `engine/terminal.rs:38-105` | envelope: plan fake (38), empty denial/decline collectors (51-53), `harness:"fake"`, model `fake`/`fake`, effort null (65-72), `vendor_session_id:None` (76), `Bound::NONE` (78), `structured_output:None` (83), `steps:None` (88), `Cost::UNAVAILABLE` (90), `transcript:None` with fake comment (97-100), `vendor_options:{}` (102), vendor `turn_id` only (103-105) | from the session plan, the turn's `Effective`, the collected denials and declines, `IdentityConfirmed` (ID, transcript), `VendorTerminal` (structured output, steps, cost, vendor), `TurnEnd.instance` (vendor version and status on every outcome, AD7) and the frozen vendor options |
| 34 | `engine/terminal.rs:340-365` | every failure `VendorError`; failed stop reasons forced to `error`/`interrupted` | `class_hint` → class; keep the adapter's `StopReason` (Claude `max_steps`) |
| 35 | `engine/terminal.rs:376` | `exit` always `Some` | optional (server routes `null`) |
| 36 | `engine/terminal.rs:420-431` | `canonical_stop_reason` maps vendor words | the adapter's `StopReason` |
| 37 | structured output (new Core work) | no Core validation of `structured_output` | Core validates against the frozen schema (C1 Q2, draft 2020-12). Output present but invalid → `failed(structured_output_invalid)`. Schema requested, terminal `completed`, no output → the result keeps its status and carries warning `structured_output_missing` (C1 §5); it is not a failure class. The JSON Schema validator dependency is owner-approved (K14); S-CORE selects the crate under `cargo deny`. |
| 38 | `engine/status.rs:161` | `describe(fake_available)` | `plan` |
| 39 | `engine/read.rs:14, 320, 407` | `FAKE_TOKEN_SCOPE`; `vendor_identity_verified: false` always | stored scope; the session's persisted verification flag |
| 40 | `engine/recovery.rs:11, 413, 742`; `engine/reprobe.rs:222` | `FakeRecovery` | `AnchorRecovery`; per-session `AdapterSet::recover` for cleanup (AD9) |
| 41 | `engine/stop.rs:262, 330` | `FakeShutdown` | `AdapterShutdown` |
| 42 | `via-cli/src/server.rs:18, 273-279` | `FakeConfig::from_environment` | `AdapterConfig::load(BootstrapEnv::from_process(), harnesses)` |
| 43 | `via-cli/src/client.rs:252-260` | forwards `VIA_FAKE_*` | forwards `via_core::BOOTSTRAP_ENV` (AR1) |
| 44 | `via-cli/src/server/config.rs:56-62` | `daemon.json` `File` lacks `harnesses` | an opaque `harnesses` member passed through unparsed |
| 45 | `via-adapters/src/lib.rs:150-153`, `runtime.rs:134-190, 304, 620-640`, `fake_config.rs` | `AdapterRuntime {route: FakeRoute, fake}`, `fake_available`, `fake_cwd`, `execute`, `FakeRecovery`, `FakeTurnRecovery`, `FakeShutdown` | `AdapterSet`, `fake::Adapter`, the renamed Host-fact types |

`envelope_at_maximum` (`terminal.rs:438-439`, test-failpoints only) follows #17,
#19 and #33; S-LEFTOVER adds AD20's maximum report (16 entries) to it.

### 5.2 How the harness string resolves

1. Core passes `harness: Option<&str>` and `model` unchanged in `DescribeRequest`
   (describe, spawn), and never compares them.
2. `AdapterSet::plan` parses `harness` through the table in
   `via-adapters/src/harness.rs` (`claude`, `codex`, `opencode`, and `fake` only
   when configured).
   - With no harness, it matches `model` against each configured adapter's
     current catalog (§5.3).
   - A unique match is resolved.
   - Several matches → `InvalidParam { field: "harness" }`.
   - No match → `UnknownModel`, because without a harness only a catalog can
     resolve a model.
3. The plan returns the canonical `harness` and `route` (`&'static str`). Core
   freezes them (#25) and echoes them opaquely.
4. Resume, reopen and recovery hand back `SessionRef { harness, route,
   adapter_version }`. An unknown or incompatible reference is
   `harness_unavailable` (AD12).

Tests: unique, ambiguous and absent catalog matches for a model-only spawn.
Harness given with an uncatalogued model → the vendor decides after acceptance
(AD5).

### 5.3 Catalog and metadata lifecycle

| Harness | Bundled (compiled in) | Discovered (through owned lower layers, never in `plan`) | Before discovery |
|---|---|---|---|
| Claude | PK-CC's versioned catalog (aliases, effort list VC2) | none | the bundled catalog only; an uncatalogued explicit model passes to the vendor (PK-CC §2) |
| Codex | the canonical-effort mapping (AD18) | `model/list` sent by the driver on an owned server right after `initialize` (no extra launch); cached per server instance with its version | model-only resolution fails `UnknownModel`; an explicit model passes to the vendor (400 → `vendor_error`, AD5); effort is checked against `model/list` inside `run_turn` before `turn/start` (AD18) |
| OpenCode | the selected free-model profile; the canonical-effort mapping | `GET /provider` on the session's own server during `run_turn`, before `prompt_async` (VO5); cached per instance | as Codex; `variant` checked in `run_turn` before `prompt_async` (AD18, VO17) |
| Fake | its one model | none | — |

- `plan`, `check_turn` and `models` read only the bundled data and the
  in-memory cache. A cache entry is valid while its instance lives; a new
  instance replaces it.
- `models` returns `source: bundled | discovered`.
- Nothing is persisted.

### 5.4 Per-harness configuration (replaces `FakeConfig`)

| Setting | Source | Notes |
|---|---|---|
| Environment values (`HOME`, `PATH`, `LANG`, `USER`, `LOGNAME`, `XDG_RUNTIME_DIR`) | the startup environment forwarded by auto-start (AR1) | read once; each adapter copies only its allow-list |
| `harnesses.<name>.binary` | optional `daemon.json`, absolute path | the default is a `PATH` lookup; also used for vendor replay (§8) and user pinning |
| `harnesses.<name>.inherit.{hooks, mcp_servers, plugins, skills, agents, instruction_files}` | optional `daemon.json`, booleans | OD2 switches. Default (**owner-approved**): hooks and MCP servers `false`, the rest `true`; Codex's and Claude's every category `true` (owner 2026-10-05: VIA disables nothing in Codex but memories, and Claude's default mode delivers every category, for the first release). Read at daemon start (the S1 rule). A change applies to sessions spawned after the next daemon start; each session freezes its settings. |
| `harnesses.codex.memories` | optional `daemon.json`, boolean | Codex only (owner, 2026-10-05): the default `false` launches every Codex server with `--disable memories`; `true` omits it, so Codex's own memories default applies (vendor packet §4). Part of the server key. Read at daemon start like the rest. |
| `harnesses.claude.restricted` | optional `daemon.json`, boolean | Claude only (owner, 2026-10-05): `true` launches with `--restricted`; the default `false` omits it, so Claude loads the user's own configuration like their normal Claude (vendor packet §4). Read at daemon start like the rest. |
| `checked` versions, capabilities, reserved keys, recipe | compiled into each adapter | never user config |
| Fake fixture | `VIA_FAKE_AGENT_BINARY`, `VIA_FAKE_SCENARIO`, `VIA_FAKE_SYNC_DIR` | runtime §11.1, unchanged |

Credentials never appear in config, the Store or evidence (invariant 1). The
inherited inventory the route can read (§5.4.1) is written to the turn's
evidence folder (Claude amended 2026-10-05: not recorded; vendor packet §4).

#### 5.4.1 Categories per harness (OD2)

"Verified" means seen live on 2026-09-30. Nothing marked unverified is claimed.

**Claude Code columns superseded** (owner, 2026-10-05; via-umz): Claude
launches without `--restricted` unless `harnesses.claude.restricted` is set,
its default request has hooks and MCP servers on (no `--strict-mcp-config`
unless MCP servers are requested off), and its effective states per mode,
verified live on 2.1.289 except restricted MCP servers on (`unknown`), are
in the vendor packet's §4
(`docs/specs/vendors/claude-code.md`). The Claude cells below record the
2026-09-30 state.

| Category | Claude Code | Codex | OpenCode |
|---|---|---|---|
| hooks | Settings-file hooks are ignored under `--restricted` (help; **unverified**). Plugin hooks: **unverified**. `--safe-mode` also drops prompt config, so it is not a per-category switch. | `--disable hooks`: **verified** (CX §3) | None loaded: private profile. **Not switchable to on.** |
| MCP servers | `--strict-mcp-config`: **verified** (`mcp_servers:0`) | `~/.codex` config; a `-c` override is **unverified**; inventory via `mcpServerStatus/list` (schema, **unverified**) | None: private profile; **not switchable to on** |
| plugins | Loaded (2) even under `--restricted`; no per-category switch known (**unverified**) | Plugin support exists (schema); switch **unverified** | Private profile, none from the user; **not switchable to on** |
| skills | Loaded (18); `--disable-slash-commands` (help; **unverified**) | **Unverified** | Worktree `.claude/skills` (40) load despite `--pure`: **not switchable off** (`skill: deny` unqualified inference). Inventory: `GET /skill` (called in the probe). |
| agents | Loaded (5); **no switch known** | **Unverified** | Private profile plus VIA's generated agent; **not switchable to on** |
| instruction files | CLAUDE.md is not in init; **inventory unavailable**; no per-category switch | AGENTS.md; switch and inventory **unverified** | Absence inferred only (OC §3.9); qualify |
| inventory source | Init lists plugins, skills, agents, slash commands and MCP (verified) | `configWarning` only; otherwise **unavailable** | `GET /skill`, `GET /agent`, `GET /config` (called) |

Effective state (AD13): `on` or `off` only when verified (a verified switch,
the private profile, or an inventory that lists or omits the category);
otherwise `unknown`. Every category whose effective state is not the verified
requested state is listed in the `config_switch_unverified` warning. With the
approved default (hooks and MCP off, the rest on):

| Category (default) | Claude Code | Codex | OpenCode |
|---|---|---|---|
| hooks (off) | `unknown`, **warns** | `off` (verified switch) | `off` (private profile) |
| MCP servers (off) | `off` (verified switch) | `unknown`, **warns** | `off` (private profile) |
| plugins (on) | `on` (init inventory) | `unknown`, no switch applied, **warns** | `off`, cannot inherit, **warns** |
| skills (on) | `on` (init inventory) | `unknown`, no switch applied, **warns** | `on` (`GET /skill`) |
| agents (on) | `on` (init inventory) | `unknown`, no switch applied, **warns** | `off` (only VIA's agent, `GET /agent`), **warns** |
| instruction files (on) | `unknown` (no inventory), **warns** | `unknown`, no switch applied, **warns** | `unknown` (absence inferred only), **warns** |

Qualification in each `x.3.4` bead can turn an `unknown` into a verified state,
which removes that warning.

The owner can silence a warning by setting the category to the state the route
can deliver.

Cwd identifies only where configuration is discovered, not its contents. The
evidence inventory is what records contents.

### 5.5 How the fake is reached, and its version policy

Chosen: a runtime-configured variant compiled into every build.
- `Adapter::Fake` is available only with all three fixture paths.
- Otherwise it is `harness_unavailable` and absent from `models`.

Reasons:
- `scripts/check-release-features.py` drives a fake turn through the release
  binary.
- Daemon scenario tests start the real binary.
- It is runtime §11.1's approved mechanism.

Alternatives, rejected for now:
- **A feature-gated variant.** Revisit if the owner wants the fake out of
  release, after the release check gets another turn driver.
- **Test-only construction.** It cannot reach tests that start the binary.

Version policy, owned by the fake adapter with no Core branch: the fake has no
handshake and never refuses. It reports `vendor_version: null` and
`version_status: untested` with the warning "the fake agent reports no version",
exactly as S1 does today (`api.rs:1835-1851`). It declares that its tools stay in
its group (AD9).

### 5.6 Literal guard: `scripts/check-harness-literals.py`

This is a guard against harness literals. It cannot prove that Core has no
harness-shaped behaviour; review still owns that.

- **Scope.** `crates/via-core/src/**/*.rs` and `crates/via-cli/src/**/*.rs`.
  A small lexer tracks line and block comments, string, byte-string and raw
  string literals (`r#"…"#` at any hash depth), char literals and braces.
- **Test exclusion.** Excluded: an item whose attributes include
  `#[cfg(test)]`, up to its matching brace, found with the lexer rather than by
  column. A file is excluded only if it is reached solely through a
  `#[cfg(test)] mod name;` declaration (`name.rs` or `name/mod.rs`). No path
  pattern excludes anything by name.
- **Tokens.** Every identifier, string and comment word is split into subwords
  on `_`, digits and camelCase boundaries, then lowercased. Each forbidden name
  is normalized the same way, and matched both as its subword sequence and as
  its joined form. For example `opencode` matches `open`,`code` and `opencode`,
  and `openai` matches `open`,`ai` and `openai`. Names come from the table in
  `via-adapters/src/harness.rs` plus `fake`, `acp`, `anthropic` and `openai`.
- **Exceptions.** `scripts/harness-literals-allow.txt` holds
  `path: substring: reason` entries and is expected to be empty.
- **Self-test.** `--self-test` covers `FakeConfig`, `fake_cwd`, `"codex"`,
  `OpenCodeAdapter`, `OpenAI`, a comment, a raw string with a column-0 brace,
  a `#[cfg(test)]` module, a `cfg(test)` file module, a non-test file named
  `tests.rs`, and an allow entry.
- **Gate.** Add the command after `check-layers.py` in
  `.repo-context/verification.md`. It fails on today's tree, listing §5.1.

## 6. Adding a future harness (Devin, Pi, Amp, Oh My Pi, Antigravity CLI, …)

This touches **no** `via-core` file. Per OD5a, the work is these paths plus one
enum variant, then a VIA rebuild and release.

1. **Evidence.** An isolated live probe, then `docs/specs/vendors/<h>.md` from the
   packet skeleton. The packet covers: process shape; handshake checks and what
   they cannot prove; capabilities; reserved keys; never-ask recipe; environment
   allow-list; §5.4.1 category table; cancel and cleanup evidence (do tools stay
   in the group?); class hints; usage source (per call or aggregate); catalog
   lifecycle; recovery; qualification cases. Record the terms status
   (invariant 1).
2. **Routes.** Add `crates/via-routes/src/<h>/`: typed messages with `Unknown`,
   encoders and pairing. A new transport is its own Wire slice.
3. **Adapter.** Add `crates/via-adapters/src/<h>/` with these modules:
   - `plan.rs`: capabilities, `checked` versions, reserved keys, bundled catalog,
     `check_turn`;
   - `launch.rs`: process spec, allow-list, category switches;
   - `driver.rs`: `prepare`, `run_turn`, steer, close, auto-decline, handshake
     checks;
   - `normalize.rs`: observations, `StopReason`, `ClassHint`, usage.
4. **Enum.** In `crates/via-adapters/src/harness.rs`, add one table row (C1
   name, route ID, default binary) and one `Adapter` variant. Exhaustive
   matches in `via-adapters` list every dispatch site.
5. **Config.** Add any new environment name to `BOOTSTRAP_ENV`. No new
   `daemon.json` keys beyond `harnesses.<h>.*`.
6. **Tests.** Add sanitized fixtures under
   `crates/via-adapters/tests/fixtures/<h>/`, replayed through the fake agent's
   replay mode (§8), and add `conformance_<h>.rs`.
7. **Live check.** Add `scripts/qualify/<h>.py`.
8. **Docs.** Add the name to C1 §4 (doc only). The guard reads the table.
9. **ACP harnesses.** Devin and Oh My Pi later come as profiles of the one ACP
   adapter (OD5b).

A harness that needs a new C1 concept (a failure class, warning or event) is a
C1 change, not a routine addition.

## 7. Slice plan

**Existing Bead edges (read with `bd show`):** `x.3.1` → `x.3.2` → `x.3.4` →
`x.3.3`, for `x` in `via-p98`, `via-5lr` and `via-4sw`. Each `x.3.3` depends on
`x.3.4` and blocks `via-gvg.1`/`via-d9o.1`. Each `x.3.2` depends on
`via-jm4.22`.

**Coordinator updates required:**
- create S-SPEC, S-CORE, S-LAUNCH and S-LEFTOVER;
- add the edges S-SPEC → S-CORE → each `x.3.2` and S-LAUNCH → each `x.3.2`;
- move S-LEFTOVER to the integration stage: each `x.3.3` → S-LEFTOVER →
  `via-gvg.1` and `via-d9o.1`. No adapter slice asserts a report; every
  report assertion is S-LEFTOVER's. This also orders the overlapping writes:
  Core in S-CORE; Host, Wire and Store in the Codex then OpenCode slices
  (coordinator-serialized); Host, Wire, Route, Adapter, Store and Core
  seams in S-LEFTOVER, after every adapter slice;
- add Codex-after-Claude as a sequencing edge (`via-p98.3.3` → `via-5lr.3.2`)
  if the owner wants Claude first enforced, since no such edge exists today.

**Owner gates:**
- OD2's default is approved; S-LAUNCH sets it.
- The invariant 2 edit is approved; S-SPEC applies AD12's wording.
- OD3 is decided; there is no OD3 gate.
- Leftover detection (conflict 4): the owner chose A on 2026-10-01; its rows
  are applied in `via-jm4.30` (S-SPEC-C4). A: S-LEFTOVER as below. B: S-LEFTOVER first designs
  anchor-subreaper detection. C: S-LEFTOVER is dropped and every `leftovers`
  stays `null`.

**Failure-first:** every listed test is shown red before its fix.

| Slice | Bead | Owned paths | Tests that must fail first | Acceptance |
|---|---|---|---|---|
| S-SPEC | new, coordinator | the files in §3.7, including `.repo-context/invariants.md` rule 2 (AD12 wording) | — | every §3.7 row applied or rejected with a reason; Sol review converged; the environment-scan narrowing accepted or refused by the owner (conflict 4) |
| S-CORE | **new**, coordinator-owned | `crates/via-adapters/src/**` (`harness.rs`, `capabilities.rs`, `plan.rs`, `observation.rs`, `config.rs`, `fake/`); `crates/via-routes/src/` (move the fake to `fake/`); `crates/via-core/src/**`; `crates/via-cli/src/{server.rs, server/config.rs, client.rs}`; `crates/via-fake-agent` (capability profile and terminal fields); `scripts/check-harness-literals.py`; `Cargo.toml` (drop `agent-client-protocol`); `.repo-context/verification.md` | (1) guard lists §5.1; (2) fake `rate_limit` hint → `failed(rate_limit)`; (3) the AD6 ledger cases; (4) `require cancel:partial`; (5) `StopReason::Other` with `tool_use`, and failed `MaxSteps` kept; (6) AD12 chain; (7) AD4 retained terminal under a stalled queue, and the 20 s-after-acknowledgement case; (8) AD9: the contained fake with an open tool stays `quiescent`, and a settled result never carries `pending`; AD9's per-case table on fake profiles; (9) AD16 four-server resume, with the fake in a persistent-connection test profile; (10) model-only spawn resolution; (11) steer delivered on a native fake profile, plus `no_active_turn`/`turn_mismatch`; (12) generic inheritance and clearing, including queued resumes inheriting from the latest accepted turn; (13) envelope fields of #33 populated; (14) a late denial committed `late:true`; (15) structured output present but invalid → `failed(structured_output_invalid)`, and a requested schema with no output → status kept plus warning `structured_output_missing`; (16) AD7: a version observed at the handshake followed by a protocol failure, overflow, transport loss or force stop still appears in the envelope, and a failure before any handshake reports `null`; (17) AD4 on a persistent fake profile: Host-confirmed server death → `failed(server_lost)`, lost connection to a live server → `unknown`; (18) AD18: an effort unknown to the plan → `invalid_params`, and a catalog-only mismatch → `failed(submit_failed)` with `failure.data {reason:"invalid_param", field:"effort"}`, `vendor_code` absent, and no vendor submission; a handshake refusal → `failure.data.reason:"handshake_refused"`; (19) AD13 effective states and `config_switch_unverified` in both directions, including requested `on` with no switch and effective `unknown`; (20) AD4 wall path: without an earlier order on a persistent fake profile, the interrupt is sent before returning, acknowledged → `failed(deadline_wall)` with `cancel.outcome: acknowledged`, unacknowledged → `failed(deadline_wall)` with `cancel {requested, uncertain}`; with an earlier order whose `force_at` passed first, and with one capped at the wall, the order's row; the capped case gives `unknown` with `cancel.outcome: unknown` on the persistent profile and S1's `unknown` with `requested` on the per-turn fake (conflict 15, `stopped`); the per-turn fake keeps every S1 wall assertion; `stop_outcome` with `acknowledged`; (21) AC7: one `config_switch_unverified` warning with `data.categories` listing each affected category | S1 scenario and unit **assertions** preserved as characterization. Tests may migrate to the renamed and generic APIs, but no expected outcome changes except those listed. Literal guard green. Full gate and release-feature check green. |
| S-LAUNCH | **new**, coordinator-owned | `crates/via-cli/src/client.rs`; `crates/via-adapters/src/{config.rs, instance.rs}` (instance version cache and binary identity) | (1) auto-started daemon lacks `PATH`/`HOME`; (2) `harnesses.<name>.binary` and `inherit` parsed and frozen per session, relative paths refused; (3) AD7 refusal cache: keyed by binary identity plus recipe digest, cleared by an identity change or 10-minute expiry, never written for a startup timeout | describe starts nothing (C2 §7 item 1) |
| S-LEFTOVER | **new**, coordinator-owned (OD3), integration stage | `crates/via-host/src/{host.rs, linux.rs, lib.rs}` (scan, `CloseReport.leftovers`), `crates/via-host/src/{anchor.rs, protocol.rs}` (start bound in `Spawned`, option A) and `crates/via-host/tests/`; `crates/via-wire` (`WireCloseReport`); `crates/via-routes/src/` (the shared runtime and each route's close and loss paths); `crates/via-adapters/src/` (`TurnEnd` and driver `CloseReport`); `crates/via-core` (envelope, close result, `session.closed`); `crates/via-store` (`commit_closed`) | AD20's failure-first list | AD20's characterization list; the maximum report in `envelope_at_maximum`; opt-in live checks (K4 models) with controlled survivors: Claude after interrupt lists only the double fork (LH E1); a Codex C1 close (unsubscribe) and idle retirement give `null`; a supervised Codex server loss with turns in flight gives one non-null shared report, which may be empty (sandboxed, LH E2c) or `incomplete`; an OpenCode dispose lists setsid and double-fork survivors (LH E1, E2i) |
| Replay mode | inside S-CORE (test-only crate) | `crates/via-fake-agent` | a replay fixture with an expected-line mismatch fails the run | §8 item 3 |
| Claude fixtures | `via-p98.3.1` (can start now) | `crates/via-adapters/tests/fixtures/claude/`, `conformance_claude.rs` | red by construction | sanitized from CC runs: c0_isolated, c0_bad_model, c0_bad_effort, c0_invalid_resume, c1a–c, c3_queue, c4, c5, c7, c9a/b, c10, c11b |
| Claude adapter | `via-p98.3.2` | `crates/via-adapters/src/claude/`, `crates/via-routes/src/claude/` | p98.3.1 suite plus: synthetic message → no progress; c0_bad_effort → `invalid_params`; c10; c7 cleanup via `GroupAbsent`; AD19 cancel order (interrupt, EOF, S1's close); init mismatch → `resume_mismatch`; c11b one decline and zero denials; `result.usage` aggregate | fixtures green; `require steer` refused |
| Claude qualification, then review and live | `via-p98.3.4` → `via-p98.3.3` | `scripts/qualify/claude.py` (from `run_cases.py`) | — | recipe continuity; §5.4.1 switches qualified or declared; tool processes carry the marker (AD20, option A); Haiku live check records the `checked` version |
| Codex | `via-5lr.3.1` → `.3.2` → `.3.4` → `.3.3` | `crates/via-adapters/src/codex/`, `crates/via-routes/src/codex/`; shared-server lease and AR6 non-turn owner in `crates/via-host`, `crates/via-wire`, `crates/via-store` (coordinator-serialized) | commentary excluded; keyless usage sum 20522+20613; c3 (interrupt only) settles `uncertain` at the P7 window; AD18 effort checked against `model/list` before `turn/start`; wall expiry writes `turn/interrupt` before return; c8 `auth`; c7 `vendor_error`; stale steer `turn_mismatch`; `--disable hooks`; 38 s `initialize`; `userAgent` version parse; AD16 lease reuse | `gpt-6-luna` live check; MCP switch qualified or declared; tool processes carry the marker, sandboxed and full access (AD20, option A); bounds per `via-5lr.3.4` |
| OpenCode | `via-4sw.3.1` → `.3.2` → `.3.4` → `.3.3` | `crates/via-adapters/src/opencode/`, `crates/via-routes/src/opencode/`; HTTP/SSE in `crates/via-wire`; HTTP/SSE replay in `crates/via-fake-agent`; AR6 session owner in `crates/via-store` and `crates/via-host` | VO1 shapes (a)–(d), terminal recognition independent of tool parts; acknowledgement on abort error plus idle with the final assistant and tool updates delayed past `force_at` → `Interrupted`; cancel racing a natural completion or failure keeps the natural terminal; wall expiry sends `POST /abort` before return; cleanup reads bounded by `ack + 60 s`, not `close_by`; server close sends `POST /instance/dispose` then S1's hard stop; t18 `auth`; t19 `rate_limit`; t16 `uncertain`; `output_schema` refused; `format` omitted on plain turns; `variant` checked; idle-retirement slot release | free-model live check; §5.4.1 rows qualified; tool processes carry the marker (AD20, option A) |

## 8. Verification

1. **Default gate (every change).**
   - `cargo fmt --all --check`
   - clippy with `-D warnings`
   - `cargo nextest run --locked --workspace`
   - `cargo deny check`
   - `python3 scripts/check-layers.py`
   - `python3 scripts/check-harness-literals.py`
   - the existing test-failpoints and release-feature commands

   No live vendor runs.
2. **C2 conformance kit.** `crates/via-adapters/tests/conformance.rs`
   parameterizes C2 §7 (as amended) by adapter.
   - It runs on `Adapter::Fake` scenario profiles: native, partial and
     unsupported verbs; persistent connection; steer; denials and declines;
     each terminal status and class; usage keys and aggregates; P7 window;
     late items.
   - The daemon scenario tests (`via-cli/tests/s1_*`) remain the Core
     characterization suite.
3. **Recorded-vendor replay (default gate).** A new bounded, test-only mode of
   `via-fake-agent`.
   - **Selection.** When started as `<dir>/<name>` with
     `<dir>/<name>.replay.json` beside it (resolved from `argv[0]`), it replays
     that fixture. The adapter under test reaches it through
     `harnesses.<name>.binary`, so adapters need no fixture environment.
   - **Fixture content:** an exact argv check; `--version` output;
     expected-input steps (JSON subset match against Claude user lines, control
     replies, or Codex requests with ID capture for response pairing); emit
     steps (verbatim lines with captured IDs substituted); timed and
     signal-gated steps for interrupt races.
   - **Bounds.** 1 MiB lines, 10,000 steps and a whole-run deadline. Any
     mismatch exits non-zero with the step number.
   - **Existing fake protocol.** The start-request mode is unchanged.
   - **OpenCode.** It needs the HTTP/SSE variant from `via-4sw.3.1`.
   - **Provenance.** Each fixture names its source run (for example `CC c7`),
     and each `x.3.1` bead checks sanitization.
4. **Maintainer live check (opt-in; never in the gate or on users' accounts).**
   - One script per harness, `scripts/qualify/<harness>.py`, seeded from the
     re-probe scripts and run through the real `via` binary under a budget cap.
     Evidence goes under `scratchpad/`.
   - Models per K4: Haiku, `gpt-6-luna` low or medium, and a free OpenCode
     model.
   - Cases: the §2 rows fixtures cannot prove (acceptance, interrupt and
     cleanup, resume continuity, private-profile auth failure where possible
     without copying credentials, §5.4.1 switches, usage).
   - A pass adds the vendor version to the adapter's `checked` set in the next
     release. Infrastructure, auth or quota failure is a blocker, never a pass.
   - Each case ends `pass`, `fail`, `blocked` or `not_observable`, and only
     `pass` counts. `not_observable` means VIA records no evidence for the
     case, such as Claude's init MCP inventory. The run passes when every
     case passes and every daemon it started has stopped. Claude's runner
     (via-kr9, 2026-10-06) takes `--via`, `--evidence` (a new directory
     under `scratchpad/`), `--model` (default `haiku`), `--budget-usd` and
     `--claude`. It writes `summary.json` and exits non-zero unless the run
     passes (`docs/specs/vendors/claude-code.md` §4 has its first result).
   - It runs before each adapter slice merges and when a vendor ships a new
     version.
5. **Limits.** The gate proves:
   - Core carries no harness literals;
   - Core's generic handling of the C2 surface is correct;
   - each adapter maps its recorded traffic.

   It does not prove that a new vendor release behaves the same. AD7's
   per-launch handshake checks and item 4 cover only what AD7 lists.
