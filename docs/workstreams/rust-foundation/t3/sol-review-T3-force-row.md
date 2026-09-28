# Task 3 force row and decision 5: Sol reviews

GPT-6 Sol (medium) reviews, verbatim, local links converted to repo paths.

## Force row (5f3c9f7..6f5be03)

**Verdict: SOUND WITH CHANGES.** Route owns the meaning of a Host-recorded exit: Wire supplies the exit fact, while Route has the daemon force signal. The new check covers `finalize`; the pre-terminal EOF path already checks force after `wait_exit` (`crates/via-routes/src/runtime.rs:321`, `crates/via-routes/src/runtime.rs:407`). I found no other path where an exit *returned to Route under force* reaches Core as `failed(process_exited)`.

**Ranked findings**

1. **Minor — record the new validation seam in the design.** The report identifies the missing §10 seam entry and §11 regression entry (`docs/workstreams/rust-foundation/t3/reports/T3-force-row.md:166`). Add both to `docs/workstreams/rust-foundation/t3/design.md:1552`, including that the pause occurs after Wire records the exit and before Route receives it.

2. **Minor, pre-existing — Wire’s cancellation comment overpromises.** `read_either` uses a non-biased `select!`, so a ready read can win over a ready cancel despite “before any byte is read” (`crates/via-wire/src/runtime.rs:542`). This does not reopen the recorded-exit failure fixed here. Address the comment and selection policy in a separate Wire change with a focused test; changing selection priority is not needed for this patch.

The regression’s self-exit-before-force case matches the design’s **observation** rule: an exit Route observes under daemon force takes the force row (`docs/workstreams/rust-foundation/t3/design.md:872`). It does **not** prove Host stopped a live vendor, so `requested` rather than `forced` is consistent with the spec (`docs/specs/runtime-contracts.md:872`). The seam is feature-gated and listed in the release check; the watch value is copied before the await. The pause and stop receipt fix the ordering the regression needs.

I inspected the specified diff, relevant callers, design, specs, report, Git status, and `git diff --check` (passed). I did not run cargo, bd, or independently reproduce the report’s load-test results.
## Decision 5 round 1 (6f5be03..be754ec)

**Verdict: UNSOUND.** The change records the right timestamp, but it does not establish the specified cleanup behavior in the delayed-watcher cases.

1. **Important — Force has two observable states.** `crates/via-core/src/engine/latch.rs:562` publishes `forced_at` before the bool force watch. Host can wake and stop a vendor while `crates/via-routes/src/runtime.rs:478`, allowing the exit to miss the force row. Core owns both watches, but their separate publication makes the new one an effective side channel. **Fix:** carry the timestamp in one force watch and derive the boolean check from its presence. The inspected production paths raise force through `raise_force`; I found no separate production write.

2. **Important — A delayed watcher can still miss the three-second cleanup requirement.** Until `crates/via-host/src/host.rs:829`, its ledger has no `stopping` value. The `crates/via-host/src/host.rs:1161` can pass after force is raised if its caller checked force just beforehand. If the watcher runs after the deadline, `crates/via-host/src/host.rs:1964`; the worker’s “sends Stop with no grace” claim is therefore unproven. The timestamp prevents a *fresh allowance*, but does not ensure a group is stopped within three seconds as §6.8 and runtime §7 require. **Fix:** make the ARM gate observe force atomically with its phase decision, and provide a cleanup path that reliably attempts Stop when the recorded deadline has already passed. Preserve `unknown` when ARM may have been sent but no stop or terminal evidence was obtained.

3. **Minor — The late-registration test covers a different path.** In `crates/via-host/tests/s1_host.rs:832`, acquisition pauses at `anchor_intent`, after `crates/via-host/src/host.rs:1046`. It exercises the later ARM gate, not registration after the snapshot. **Fix:** pause before registration, take the snapshot, then release registration and assert it receives the original deadline.

4. **Minor — The daemon test has a wall-clock flake risk.** `crates/via-cli/tests/s1_store_failure.rs:2582` starts its clock before launching the stop client and allows roughly one second of scheduling margin. Keep it as an integration check, but assert the deadline Host received through a deterministic test seam or a paused-time Host test. The reported RED is meaningful for the old fresh-budget behavior; I did not independently verify its logs.

The new seam is feature-gated and appears in the release exclusion list. I found no new lock-order or await-with-ledger-lock issue. This was source inspection only: I made no edits and ran no Cargo, tests, or Beads commands.
## Decision 5 round 2 (6f5be03..b08b4e8)

**Verdict: SOUND WITH CHANGES.** D5-1 and D5-2 close the round-1 races. Two deadline claims still need correction.

1. **Important — late replies can record forced evidence.** `crates/via-host/src/host.rs:566` uses `timeout_at` for the reply. Tokio polls the read before checking the timer, so a reply ready after the deadline can succeed; `crates/via-host/src/host.rs:2037` then records `forced`. **Fix:** check the clock after a successful reply, before setting the fact, and test an already expired deadline with a ready `Stopping` reply.

2. **Important — the absolute bound and the late-write behavior conflict.** `docs/specs/runtime-contracts.md:954` says Host *stops* groups within 3 seconds. `crates/via-host/src/host.rs:2030` deliberately waits for the control lock and completes the write even after that deadline. A blocked write can leave the early-stop task pending; `crates/via-host/src/host.rs:1692` remains deadline-bounded, but cannot make the Stop arrive. The one-outstanding-request, ≤1 KiB protocol makes a full buffer unlikely; it does not prove the absolute guarantee. **Fix:** update normative §7 to specify the deadline for the attempt, late delivery, and uncertain cleanup when delivery fails; then align design §6.8. If complete delivery by 3 seconds is required instead, an arbitrarily unwritable socket makes that guarantee impossible. A bounded write future can guarantee a *polled attempt*, not a completed frame.

The **exactly-one-of-three ownership argument holds** for an acquisition that continues through its ledger sections: registration refuses after force; a verified entry meets the ARM gate; an arming entry is stopped by its owner after `Spawned`; an already armed entry enters the task’s one-time snapshot. The phase changes and snapshot share the ledger mutex. No await occurs under it, and `raise_force` takes no ledger lock, so I found no lock-order cycle. A concurrent caller close may send another `Stop`; §5.1 makes that idempotent. `crates/via-core/src/engine/latch.rs:565` is the sole production force writer I found. Its single `send_if_modified` publishes first-raise-wins atomically; the structural proof is sufficient despite the missing behavioral RED.

The D5-2 and D5-3 REDs target meaningful prior failures. The late-registration test really does pause at anchor intent **before** `crates/via-host/src/host.rs:1108`; my round-1 D5-4 premise was wrong. The deadline unit test asserts `at + EARLY_STOP` exactly. Removing the wall-clock daemon test loses a daemon-level timestamp assertion, but the direct Host assertion is stronger for that value; the existing daemon stop coverage remains.

Merge hazards are the mechanical force-type edits in `drive.rs`, `close.rs`, and `reprobe.rs`, plus overlapping `host.rs` context near `reprobe_held`; resolve against merged `rust-foundation` and verify the result. I inspected source, diffs, tests, reported RED/GREEN evidence, and Git status. I did **not** run cargo, bd, or independently reproduce the reported test runs. The worktree remained clean.


## Decision 5 round 3 (b08b4e8..a4f12df)

**Verdict: SOUND WITH CHANGES.** The code fix is correct and minimal; §7 still needs more precise wording.

- **Finding 1 addressed.** `crates/via-host/src/host.rs:570` rejects a successfully read reply when the clock is at or past the deadline. `transact_by` has one caller, `stop_through` (`crates/via-host/src/host.rs:2043`); that covers both the early-stop task and the owner’s post-`Spawned` stop (`crates/via-host/src/host.rs:1262`). I found no other caller whose behavior changes.
- **The RED is meaningful and deterministic for this race.** `crates/via-host/src/host.rs:2431` makes the reply readable before using an already expired deadline. The saved RED log shows the old code recorded forced evidence. It is a synthetic preloaded reply, but directly tests the read-first timer behavior without relying on timing luck. I did not run cargo.
- **The unchanged close needs a narrower spec sentence.** I agree that a separate Route close can obtain truthful `stopped_live` evidence under its own deadline (`crates/via-host/src/host.rs:1737`). But §7’s unqualified “a reply received after the bound never counts as force evidence” (`docs/specs/runtime-contracts.md:959`) is broader than that distinction: close can also read a reply left pending by a timed-out early stop and set `forced`. Specify that the **early-stop transaction** does not count such a reply.
- **Finding 2 is only partly resolved in the spec.** §7 says the 3 s bound “bounds the `Stop` attempt” and “every armed group is sent one `Stop`” (`docs/specs/runtime-contracts.md:957`). The control lock and write in `stop_through` can remain pending past that bound (`crates/via-host/src/host.rs:2038`); §7 itself acknowledges that a completed write cannot be promised. Describe an attempted late delivery and an uncertain outcome if delivery cannot complete, rather than a bounded attempt or guaranteed send.

Read-only inspection covered the supplied diff, callers, spec commit, test, and saved RED log. No files were edited; cargo and bd were not run.