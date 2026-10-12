# Tool-process lifecycle per harness (via-jm4.22.1)

Worker: implementer (Opus 5.5 medium), 2026-09-30. Saved by the coordinator from the hand-back text.
Committed copy with absolute paths removed; raw evidence and scripts stay local in the gitignored `scratchpad/execution/tool-lifecycle/harnesses/`.

Status: DONE_WITH_CONCERNS

# Tool-process lifecycle per harness (via-jm4.22.1): findings

The agent process does not reliably own its tool processes. Only Codex, and only when its bwrap sandbox is on, cleans up after a crash. Once the agent is gone, whatever survives keeps running as a live orphan, still writing files. I observed no vendor zombies in any run.

Versions: Claude Code 2.1.285, codex-cli 0.159.2, OpenCode 1.18.32. Runs were 2026-09-30, 18:30–18:59 UTC, on WSL2 (PID 1 is systemd; orphans went to the WSL `Relay(1698398)` subreaper).

## Workload

Each run had the model start one tool call: `bash <marker-dir>/work.sh`. It launches five bounded heartbeat loops (90 × 1 s) that each append a timestamp to their own file:

- `hb_main`: foreground; keeps the tool call running.
- `hb_bg`: `&`.
- `hb_nohup`: `nohup … &`, same process group.
- `hb_setsid`: `setsid … &`, new session and group.
- `hb_dfork`: `( setsid … & )`, a double fork. The intermediate shell exits at once, so this one is already orphaned (parent = Relay) while the agent is still alive. I added it after the first Claude runs to tell a parent-tree kill apart from a group or session kill.

Every process carries `VIATL_MARKER`. The event fired once `hb_main` had at least 3 lines. I took snapshots at 50 ms intervals up to +1 s, then at +1, +5 and +30 s (longer for some graceful events). After that I SIGKILLed everything still alive.

"Survived" below means the process was alive and its heartbeat file still growing at the last snapshot, and it stopped only when my cleanup killed it. The loops would otherwise have run to their 90 s bound.

## 1. Results per harness × event

### Claude Code (`claude -p` stream-json, Haiku)

The tool shell is its own session leader (it holds a private pgid and sid). Its stdin is `/dev/null` and its stdout/stderr go to a file, `<session-scratch>`, not to a pipe.

| Event | What survived | How long | New parent | Zombie? | Heartbeat evidence | Wire |
|---|---|---|---|---|---|---|
| E1 `control_request` interrupt | Only `hb_dfork` | ≥45 s, until my cleanup | Relay (it was already there before the event) | No | Everything else was dead within 50 ms; its last write was 0.1–0.2 s before the event. `hb_dfork` wrote +44.5 s. | `control_response success {still_queued:[]}`, then a `tool_result is_error` "user doesn't want to proceed…", then "[Request interrupted by user for tool use]", then `result error_during_execution`. Claude stays alive. |
| E2 stdin EOF mid-tool | Everything | ≥90 s | Unchanged (still under claude) | No | All files still growing at +90 s | Nothing new except `tool_progress` heartbeats. Claude kept running the tool. |
| E3 SIGTERM to claude | Only `hb_dfork` | ≥30 s | Relay | No | Everything else dead within 50 ms | `tool_result "Exit code 137"`, then claude exits 143 with no `result` message |
| E4 SIGKILL to claude | Everything: tool shell, work.sh, main, bg, nohup, setsid | ≥30 s | Tool shell → Relay; the rest keep their bash parents | No | All still growing at +30 s | stdout EOF, exit −9 |
| E5 SIGKILL to claude's group | Same as E4 | ≥30 s | Relay | No | All still growing | Exit −9. Claude's group contains only claude, because the tool is in another session. |
| E6 SIGINT | Nothing (this run had no dfork) | – | – | – | Dead within 50 ms | Same as E1, then exit 0 |

E2, E4, E5 and E6 ran before I added `hb_dfork`. E1 and E3 were run twice, once without and once with it.

### Codex app-server (stdio, `gpt-6-luna` confirmed in `model/list`, effort low, `workspace-write`, `--disable hooks`, private `CODEX_SQLITE_HOME`)

Process tree during the tool call:

```
app-server
 ├─ codex-code-mode-host
 └─ codex-linux-sandbox        (own session)
     └─ bwrap                  (own group)
         └─ codex-linux-sandbox  = PID 1 of a new pid namespace
             └─ bash work.sh …
```

The bwrap argv contains `--as-pid-1 --new-session --die-with-parent … --unshare-user --unshare-pid --unshare-ipc --unshare-net --proc`. `hb_dfork` got reparented to the namespace PID 1, not to Relay.

| Event | What survived | How long | New parent | Zombie? | Heartbeat evidence | Wire |
|---|---|---|---|---|---|---|
| E1 `turn/interrupt` | Everything | ≥30 s | Unchanged | No | All growing | `{}`, then `thread/status idle`, then `turn/completed status:"interrupted" items:[]`. The `commandExecution` item never completes. |
| E1c: interrupt, then stdin close 5 s later | Nothing | – | – | No | Dead within 50–150 ms of the close | app-server exits 0 |
| E1t: interrupt, then `thread/backgroundTerminals/clean` 3 s later (experimental; needs `capabilities.experimentalApi`) | Nothing except code-mode-host, which is per server | – | – | No | Dead within 50 ms | Response `{}`, then `item/completed commandExecution status:"failed" exitCode:-1`. The server stays up. |
| E2u `thread/unsubscribe` | Everything | ≥30 s | Unchanged | No | All growing | `{status:"unsubscribed"}`, then silence |
| E2c app-server stdin close | Nothing | – | – | No | Dead within 50 ms | stderr `exec_command failed: UnknownProcessId`; exit 0 |
| E3 SIGTERM | Nothing | – | – | No | Dead within 50 ms | Exit 0 |
| E4 SIGKILL (sandboxed) | Nothing | – | – | No | Dead within 50 ms | Exit −9 |
| E5 SIGKILL to the group | Nothing | – | – | No | Dead within 50 ms | Exit −9 |
| E6 SIGINT | Nothing | – | – | No | Dead within 50 ms | Exit −2 |
| E4 SIGKILL with `danger-full-access` (no bwrap) | All 5 heartbeat loops (main, bg, nohup, setsid, dfork) | ≥30 s | Relay | No | All growing | Exit −9. Only the direct tool shell (work.sh) died; the loops were left orphaned. |

### OpenCode serve (full private HOME/XDG/DB/password recipe, `opencode/mimo-v2.6-flash-free`)

The bash tool runs `work.sh` as the leader of its own session/group, and its stdout/stderr are sockets to the server. `/doc` offers `POST /instance/dispose` and `POST /global/dispose`, plus v2 `POST /api/session/{id}/interrupt`, which I did not test.

| Event | What survived | How long | New parent | Zombie? | Heartbeat evidence | Wire |
|---|---|---|---|---|---|---|
| E1 `POST /session/{id}/abort` | `hb_setsid` and `hb_dfork` | ≥30 s | Relay | No | main, bg and nohup dead within 20 ms | 200 `true`; SSE `session.error`, `session.status`, `session.idle`; the tool part is `completed` with "User aborted the command"; the message error is `MessageAbortedError` |
| E2i `POST /instance/dispose` | setsid, dfork | ≥30 s | Relay | No | The rest dead within 50 ms | 200 `true`; `MessageAbortedError`; the server stays up; the SSE stream closes |
| E2g `POST /global/dispose` | setsid, dfork | ≥30 s | Relay | No | The rest dead within 50 ms | Same as E2i |
| E3 SIGTERM | Everything (work.sh → Relay) | ≥30 s | Relay | No | All growing | The server died from the signal itself (wait status 15): it has no handler |
| E4 SIGKILL | Everything | ≥30 s | Relay | No | All growing | – |
| E5 SIGKILL to the group | Everything | ≥30 s | Relay | No | All growing | – |
| E6 SIGINT | Everything | ≥30 s | Relay | No | All growing | Killed by the signal (status 2) |

Zombies: none from any vendor in any run. Twice a snapshot showed `Z [claude]`, but that was my own Python harness not yet having reaped the agent it had just killed. That is a reminder that VIA must reap its own agent child. After each cleanup, zero processes remained, zombie or live, so Relay reaps the orphans promptly.

## 2. Are tool processes independent of the agent, and what is the vendor mechanism?

### Claude Code: independent at the OS level

Claude Code is closed source, so this is behaviour only. The Bash tool is a new session with its output in a file, so nothing in the kernel ties it to claude.

Claude does clean up on its own graceful paths (interrupt, SIGTERM, SIGINT), and the evidence says it walks the parent-pid tree:

- It killed the `setsid` grandchild, which was in another session and another group.
- It missed the double-forked one, which was no longer its descendant.
- The tool result was "Exit code 137", which suggests SIGKILL (inference).

On SIGKILL there is no cleanup at all. stdin EOF does not stop an in-flight tool. The `--help` text says nothing about any of this.

### Codex: dependent while sandboxed

The chain, from the source at tag `rust-v0.159.2` (commit ff6aec9):

- **PDEATHSIG on the first sandbox process.** `core/src/spawn.rs:96-141` and `utils/pty/src/child_command.rs:305-316` run `setsid` (`process_group.rs:51` `detach_from_tty`) and `set_parent_death_signal`. That is `PR_SET_PDEATHSIG` with SIGTERM plus a ppid re-check (`utils/pty/src/process_group.rs:29-41`). It is followed by `kill_on_drop(true)`.
- **bwrap dies with it.** `linux-sandbox/src/linux_run_main.rs:595-612` forks bwrap into its own group and calls `terminate_with_parent`, i.e. `PR_SET_PDEATHSIG` SIGTERM (`:865-875`). `bwrap.rs:355-377` adds `--new-session --die-with-parent --unshare-pid`, and `launcher.rs:37-45` inserts `--as-pid-1`.
- **The pid namespace takes everything else.** The inner `codex-linux-sandbox` is PID 1 of the namespace. When it dies, the kernel kills every process in the namespace, including setsid and double-forked ones.
- **Graceful paths.** stdin close and SIGTERM go through session shutdown, `shutdown_session_runtime` → `unified_exec_manager.terminate_all_processes()` (`core/src/session/handlers.rs:287-310`, `unified_exec/process_manager.rs:1796`). `terminate()` SIGKILLs the process group (`utils/pty/src/process_group.rs:269-272`, `pty.rs:93-107`). The stdio server exits when stdin closes (`app-server/src/lib.rs:775,1165`), and SIGTERM is handled in `app-server-transport/src/transport/stdio.rs` (signal-hook).
- **Interrupt deliberately leaves processes running.** `Op::Interrupt` only calls `interrupt_task` (`handlers.rs:57`). "Background terminals" persist across turns by design. `Op::CleanBackgroundTerminals` → `close_unified_exec_processes` (`handlers.rs:61,445`; `tasks/mod.rs:902`) is the per-thread kill, and `thread/unsubscribe` only removes the subscription (`thread_processor.rs:1020-1046`).
- **Code-mode host.** It is spawned with a stdin pipe and `kill_on_drop` (`code-mode/src/remote_session/connection.rs:158-167`). That it exits on stdin EOF is my inference.

Without bwrap (`danger-full-access`), PDEATHSIG covers only the direct tool shell, and the live E4 run showed all grandchildren surviving. It also follows from the code that a graceful group kill would miss setsid and double-fork grandchildren there (inference; I had no turns left to test it).

### OpenCode: independent

From the source at v1.18.32 (commit 545f51d):

- The bash tool spawns with `detached: true` (`packages/opencode/src/tool/shell.ts:293-309`), which is `setsid`.
- Abort, timeout and scope release call `handle.kill({forceKillAfter:"3 seconds"})` (`shell.ts:537-555`). That is `process.kill(-pid, SIGTERM)` on the tool's own group, then SIGKILL (`packages/core/src/cross-spawn-spawner.ts:292-311, 373-398, 427-435`). Dispose closes the instance scope, which leads to the same group kill (inference, but it matches what I observed).
- `serve` installs no SIGTERM/SIGINT handler (`src/cli/cmd/serve.ts`: `Effect.never`). Grepping `src` found no `prctl` and no parent-death mechanism.
- The group kill cannot reach setsid or double-forked descendants.

## 3. A graceful message that reliably stops every tool process

- **Codex (sandboxed):** yes.
  - `turn/interrupt` followed by `thread/backgroundTerminals/clean {threadId}` stops everything for that thread and keeps the shared server up. It is experimental and needs `initialize.capabilities.experimentalApi:true`.
  - Closing app-server stdin, or SIGTERM, also stops everything, but for every thread on that server.
  - `turn/interrupt` alone and `thread/unsubscribe` do not.
- **Claude Code:** the `interrupt` control_request (or SIGTERM/SIGINT) stops everything still in its parent tree. It misses processes that re-parented away (double fork or daemonizing). stdin EOF does not stop tools.
- **OpenCode:** none reaches everything. `/session/{id}/abort` and `/instance/dispose` / `/global/dispose` kill the tool's process group only, so `setsid` and double-forked descendants survive. SIGTERM and SIGINT kill nothing.

## 4. Plain answers to the owner's questions

- **Are tool calls independent of the agent process?**
  - Claude and OpenCode: yes, fully at the OS level. Only the agent's own graceful cleanup code ties them together, and it is partial.
  - Codex: no while its bwrap sandbox is on, because the kernel enforces the link (PDEATHSIG plus the pid namespace). Without the sandbox it is mostly independent.
- **What happens if the agent crashes (SIGKILL)?**
  - Claude: every tool process keeps running.
  - OpenCode: every tool process keeps running. OpenCode also dies silently on a plain SIGTERM or SIGINT with the same result.
  - Codex (sandboxed): everything dies within 50 ms.
  - Codex full-access: every background grandchild keeps running.
- **Zombies or running orphans?** Running orphans. They keep writing files and are re-parented to the nearest subreaper (WSL Relay here; on native Linux this would be a subreaper such as systemd --user, or PID 1), which reaps them when they exit. I saw no zombies.
  - Escaped Claude tools write to a file, so they do not die of SIGPIPE.
  - Escaped OpenCode tools write to a socket to the dead server, so ones that print to stdout would likely get EPIPE/SIGPIPE on their next write. Silent ones do not (inference).

## 5. Evidence, turns and cleanup

Evidence (gitignored):

- Per run, under `scratchpad/execution/tool-lifecycle/harnesses/<claude-code|codex|opencode>/runs/<event>-<stamp>/`:
  - `log.jsonl`: full trees with pid, ppid, pgid, sid, state, argv, fds and parent, per snapshot; fast liveness; heartbeat stats; wire events after the event; cleanup.
  - `wire.jsonl` for Claude and Codex.
  - `sse_after_event.txt` and `messages.json` for OpenCode.
  - Copies of the `hb_*` files.
- Across all runs: `harnesses/summary.txt` (generated by `harnesses/summarize.py`).
- Tooling: `harnesses/lc.py`, `harnesses/*/run.py`. The OpenCode driver imports `adapter-reprobe/opencode/oc.py` and `turn.py`.
- Codex `model/list` output: `harnesses/codex/runs/models/model_list.json`.
- Upstream source clones used for the citations: `<session-scratch>`.

Turns used:

- Claude: 8 (E1 ×2, E2, E3 ×2, E4, E5, E6).
- Codex: 10, the cap (E1, E1c, E1t, E2u, E2c, E3, E4, E4 full-access, E5, E6). `model/list` needed no turn.
- OpenCode: 7 (E1, E2i, E2g, E3, E4, E5, E6).

Cleanup: every run SIGKILLed its remaining marked processes and the agent group, then verified none were left. A final scan found 0 processes with `VIATL_MARKER` and 0 of 193 heartbeat files growing; the newest was written 63 s before the check. No `opencode serve`, app-server or `claude -p` process I started remains. The OpenCode `private/` directories, which hold the password, were deleted. I sent nothing to the owner's servers. I made no tracked edits, no commits, no bd, no push.

Concerns:

1. **All of this is WSL-specific.** The Relay is the subreaper; native Linux will re-parent elsewhere, though the survival results do not depend on it.
2. **Claude's cleanup mechanism is inferred from behaviour.** It is closed source; I did not check its public web docs beyond `--help`.
3. **`hb_dfork` coverage is partial.** It was added after Claude's E2/E4/E5/E6 runs; in those runs everything survived or nothing did, so the conclusions hold.
4. **Codex full-access was measured for the crash only.** Its graceful group-kill gap is inferred from source.
5. **OpenCode v2 `/api/session/{id}/interrupt` was not tested.**
6. **Leftover Claude output files.** Claude Code left its task-output files under `<session-scratch>`. I did not delete them.

Files changed: new untracked files under `scratchpad/execution/tool-lifecycle/harnesses/` only.
Commit: none.
Next action needed: coordinator to integrate this with via-jm4.22.2, notably Codex `thread/backgroundTerminals/clean` as the per-thread stop, and the fact that no graceful OpenCode path reaches setsid or double-forked descendants.
