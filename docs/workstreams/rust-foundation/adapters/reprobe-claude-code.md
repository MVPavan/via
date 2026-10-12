# Claude Code live re-probe report (via-p98.4)

Worker: implementer (Opus 5.5 medium), 2026-09-30. Status: DONE_WITH_CONCERNS.
Saved by the coordinator from the worker's hand-back text (the harness rejected the worker's own Write).
Committed copy with absolute paths removed; raw evidence and scripts stay local in the gitignored `scratchpad/execution/adapter-reprobe/claude-code/`.
Evidence there: `run_cases.py`, `show.py`, `dump.py`, `dump_sept.py`, `shape_diff.py`, `help/`, `runs/`.

## Summary
- **Installed version:** 2.1.285. The packet pins 2.1.283.
- **Budget:** 19 runs reached the API; the cap was 25. One of the 19 was a 404 bad-model rejection. 3 other runs failed before any API call.
- **Clean-up:** every run ended without timeout, with an empty process-group snapshot and no leftover descendants. Nothing is running.
- **Model:** Haiku only (`--model haiku`, which resolves to `claude-haiku-4-5-20251001`).
- **Safety:** no credentials were read or copied, no tracked files were edited, no `bd`, no commit.

**Three most important drifts:**
1. **The version moved out of the tested set.** The binary auto-updated from 2.1.283 to 2.1.285 on 2026-09-29, with no action from you. Under A2 it is now `untested`, so bound-bearing spawn/resume is refused unless `allow_untested` is set. No protocol break was observed.
2. **One new field.** `usage.fallback_credit` (always null so far) now appears in `result.usage` and `assistant.message.usage`. Additive.
3. **Cosmetic help changes.** `--help` gained `--desktop`, and the `--model` help lost its example model name. Across the 14 cases repeated from September, no other key, message type, subtype, capability or terminal value changed.

**Does it still fit C2 as written?** Yes for the operations and wire shapes; everything C2 maps behaves as in September. It does not fit the A2 exact-version gate, and there are the concrete pressure points listed in §4.

## Full report

### 1. Version and shape
- **Version:** init reports `claude_code_version:"2.1.285"` in every run. The 2.1.283 binary is still on disk at `~/.local/share/claude/versions/2.1.283` (2.1.282 and 2.1.284 are there too), so both help outputs could be diffed without a model call.
- **Process shape:** unchanged. One `claude -p` process per VIA turn, and the vendor UUID persists across processes. Both three-process and two-process chains (`c1a→c1b→c1c`, `c9a→c9b`) kept the same UUID.
- **Transport:** stdio carrying NDJSON stream-json in both directions; stderr is free text.
- **Init capabilities:** the same five as September: `interrupt_receipt_v1`, `interrupt_cancel_queued_v1`, `msg_lifecycle_v1`, `mcp_read_resource_v1`, `mcp_tool_ui_meta_v1`.
- **Hidden flags:** `--max-turns` and `--append-system-prompt-file` are missing from `--help` in both versions. `--max-turns` works live. `--append-system-prompt-file` was not exercised.

### 2. C2 mapping (September → today)

**Unchanged from September:**
- **describe / version gate:** `--version` and init `claude_code_version` still carry the version. Only the value changed.
- **open_session:** with `--session-id UUID`, the init and result `session_id` match in all 18 runs that produced an init.
- **StartTurn:** the single `{"type":"user","message":{"role":"user","content":[{"type":"text","text":...}]}}` line is accepted.
- **resume:**
  - `--resume UUID` keeps the same UUID and history: `c1b` recalled the nonce from `c1a`, and `c9b` read the file `c9a` created.
  - A missing session gives a pre-init result with `errors:["No conversation found with session ID: …"]`, `error_during_execution`, `num_turns:0` and exit 1. That result echoes the requested UUID.
- **Steer:** a second user line sent during `sleep 3` merged into one result (`FIRST_DONE\n\nSECOND_DONE`, `queued_turn_count:0`). Steer stays unsupported.
- **Interrupt** (`c7`):
  - The nested receipt `{"type":"control_response","response":{"subtype":"success","request_id":…,"response":{"still_queued":[]}}}` arrived 216 ms after the tool_use.
  - Then came the result `error_during_execution` / `aborted_tools`, with the same `[ede_diagnostic]` error string as in September, and exit 1.
- **Close:** closing stdin after the terminal gives exit 0.
  - New fact: closing stdin right after the prompt (`c10_early_eof`) does not abort the turn. It completes with `success` and exit 0.
- **Identity and acceptance:** the order is init, then assistant (thinking/text/tool_use), then result. No `msg_lifecycle` receipt message appeared in any run.
- **Progress:** the same message types as September: assistant blocks, user `tool_result`, `system/thinking_tokens`, `task_started`, `task_notification{status}` and `rate_limit_event`.
- **Final text, structured output and schema:** `result.result` carries the final text.
  - `structured_output` still comes with `stop_reason:"tool_use"`.
  - Replacing the schema (alpha then beta) and clearing it by omission both worked on the same UUID.
- **max_steps:** `--max-turns 1` ends with `error_max_turns`, `num_turns:2`, "Reached maximum number of turns (1)". A resumed process with a limit of 2 succeeds.
- **Usage:** output tokens are per turn (156 / 167 / 52) while `total_cost_usd` is cumulative (0.0101 → 0.0208 → 0.0299). The only change is the extra `fallback_credit` field.
- **Never-ask:** with `dontAsk` plus `--permission-prompts none`, `system/permission_denied{…,decision_reason_type:"mode"}` arrives 2 ms after the tool_use. The file is not created and the denial is listed in `permission_denials`.
- **Environment allow-list (B7):** `HOME`, `PATH` and `LANG` are still enough.
- **Strict Bash sandbox:** still fails closed before init because `socat` is missing.
- **Read-only tool surface:** `--restricted --tools Read,Glob,Grep` exposes only those three tools.
- **Auth failure** (fresh HOME):
  - A matching init arrives first, then a synthetic assistant message with `model:"<synthetic>"`, `error:"authentication_failed"` and `is_api_error_message:true`.
  - Then a result with `subtype:"success"`, `is_error:true`, `terminal_reason:"api_error"`, and exit 1.
  - This is the same as September.

**New evidence (not tested in September):**
- **Bad model:**
  - Init echoes the requested `claude-nonexistent-model-9` without checking it.
  - A synthetic assistant message follows with `error:"model_not_found"`.
  - The result is `subtype:"success"`, `is_error:true`, `terminal_reason:"api_error"`, `api_error_status:404`, exit 1.
- **Bad effort:** `--effort bogus` only prints a stderr warning ("Unknown --effort value 'bogus' — ignoring it and using the default effort."). The turn succeeds with exit 0.
- **Valid effort:** `--effort low` on Haiku succeeds, but nothing in the stream shows it was applied. `per_turn_effort_active` is false with or without the flag.
- **Decline path, finally exercised (`c11b`):** the flags were `--permission-mode default --permission-prompts host --permission-prompt-tool stdio`.
  - The CLI sent `control_request{request_id, request:{subtype:"can_use_tool", tool_name, display_name, input, description, permission_suggestions, tool_use_id}}` 6 ms after the tool_use.
  - We answered with the packet's proposed reply: `{"type":"control_response","response":{"subtype":"error","request_id":ID,"error":"VIA declines unsupported control request"}}`. The CLI accepted it.
  - The tool did not run: tool_result `is_error:true` with `tool_result_meta:[{non_execution_kind:"permission-rule"}]`. The denial is listed in `permission_denials`.
  - **No** `system/permission_denied` event was sent in this path. The turn completed normally.
- **`--permission-prompts host` without `--permission-prompt-tool stdio`** (`c11`): no control request arrives. The tool is denied directly with a `system/permission_denied` event that has no `decision_reason_type`.
- **The packet's exact §4 full-bound recipe** (`c9a`/`c9b`), plus `--append-system-prompt`:
  - Bash wrote `made.txt`.
  - A resumed process read it back.
  - The instruction marker was honoured on both turns, and the UUID stayed the same.

**Not probed:** recover (there is no mechanism to probe), uncertain submission and transport loss. The `c6_file_tools`, `c6_symlink`, `c6_add_dir` and `c8_read` cases were not re-run.

### 3. Drift and packet corrections
- **D1 (breaking for the gate):** 2.1.285 is outside `{2.1.283}`.
- **D2 (additive):** `usage.fallback_credit`.
- **D3 (cosmetic):** `--desktop` added; `--model` help example removed.
- **D4:** the 14 repeated cases show no other difference (`shape_diff.py`, COMMON section).

Packet assumptions that today's runs contradict or refine:
- **P1 (§6):** the decline encoding is now live-verified. In that path the live `permission_denied` event is absent.
- **P2 (§4):** the packet says an unknown effort is rejected. The vendor actually accepts and ignores it, warning only on stderr, which the packet says is never read. VIA has to validate effort itself.
- **P3:** init `model` is not a resolved identity for full model names; it is only resolved for aliases.
- **P4 (§5):** auth and model errors come as `subtype:"success"` with `is_error:true`. Classification must use `is_error`, `terminal_reason`, `api_error_status` and the synthetic `error` code, never `subtype`.

### 4. Interface pressure
1. **Exact-version gate against silent auto-update** (inference): A2 as written will refuse Claude after almost every update.
2. **Synthetic assistant messages on API errors:** after a matching init, the synthetic `is_api_error_message:true` message would count as "acceptance" under the current rule. C2 has no concept of a vendor-synthetic assistant message.
3. **Two denial shapes:** C2 `action.denied` must be derivable from the terminal `permission_denials` alone, because the live event is not guaranteed.
4. **The A6 decline path is never exercised on the `none` recipe:** only `--permission-prompt-tool stdio` produces an incoming request to decline.
5. **Tool children leave Claude's process group** (same in 2.1.283 and 2.1.285):
   - In `c7`, the Bash tool's `bash` runs as its own session leader, and `sleep` is its own group leader (pid == pgid).
   - A pgid-absence check on Claude's group does not cover them; the runner only found them by walking the parent-pid tree.
   - Inference: the runtime §5.2 group-absence proof is not enough to claim `Quiescent` on this route.
6. **Effort is unobservable,** and the vendor's only warning goes to stderr, which the packet says is never read.
7. **Graceful close means "finish the turn":** stdin EOF does not cancel, and once stdin is closed an interrupt can no longer be sent.

### 5. Commands
`run_cases.py` is adapted from the September runner. Other helpers are `show.py`, `dump.py`, `dump_sept.py` and `shape_diff.py`.
- **Base argv:** `claude -p --input-format stream-json --output-format stream-json --verbose --model haiku --permission-mode dontAsk --permission-prompts none --strict-mcp-config (--session-id|--resume) UUID <case flags>`
- **Per-case flags:** in the `case_info` table.
- **Environment:** an explicit map (`PATH`, `HOME`, `LANG`, `USER`, `LOGNAME`, `XDG_RUNTIME_DIR`); only `HOME`, `PATH` and `LANG` for the `c8_*` cases.
- **Cap:** 70 s per run.
- **Order run:** `python3 run_cases.py <case>` for c0_oauth, c0_isolated, c0_invalid_resume, c0_bad_effort, c0_bad_model, c1a, c1b, c1c, c3a, c3b, c3_queue, c4_never_ask, c5_read_only, c7_interrupt, c8_minimal, c12_effort, c6_sandbox, c9a, c9b, c10_early_eof, c11_host_prompt, c11b_stdio_prompt.
- **Help diff:** `claude --help` against `~/.local/share/claude/versions/2.1.283 --help`, saved in `help/`.

### 6. Decisions for you
1. **Version policy:** add 2.1.285 to the tested set on this evidence, switch to a tested range, or pin a versioned executable path instead of the auto-updating symlink.
2. **Acceptance rule:** decide whether C2 acceptance excludes `is_api_error_message:true` messages. If it doesn't, auth and bad-model failures count as accepted turns.
3. **Never-ask route:** stay on `--permission-prompts none`, or move to `host` + `--permission-prompt-tool stdio` with the now-verified decline reply.
4. **Tool-descendant cleanup:** decide whether Host must track descendants or use a cgroup for Claude's Bash tool before claiming `Quiescent`.

Files changed: new files under `scratchpad/execution/adapter-reprobe/claude-code/` only (`run_cases.py`, `show.py`, `dump.py`, `dump_sept.py`, `shape_diff.py`, `help/`, `runs/`). Commit: none.

Side effect, as in September's recipe: `c1*`, `c3a/b` and `c9a/b` used Claude's normal session persistence. That created new session transcripts in your `~/.claude/projects`, keyed to the disposable cwds under my folder. Nothing existing was changed or deleted.
