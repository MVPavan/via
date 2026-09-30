**UNSOUND**

### Findings

- **Important — shared pool can refuse turn-critical work.** `crates/via-core/src/engine/read.rs:262` admits `logs` checks to the same 16-slot pool whose cap refuses new steps at `crates/via-store/src/blob.rs:151`. Sixteen concurrent stalled `logs` checks can therefore fill it; a prompt write, final-text step, or turn-folder creation is then refused. The old unowned `logs` checks could not consume these slots. The smallest fix is to reserve capacity for turn-critical steps and test that a saturated `logs` load cannot refuse one.

- **Minor — one intermediate commit fails the required gate.** In `d1bfa42`, `crates/via-core/src/engine/receipt.rs:208` makes `spawn_admitted` 101 lines, failing `clippy::too_many_lines`. `f81409d` fixes the tip, which passes Clippy. The intermediate commit still breaks a per-commit CI run or bisect; squash those two commits before preserving this history.

### Measurement suggestions

Record blob-pool occupancy and cap refusals by step type, especially under concurrent `logs` calls. That would inform the reservation size.

### Could not verify

- I did not rerun tests on `178e41a` because the review must leave Git state unchanged. The worker’s RED logs show failures for fixes 1–5; fixes 6–7 change tests and evidence handling rather than runtime behavior.
- No focused test cancels a caller while its step runs, or fills the pool to exercise turn-folder refusal at the cap. Ownership and failure mapping follow from the inspected code, but those paths lack dynamic proof.

The focused tests I ran passed, including the revised stalled-blob shutdown test. Current Clippy and the layer check passed; Git status remained clean.