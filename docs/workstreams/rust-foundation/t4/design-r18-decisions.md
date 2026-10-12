# T4-0 design round 18: decisions

Input: `review-r17-sol.md`, Sol high's review of round 17 at `d43a56f`
(UNSOUND: 1 blocker, 2 important). Sol confirmed that the other round-16
findings are resolved and accepted identity by length and SHA-256. Tag each
change `[t4r18.N]`. These are targeted fixes only.

1. **One pass for a prompt file, outside `admission`** (the blocker and
   Important 1).
   - For a `prompt_file` request, the handler first reads the file once,
     with no lock held. It streams the file into a temporary blob while
     computing length and SHA-256, and runs the existing before and after
     `fstat` change check, all under the 10 s bound. The identity comes from
     that same pass, so it always matches the blob's bytes.
   - The handler then takes `admission` and looks up the key:
     - on a match, it returns the stored receipt, or `idempotency_conflict`
       if the identity differs, and discards the temporary blob;
     - for new work, it applies the disk floor and the `wal.max` checks, then
       commits the turn, which adopts the blob; a refusal discards the blob.
   - `admission` is never held during file I/O, so `close` and daemon stop
     are not delayed (`crates/via-core/src/engine/stop.rs:114`).
   - The retry-before-floor rule from round 17 decision 1 still holds: the
     floor is checked only after the lookup finds no key. A retry below the
     floor writes at most one temporary blob, which is discarded. This is
     accepted, because the floor leaves gigabytes free.
   - Remove the separate identity-only streaming pass.
   - Tests:
     - a file changed between two requests with the same key gives
       `idempotency_conflict`, never a mismatched blob;
     - a slow copy does not delay a concurrent `close`.
2. **`status` on a terminal turn** (Important 2). When the turn that `status`
   selects in the Store is terminal, `progress` is `null`, whatever the
   in-memory `Running` entry still shows. The entry is cleared after the
   terminal commit (`crates/via-core/src/engine/drive.rs:699`). Test the
   interval between the terminal commit and `finish_running`.

## Outputs

- **`t4/design.md`:** apply 1–2, and update A39 and the tests.
- **`t4/reports/T4-0.md` §22:** map each finding to its change.
- **Self-check:**
  - no identity-only pass remains;
  - no file I/O runs under `admission`;
  - recheck the citations;
  - no absolute paths and no "frame".
- **Commit** on `wt/t4-0` as "docs(workstream): T4-0 round 18 design and
  report", with the Opus trailer, then stop.
