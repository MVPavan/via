> Preserved at the owner-directed pause, 2026-09-26. Historical snapshot: source line numbers and intermediate dispositions below may predate the frozen code. Refer to `../session-handoff.md` for current status. Artifact filenames without a repository path refer to the original local-only scratchpad directory.

> Design correction received Astra-high design and Sol-high PASS. Shared-spec integration, implementation and acceptance remain pending. Preservation here does not apply the proposed amendments.

# S1 shutdown ownership and uncertain exit

Status: Astra-high proposal for independent Sol-high review, 2026-09-26.
Scope: ownership after an operation deadline versus final daemon process exit.
Private design only; no new supervisor, persistent service or ownership graph.

## Verified tension and decision

C1 §§3.5–3.6/7.4–7.6 already separate cancellation outcome from cleanup,
permit uncertain cleanup and keep crash recovery conservative. Runtime §5
already acknowledges that anchor self-group KILL kills the vendor reaper and
allows OS adoption without claiming Host reaping. Runtime §5.2's instruction
to retain reaping ownership after a caller deadline is ambiguous about final
daemon death; §6 normal shutdown joins owners, while §7 F12 bounds final
shutdown and reports failures.

Current `via-host/src/host.rs` retains timed-out JoinHandles in Arc<HostTasks>.
Its shutdown report has recovery and pending count, but a recovery error loses
that report and completed join errors are ignored. Current daemon stop sends
`{"stopping":true}` before cleanup, then exits nonzero on an incomplete report.
Holding a Rust handle cannot keep a reaper running after its daemon dies.

**Decision:** a caller deadline never abandons an owner while the daemon
continues serving. Final daemon process exit is a separate, explicit boundary:
if joins, cleanup or flushes remain unresolved, exit is **incomplete**, with
nonzero daemon exit status and truthful evidence. OS adoption may follow; it
proves neither child exit, group absence nor reaping. No perpetual replacement
reaper is required or introduced. Positive shutdown still joins everything
that VIA owns and satisfies the existing cleanup/durability predicates.

## 1. Per-operation deadline while the daemon lives

Cancel, close, wall/idle expiry and a Host shutdown attempt use their existing
absolute deadlines. Work cannot continue to gain a fresh wall budget merely
because cleanup is pending. A private group still has the reviewed 3 s OS
cleanup allowance; an explicit close deadline already includes its allowance.
Vendor-specific pending cleanup rules such as C1 P7 remain unchanged.

At deadline, return the evidence available: uncertain cleanup if absence is
unproved, or quiescent if the existing positive predicate was established.
Do not conflate this with whether the daemon has collected all task joins.
Retain reaper/status handles and their necessary owners in Host's tracked
state; a session driver's drop cannot be the last owner of that state while
the daemon stays alive. Continue observing exits/collecting finished tasks.
Unresolved processes do not release admission capacity solely because a wait
timed out. No detached task, new runtime or background helper is created.

Cancellation of the **shutdown future itself** is also a live-daemon case.
The owned registry must retain every unfinished JoinHandle until its result
is collected, even if a caller drops/aborts that future at an await. Moving
handles into a local vector with `mem::take` and reinserting only on normal
return is not cancellation-safe and does not satisfy this rule. A later
shutdown/collection attempt must still see those joins. The Host owner fixes
this within its existing registry; it is not the final process-exit exception.

A later cleanup observation does not silently rewrite an already-settled
envelope outside C1's existing revision rules. Host can still record new
process/absence evidence and reap a child. A caller timeout is not proof
that a Store command stopped; all existing no-resend and F12 rules remain.

## 2. Explicit daemon shutdown

Keep C1 daemon/stop's current acceptance reply `{"stopping":true}`. It means
the request was accepted and the relevant admission gate is closing; it is
**not** proof that processes stopped, Store flushed or the daemon exited.
The CLI may report “stopping requested,” never “clean shutdown complete” from
that receipt alone. A caller disconnect does not cancel the accepted stop.
Active-work refusal without drain/force remains unchanged.

- `--drain` lets already accepted turns finish under their existing work
  deadlines. Its drain phase is not given an invented 10 s work deadline.
  Once the accepted queue and active work settle, enter final shutdown.
- `--force` enters final shutdown immediately after accepting the request,
  closing sessions according to existing C1 semantics. An idle ordinary stop
  also enters final shutdown immediately.
- Final shutdown uses **one absolute 10 s deadline**, including client closes,
  Host cleanup, task joins, final durable records, raw sync and Store shutdown.
  Do not grant Host another 10 s after other phases consumed this budget.
  F12 instead measures that same total from first Store failure, as already
  specified; do not restart its clock when final shutdown begins.

Stop admission/listening, settle or classify turns, close input and request
owned-group cleanup while output drains, collect process/task evidence, sync
raw and commit final records, then join Store. Keep the Core Store owner alive
while any live lower-layer work still needs it. This preserves the normal
write/close order and reviewed ownership graph.

**Clean shutdown:** required cleanup is positively proved, no owned join is
pending/failed, required raw flush and Store commits/joins succeeded. Emit the
bounded clean diagnostic, release resources/locks and exit daemon status 0.
The earlier stopping receipt alone never establishes this outcome.

**Incomplete shutdown:** at the global deadline, or after an unrecoverable
failure that precludes clean completion, snapshot remaining uncertainty and
exit daemon status 4. Stop/drop control connections so existing anchors receive
EOF and run their already-reviewed cleanup. As part of final runtime teardown,
abort/cancel unfinished asynchronous tasks and collect whatever completes
within the remaining deadline; report tasks that did not join. No success
claim is inferred from abort, handle drop, lost control, or pending SIGKILL.
There is no promise that VIA keeps reaping after this process boundary.

Only daemon main selects this final incomplete-exit path. A library timeout or
Host handle drop must not call process exit or silently detach work while VIA
continues serving. Current blocking Store Drop must stay off Tokio workers.
If orderly drops/joins cannot complete, daemon main may take a final nonzero
process-exit path instead of waiting indefinitely in them; locks are then
released by process termination, not manually released beneath live Store
threads. This is crash-like termination with conservative recovery, not a
successful flush or graceful shutdown. No second Store owner is added.

The daemon must attempt final exit within the total deadline. For returned
I/O errors and controllable asynchronous stalls, actual bounded exit remains
a required test. An uninterruptible kernel operation may prevent even OS
termination; report that as an unmet process-exit bound/infrastructure failure,
never a pass or an excuse to claim successful cleanup. This retains runtime
§7's existing limitation rather than promising an impossible Rust timeout.

## 3. Required report, separate facts

Host shutdown must return its snapshot on every path, including an already
expired deadline and journal/recovery failure. Smallest concrete shape:

```rust
pub struct ShutdownReport {
    pub recovery: Vec<RecoveryReport>,
    pub pending_tasks: usize,
    pub failed_tasks: usize,
    pub failure: Option<HostError>,
}
// Existing Host::shutdown returns this report rather than discarding it on Err.
```

`pending_tasks` counts retained, unjoined tasks, including those whose finish
was not collected; zero alone is not success if a join failed. Collect and
count panic/cancelled JoinErrors instead of treating every completed handle
as successful. A reaper must also preserve a failed child wait as a failed
task outcome; finishing the Rust task after ignoring `child.wait()` failure
is not successful reaping. `failure` preserves the named deadline/Store/
recovery failure; keep whatever per-owner recovery facts were established
before it. A shutdown-specific recovery loop or private partial-result helper
may retain those facts; the public recovery API need not change. Adapter/
Wire propagate a bounded passive summary, never operating Host handles.

The daemon's final summary separates: stop mode; elapsed time; pending and
failed joins; process owners with uncertain cleanup; raw flush status; Store
commit/join status; and overall clean/incomplete disposition. Do not equate
`GroupAbsent` with reaped, or a joined status task with group absence.
Existing session/turn cleanup and journal evidence are persisted only on
successful Store commits. This seam adds no daemon/status field or new RPC. No new durable-report subsystem
is introduced: final diagnostics are best-effort and may be lost on Store
failure or abrupt process death; never delay final exit indefinitely to print.
The outer harness captures daemon exit status and available diagnostics.

If a result cannot be persisted, retain F12's named store_error and
terminal_persisted:false behavior. Do not manufacture an envelope or replace
a prior committed vendor result with the daemon's shutdown failure. Restart
uses existing durable submission intent, unknown/no-resend and anchor cleanup
rules even if the final incomplete diagnostic was lost.

## 4. Acceptance and exact amendments

Positive F19–F22, platform P-I2 and ordinary shutdown tests remain unchanged:
prove owned-group absence, no unrelated signal, correct ordinary grandchild
cleanup, and the required joins/flushes. An incomplete exit never substitutes
for these positive cases. Add bounded regressions:

1. A short per-operation deadline returns pending/uncertain, but the daemon
   remains alive and retains the reaper. Release the controlled child barrier;
   its exit is subsequently observed and the join is collected.
   Separately cancel the shutdown future while it awaits a child: a second
   inspection/shutdown must still report that owned join, and releasing the
   barrier must allow it to be collected without a detached task.
2. Clean daemon stop emits acceptance, then completes group absence, joins,
   flush/commit and daemon exit 0. Assert the receipt precedes completion and
   does not by itself satisfy the gate.
3. Hold an owned task past final shutdown's deadline. The report preserves
   its pending count, daemon exits 4 within the bound, and the fixture harness
   cleans up using the existing anchor/evidence seam. No fake reaping or
   quiescence is reported. Separately make a task panic to prove failed count.
4. Combine journal/recovery error with a pending reaper: both failure and
   pending count survive through the C2/Wire summary; no error-only path loses
   ownership/evidence. Persistent Store failure still passes its original
   named-error/no-resend/restart tests, not a weakened shutdown-only test.
5. After incomplete daemon exit, ordinary surviving anchor EOF/reconnect
   cleanup must satisfy F22's positive predicate. A deliberately unverified
   survivor remains explicit uncertainty and is never numerically signalled.

| Shared source | Exact change |
|---|---|
| Runtime §5.2 deadline paragraph | Retention “afterward” applies while daemon lives; final daemon exit follows explicit incomplete policy and makes no continuing-reaper guarantee. |
| Runtime §§2,6,7 | Define clean vs incomplete final exit, preserve owner until normal joins, use one shutdown deadline and retain the blocking-I/O/process-exit limitation. |
| Runtime Host report sketch / C2 bootstrap shutdown | Carry pending/failed joins and named failure even when recovery fails; keep lower identities out of Core operational access. |
| C1 §3.14 | Specify stopping acceptance-only response; drain work phase versus final 10 s shutdown, daemon exit 0/4 evidence. No new cancel outcome or fake graceful-cancel semantics. |
| Coding standard §§5–6 | No-detach ownership applies throughout live daemon operation; explicit final incomplete process exit may leave OS-adopted children, without a quiescence or Host-reaped claim. |
| Runtime §11 / existing harness gates | Add five regressions above without relaxing positive process/Store gates or adding a new supervisor. |

Sol review should assess the explicit final-exit exception and receipt meaning,
not treat retained Arc handles as proof that tasks survive process death.
