SOUND

**Findings:** None. The daemon now counts blob steps still running after the Store drain as pending work, selects an incomplete exit, and includes the count in its shutdown summary. The Store’s direct stderr diagnostic is gone. The new test checks that shutdown case. I found no new defect in the tip fix or the full T4-2 chunk against the plan and design.

**Could not verify:** I did not rerun runtime tests in this read-only review. The report’s referenced gate logs are absent from this worktree. `cargo fmt --all --check`, `scripts/check-layers.py`, `git diff --check`, and final Git status passed.