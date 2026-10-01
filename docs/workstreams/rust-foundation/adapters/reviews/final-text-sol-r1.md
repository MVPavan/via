**UNSOUND — one Important wording defect.**

- **Important — [docs/specs/via-api-v1.md:583](../../../../../docs/specs/via-api-v1.md#L583): recovery after a failed write is unconditional.** The text promises a cut, synced, named file unless sync fails. Implementation also returns no reference when truncation fails or a timed-out append loses its file handle: see [Store `finish`](../../../../../crates/via-store/src/final_text.rs#L159) and [append’s error path](../../../../../crates/via-store/src/final_text.rs#L136). These failures can occur before sync is attempted.  
  **Smallest fix:** say a failed file step fails the turn `store`; after a failed write, the retained prefix is named with `truncated: true` **only if truncation and both syncs succeed**, otherwise `final_text_file` is `null`.

The other requested behavior matches:

- **Ordering:** Store truncates if necessary, syncs the file, then syncs its folder. Core awaits settlement before committing the terminal envelope.
- **Ordinary write errors:** short writes preserve the whole-character prefix; a write failing before any byte names an empty file. Both produce `failed(store)` when finalization succeeds.
- **Sync errors:** either sync failing yields no final-text reference and `failed(store)`.
- **64 MiB cut:** the implementation retains the UTF-8 prefix ending at or before the cap, sets `truncated: true`, and does not fail the turn for size.

With that qualification, the amendment is consistent with the cited C1 and runtime sections. §3.12 lists existing evidence files, which need not all be referenced by an envelope. Terminal persistence failure still follows the Store rule; no envelope is fabricated.

The distinction from structured output is clear and defensible: a marked UTF-8 prefix remains useful evidence, whereas partial JSON cannot represent the complete validated value. Structured-output spill failure participates in the naming commit’s retry and resolution rules; final-text write failure already marks the turn failed.

**Existing test:** [s1_bounds_final_text_spills_to_a_file](../../../../../crates/via-cli/tests/s1_bounds.rs#L199) checks the cap, short-write, zero-byte write-failure, sync-failure, and kill-after-commit outcomes. The crash pause is reached after the terminal commit attempt. It supplies the requested failpoint/crash test.

**Could not verify:** I did not execute tests. The existing test uses a lowered cap, injects failure before file sync, and tests process death rather than power loss. It does not independently detect omission of either sync call, inject directory-sync failure separately, or cover the finalization failures identified above. The sync ordering itself is verified from source.