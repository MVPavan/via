**SOUND at `98d9928` for this review scope.** R8 and the partials it qualified are fixed. No new defects found.

All pointers below are at `98d9928`; `core/` means `crates/via-core/src/engine/`.

| Finding | Status | Evidence |
|---|---|---|
| R8 | Fixed | `core/lane.rs:1116`, `:1134`: one registry read determines drainage; shutdown keeps unfinished lanes registered. |
| N4 | Fixed | `core/lane.rs:648`, `:666`, `:1101`: work finishes before deregistration; close awaits completion and removes only the matching lane. |
| N5 | Fixed | `core/lane.rs:1144`; `core/stop.rs:558`: unfinished actors remain protected and contribute to incomplete shutdown. |
| r2 #3 | Fixed | `core/lane.rs:632`, `:1005`, `:1134`: definitive EOF drainage, completion before replacement, and continuous registry protection. |
| F4 | Fixed | `core/lane.rs:584`, `:634`; `core/drive.rs:1036`: attributed durable disposal remains protected against premature session closure. |

Registry callers handle retained `Ended` lanes correctly: claims refuse them, status excludes them, and replacement waits for completion before copying state and sharing the byte budget. Recovery installs lanes before normal admission. The shutdown closure pass permits closure only after drainage completes.

Actor removal and close removal both check pointer identity, protecting a concurrently installed successor. The actor releases the core lock before acquiring the registry lock; registry readers take registry → core. No reverse acquisition or new ownership cycle was found. Clean shutdown removes ended entries at [lane.rs:1148](../../../../../crates/via-core/src/engine/lane.rs#L1148), and tracker joining covers the actor’s remaining deregistration and notification work. Unfinished actors intentionally retain their ownership on incomplete shutdown.

The new regression exercises queued cancellation during the drain wait. Supplied RED shows the closed-session assertion failing; GREEN includes that regression and reports 108/108 targeted checks. The supplied gate reports exit 0: default 512/512, failpoints 745/745, S1 61/61. Snapshot `git diff --check` passed.

**Could not verify:** fresh independent Rust runs at the exact snapshot, exhaustive concurrent schedules, or runtime leak measurements. No files or Git state were changed.