# VIA mechanisms for escaped tool processes (via-jm4.22.2)

Worker: implementer-high (Opus 5.5 high), 2026-09-30. Saved by the coordinator from the hand-back text.
Committed copy with absolute paths removed; raw evidence and scripts stay local in the gitignored `scratchpad/execution/tool-lifecycle/mechanisms/`.

Status: DONE. Research for via-jm4.22.2 only: no tracked edits, no commits, no bd.

Everything was tested on this machine (WSL2, kernel 6.6.87.2, Ubuntu 24.04.3, systemd 255, bwrap 0.9.0, Python 3.12). Where a claim comes from knowledge rather than a test here, it is marked "(not tested here)".

# 1. Results per mechanism

## Setup

The synthetic tree has a daemon, an anchor (group leader, control pipe, TERM handler, like §5.1) and a fake vendor. The vendor starts eight bounded heartbeats (at most 90 s), one per pattern:

| Abbrev | Pattern |
|---|---|
| grp | plain child, in the group |
| nhp | `nohup … &` |
| sid | setsid child (the Claude Bash shape) |
| pgd | own-pgid child (the OpenCode bash shape) |
| dbl | double-forked daemon |
| sbx | `bwrap --new-session`, without die-with-parent |
| cdx | the exact Codex 0.159.2 bwrap shape: `--as-pid-1 --new-session --die-with-parent --unshare-user --unshare-pid` (taken from the reprobe's `process_trees.jsonl`) |
| out | handoff to an outside service: `systemd-run --user` |

Crash cases:
- **a:** SIGKILL the vendor, then Stop.
- **b:** Stop while the vendor is live.
- **c1:** SIGKILL the anchor.
- **c2:** SIGKILL the daemon.
- **c3:** SIGKILL the anchor and the daemon.

The full matrix ran twice (sessions m1 and p1) with identical tables.

Survivors after each mechanism's cleanup (L = alive, . = gone):

| mechanism | case | grp | nhp | sid | pgd | dbl | sbx | cdx | out |
|---|---|---|---|---|---|---|---|---|---|
| status quo | a, b, c2 | . | . | L | L | L | L | . | L |
| status quo | c1, c3 | L | L | L | L | L | L | L | L |
| anchor subreaper + kill loop | a, b, c2 | . | . | . | . | . | . | . | L |
| anchor subreaper + kill loop | c1, c3 | L | L | L | L | L | L | L | L |
| + daemon also subreaper | c1 | . | . | . | . | . | . | . | L |
| + daemon also subreaper | c3 | L | L | L | L | L | L | L | L |
| user + PID ns + mount ns with fresh /proc | a, b, c1, c2, c3 | . | . | . | . | . | . | . | (see note) |
| cgroup scope (`systemd-run --user --scope -p Delegate=yes`) | a, b, c2 | . | . | . | . | . | . | . | L |
| cgroup scope | c1, c3 before recovery | L | L | L | L | L | L | L | L |
| cgroup scope | c1, c3 after name-based `cgroup.kill` | . | . | . | . | . | . | . | L |
| PDEATHSIG on the vendor | a, b, c2 | . | . | L | L | L | L | . | L |
| PDEATHSIG on the vendor | c1, c3 | L | L | L | L | L | L | . | L |

Notes on the table:
- **Case a, all mechanisms:** after the vendor was killed and before Stop, every tool was still alive. Today's anchor waits for Stop or EOF.
- **cdx, all mechanisms:** it dies on its own whenever its spawner dies, because Codex's bwrap already uses die-with-parent and its own PID namespace.
- **out under the PID namespace:** it never started. systemd's private bus fails inside a PID namespace ("Failed to connect to bus: No data available"). That is a side effect, not containment: a tmux server started outside the namespace still received a `sleep` from inside it, and the `sleep` outlived the namespace.

## 1. Status quo (process group only, report `Uncertain`)

- **Results:** kills only grp and nhp, and only while the anchor lives. Orphans reparent to WSL `Relay(1698398)`, which matches the OpenCode reprobe.
- **Privileges and portability:** none needed; works everywhere.
- **Fit:** it is §5.1/§5.2 as written. It is honest only if reported as `Uncertain` on all three R1 vendors.
- **Cost:** zero.

## 2. Anchor as child subreaper

**The race-free kill loop.** The anchor sends SIGKILL only to its own unreaped children, found in `/proc` (`CONFIG_PROC_CHILDREN=y` here) and killed by pid. An unreaped child's pid cannot be reused, so there is no reuse race and no pidfd is needed. The anchor reaps each child; that child's orphans come to the anchor; repeat until the anchor has no children left (ECHILD). In the tests, 1 round killed 7–10 processes.

**Results:**
- Covers a, b and c2 for every descendant pattern.
- c1 and c3 are not covered: once the anchor is dead, orphans go to `Relay`.
- If the daemon is also a subreaper, c1 is covered (3 rounds) but c3 is not.

**Interaction with the §5.1 self-KILL.** The loop must run before the self-group KILL. Once ECHILD holds, no descendant remains for the OS to adopt. ECHILD is an atomic kernel answer, so the anchor gets a positive "no descendants" proof. It does not cover outside-service handoffs.

**Privileges and portability:**
- No privileges.
- Works in Docker with the default seccomp profile (tested with the local `node:22-bookworm` image, no pull: an orphan reparented to the subreaper and was killed and reaped).
- Works on WSL and on any Linux since 3.4.
- macOS has no subreaper.

**Fit:**
- Uses only safe rustix APIs: `set_child_subreaper`, `kill_process`, `waitpid`/`waitid`. The workspace's `unsafe_code = "forbid"` is respected.
- The anchor must reap non-vendor children by specific pid, so it does not steal the tokio-owned vendor status.

**Shared Codex server:** one tree per server. It kills every thread's tools when the server stops or crashes, but cannot attribute a tool to one session.

**Cost:** small, one prctl call and one loop. A daemon-side subreaper would add orphan reaping inside the multithreaded tokio daemon, by specific pids only.

## 3. PID namespace per run

**Design tested:** the anchor calls `unshare(USER|PID|NS)` itself and stays the group leader. Its first child is ns-init (PID 1), with PDEATHSIG=KILL and a pipe check for the fork race. ns-init makes `/` private and mounts `/proc`, then spawns the vendor. Stop means: the anchor sends SIGKILL to ns-init (its own child, so race-free) and then `waitpid`.

**Results:**
- Every pattern, including dbl, sbx and nested cdx, was killed in all five crash cases.
- c1 and c3 are handled by the kernel: anchor death, then ns-init's PDEATHSIG, then the kernel kills the whole namespace.
- **Proof:** `waitpid(ns-init)` and pidfd readability on ns-init come only after the namespace is empty. Test: with a slow-exiting 4 GiB tool, a `/proc` census was empty at 1.6 ms, but the pidfd became readable at 81 ms. None of the 139 samples showed the pidfd readable while a member was alive. So this proof is stronger than a census.

**Constraints found:**
- After `unshare(CLONE_NEWPID)` the anchor cannot create threads ("can't start new thread").
- Once PID 1 exits, further forks fail with ENOMEM. So there is exactly one ns-init per anchor, and the tokio blocking pool must never start.
- rustix 1.1.5 offers only `unsafe fn unshare_unsafe` (the safe `unshare` is deprecated). This needs an `unsafe` exception.
- Without a fresh `/proc`, nested bwrap breaks ("bwrap: open /proc/12/ns/ns failed"), so Codex's sandbox would fail. The mount namespace is required.

**Live harness smoke inside `unshare --user --map-current-user --pid --fork --kill-child --mount-proc`:**
- **Claude Haiku (1 turn, $0.025):** succeeded.
- **Codex gpt-6-luna at low effort (2 turns):** succeeded. The nested bwrap worked: its own PID 1 and uid_map `1000 0 1`.
- **OpenCode mimo-v2.6-flash-free (1 turn):** succeeded.
- Claude and OpenCode each left a setsid `sleep 47.31` alive after the harness exited. Codex's copy died with its own sandbox.
- Killing the outer process emptied the namespace in under 1 ms.

**Effects on the harness environment (tested here):**
- uid stays 1000; files created inside are owned 1000:1000 outside.
- Supplementary groups show as `nogroup`, but access still works (`docker.sock` worked).
- Root-owned files show as `nobody:nogroup`, so setuid-root binaries cannot elevate. This is from the ownership view; sudo was not run.
- `ping` (which relies on a file capability) failed.
- The session D-Bus works. `systemctl --user` and `systemd-run --user` fail inside a PID namespace.
- `/proc` shows only the run.
- WSL interop (`cmd.exe`) still works.

Not tested here:
- Kernel keyrings are per user namespace.
- Pid-bearing shared state files will hold namespace-local pids. For example, Claude writes `~/.claude/sessions/<pid>.json`; it carries a `pidDomain` field.

**Availability:**
- **Docker default:** `unshare` fails with EPERM, both as root and as uid 1000 (tested).
- **Ubuntu 23.10+/24.04 default `kernel.apparmor_restrict_unprivileged_userns=1`** (not tested here: this WSL kernel has no AppArmor). Unconfined binaries get a user namespace with no capabilities, so the PID-namespace step fails unless root installs an AppArmor profile. Hosted CI runners on 24.04 are reportedly affected too (not tested here).
- macOS: none.

**Fit with §5.1/§5.2:** good. The group is unchanged, and the proof improves.

**Cost:**
- Runtime overhead is small: +0.7 ms per launch.
- The vendor environment changes, and there is real complexity: the unsafe exception, the no-threads rule, mount handling.

## 4. cgroup v2 per run

**Delegation here.** Our process tree sits in root-owned `/init.scope`, so there is no direct write access. `user@1000.service` is `Delegate=yes`, and `systemd-run --user --scope -p Delegate=yes` works. It execs in place: the anchor pid equals the daemon's child pid, and the group-leader check passes. Each scope gets user-owned `cgroup.kill` and `cgroup.events`.

**Results:**
- a, b and c2: every descendant pattern was killed.
- c1 and c3: nothing happens until recovery. Recovery writes `cgroup.kill` by the unique scope name; the scope was gone within 10 ms.
- **Extra test:** the daemon ran in its own scope, and the run scope was started with `BindsTo=<daemon>.scope`. SIGKILL of the daemon made systemd stop the run scope and kill a setsid tool within 1 s. So c2 and c3 become automatic.
- `out` survives, because `systemd-run --user` creates a sibling unit.
- A same-uid tool could deliberately move itself to another user-owned cgroup.

**Proof:** `populated 0` in `cgroup.events`, or the unit or cgroup being gone. It is name-based and survives restarts.

**Privileges:** none beyond a systemd user manager with delegation.

**Portability:**
- Works on logind/systemd hosts.
- Not in containers: cgroupfs was mounted read-only in Docker (tested).
- Not on non-systemd distros, not on WSL without systemd, and not on hosts without a user session or lingering.
- Putting the daemon in the user manager's tree changes its logout behaviour (not tested here).
- macOS: none.

**Fit:** the vendor environment does not change. A name-based kill has no reuse race, but §5.1 needs a stated exception so that the daemon can kill by name.

**Cost:**
- `systemd-run` launch: 4.0 ms median, 27 ms max.
- It depends on the external `systemd-run` binary, plus an availability probe and a fallback.

## 5. PR_SET_PDEATHSIG

**Results:** only the process that sets it is affected. The flag is cleared on fork and vendors spawn tools themselves. In c1, the vendor died but grp, nhp, sid, pgd, dbl, sbx and out survived.

**Other limits:**
- Setting it on the vendor needs `pre_exec`, which is unsafe.
- The "parent" is the spawning thread, which is dangerous from the tokio daemon.
- A daemon-to-anchor PDEATHSIG would also contradict §5.1, which requires the anchor to outlive the daemon.

**Valid use:** only the anchor-to-ns-init link, if a PID namespace is ever adopted. Codex already sets it on its own bwrap.

## 6. Graceful vendor exit first, then a backstop

Order:
1. The adapter sends the vendor's interrupt or close.
2. Wait up to `force_after_ms` for the terminal and tool facts.
3. Host Stop: TERM to the anchor's own group and its own children, then up to 2 s grace.
4. Backstop:
   - **kill loop:** repeat SIGKILL on own children and reap, until ECHILD.
   - **cgroup alternative:** `cgroup.kill`.
   - **PID-namespace alternative:** SIGKILL ns-init.
   - KILL no later than the deadline minus 1 s.
5. Proof within 1 s, otherwise `Uncertain`.

Persistent servers:
- A per-session cancel stops at step 2, because the server lives.
- Steps 3–5 run only when the server itself closes or crashes. For OpenCode that means per VIA session; for Codex, on connection close or when the last session closes.
- For crash a there is no graceful step: go straight to steps 3–5.

# 2. Recommendation (simple first)

**Step 1, now: anchor subreaper plus the race-free kill loop.** Also start cleanup automatically when the vendor exits.
- Reasons:
  - It covers the owner's main problem: a (agent crash), b (forced stop) and c2 (daemon crash, via EOF).
  - It works for every descendant pattern the harnesses showed.
  - No privileges, no vendor environment change, works in containers and on WSL, no unsafe code, small diff.
  - It resolves OD3's "separate race-free mechanism" gap without pidfd.
- **Policy call for the owner:** it also kills long-lived services an agent was asked to leave running.

**Step 2, only if the owner wants c1/c3 covered or crashes are observed: per-run systemd user scope** with Delegate=yes, `BindsTo` the daemon's own scope, and a generation-unique name.
- It is auto-detected and falls back to Step 1.
- Why it beats a PID namespace for this: no change to the vendor environment, it works on stock Ubuntu 24.04, and it gives a durable name-based kill and proof.

**PID namespace: not the default.** It is the strongest containment (all five cases, enforced by the kernel), but:
- it breaks setuid/sudo/ping and `systemctl --user` inside runs;
- it fails on Ubuntu 24.04 defaults and in Docker;
- it needs an unsafe exception.

Keep it as a possible later opt-in "contained" mode.

**PDEATHSIG: rejected as a tool mechanism.**

**Crash cases that stay uncovered:**
- **With Step 1 only:**
  - c1 (anchor killed) and c3 (anchor and daemon killed): the anchor's group and all descendants survive, reparented to the next subreaper. Report `Uncertain`.
  - c2: the tools are killed, but the proof reply is lost, so it reports `Uncertain` after restart. Fix this only if needed, with an anchor-written proof marker.
- **With Step 2:** c1 and c3 are covered where systemd user scopes exist, but not in containers, on non-systemd hosts, or on WSL without systemd.

**Residual risks under every mechanism:**
- Work handed to outside services escapes: systemd --user units, an existing tmux server, docker, ssh, `at`/cron, WSL `.exe` interop. Tested for systemd-run and tmux.
- Uninterruptible (D-state) processes can exhaust the deadline and end as `Uncertain`.
- Per-session cancel on the shared Codex server cannot be attributed, and Codex fast denials emit no item (VX7). It stays vendor-facts-only, with P7 pending cleanup.
- macOS has none of these mechanisms (deferred, via-pvj.4).

# 3. Sketch of the rule changes

## Runtime §5.1

1. The anchor sets `PR_SET_CHILD_SUBREAPER` before the vendor spawn. It reaps every non-vendor child by specific pid and never disturbs the vendor handle.
2. Signalling authority becomes "its own current group, plus its own unreaped children by pid". This is race-free because an unreaped pid cannot be reused. No other numeric signal is allowed.
3. Cleanup order:
   - TERM to the own group and own children, then grace;
   - repeat {KILL own children; reap} until `waitid(ALL, NOHANG)` returns ECHILD, or the deadline passes;
   - the self-group KILL comes last.

   Vendor exit starts this cleanup without waiting for Stop or EOF (owner policy).
4. Replace "remaining children are adopted/reaped by the OS" with: "after ECHILD, none remain; externally killed anchors leave descendants to the next subreaper; no replacement kill is guessed."
5. **If Step 2 is adopted:**
   - The anchor starts inside `systemd-run --user --scope -p Delegate=yes -p BindsTo=<daemon scope> --unit=via-run-<generation>`, after probing that it is available.
   - Killing that scope by its generation name (`cgroup.kill` or unit stop) is race-free, and the daemon may do it on anchor death and at recovery.

## Runtime §5.2

6. Add `DescendantsAbsent` evidence: the anchor reports ECHILD after the loop, with generation and time. It covers every process that remained an anchor descendant. It never covers outside-service handoffs.
7. `GroupAbsent` stays as the recovery probe.
8. A lost reply means `Uncertain`.
9. Step 2 adds `RunScopeEmpty`: `populated 0`, or the unique scope is gone.

## design.md AD9

- **Host facts:** `DescendantsAbsent` (and `RunScopeEmpty` with Step 2), next to `GroupAbsent`.
- **Private per-turn routes:** `Quiescent` = `DescendantsAbsent`. The "tools stay in group" test is no longer needed.
- **Persistent servers:**
  - per-session cancel while the server lives: vendor tool facts only (unchanged);
  - server close or crash: `DescendantsAbsent` covers every session on that server.
- **Recovery after restart:** `GroupAbsent` gives `Quiescent` only on routes whose tools stay in the group (the fake). With Step 2, `RunScopeEmpty` gives `Quiescent`. Otherwise `Uncertain`.
- **Always:** "Processes handed to services outside VIA's process tree are outside cleanup; no claim covers them."

## design.md AR2

Replace the wording with:

> "All three R1 vendors run tools in their own groups or sessions; group absence proves only the group; anchor descendant absence covers tools that stay descendants; outside-service handoffs are never covered."

OD3 resolves to option (a), plus optional (b) as systemd scopes. Its "needs a separate race-free mechanism" note is answered by rule 2.

# 4. Evidence and cleanup

**Evidence** (all under `scratchpad/execution/tool-lifecycle/mechanisms/`, gitignored):
- **Scripts:** `hb.py`, `vendor.py`, `anchor.py`, `daemon.py`, `driver.py`, `summarize.py`, `pidfd_proof.py`, `smoke.py`, `probe_unshare_constraints.py`.
- **Matrix:**
  - `evidence/results-{m1,p1}.json` and `summary-{m1,p1}.txt`: the full matrix, twice;
  - `results-m2` / `summary-m2`: PID namespace with fresh `/proc`;
  - `results-m3` / `summary-m3`: daemon subreaper;
  - per-run logs in `runs/`.
- **Individual probes, in `evidence/`:**
  - `pidfd_proof-p1.json`
  - `unshare_constraints.txt`
  - `userns_effects.txt`
  - `dbus_in_pidns.txt`
  - `docker_default.txt`
  - `overhead_and_interop.txt`
  - `tmux_outside_server_escape.txt`
  - `cgroup_bindsto.txt`
  - `smoke-{claude,codex,codex2,opencode}.json` (raw harness output in `harness/`)
- `results-t0.json`: an early smoke run.

**Cleanup:**
- Every process I started carried a `VIAMECH`/`viamech` marker or ran inside a namespace that was killed. Each run was swept with `pkill -U <me> -f <marker>` and verified empty.
- Final checks found no marker processes, no `sleep 47.*`, and no processes with a cwd under the mechanisms folder.
- Systemd: no `viamech` transient units remain; the probe scopes were garbage-collected.
- Docker: no containers remain (all ran with `--rm`).
- tmux: my private server was killed and its socket file removed.
- The one temporary file created inside a namespace was deleted.
- No sudo, no sysctl writes, no installs, no unit-file changes.
- `git status` is unchanged from the start.
- **Harness side effects:** Claude ran with `--no-session-persistence` and Codex with `--ephemeral`. OpenCode stored its one session in its own database, which is normal use.
- **One self-inflicted slip:** an early ad hoc `pkill -f` matched my own shell. It killed only that shell, and the bounded heartbeats from that run were confirmed gone.

Concerns summary: Step 1 still leaves anchor-crash cases (c1, c3) and outside-service handoffs uncovered. Killing tools when the run ends is a product policy decision for the owner.
