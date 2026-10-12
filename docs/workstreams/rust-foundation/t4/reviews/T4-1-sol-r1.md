**UNSOUND**

## Findings

- **Important — `crates/via-wire/src/runtime.rs:597`:** `undecoded.bin` is reported as saved after `write_all`, without syncing the file or its directory. The terminal envelope can then name that file and commit durably, but a power loss can leave it empty or absent. This violates the `.repo-context/coding-style.md:203`. Sync the file and parent directory before returning the path; report a sync failure as “not saved.”

The plan’s Sol scrutiny checks found no other concrete defect: terminal paths set `ended_seq`; event writes set `updated_ms`; `session_ord` advances without reuse; the turn folder is created exclusively and its parents synced before Host acquisition; folder and `stderr.log` creation failures precede anchor intent and map to `failed(store)`; Host passes stderr as a file that production VIA does not read; identity retries compare length and SHA-256; and `logs` stats fixed names on the blocking pool without opening contents. The generation check could not be assessed because the base has no identity-confirmation path.

The changed F12 fixture still tests early stopping of a live group. The fourth restart-handoff commit matches removal of `raw_log.incomplete`; over-cap stdout maps to the specified `overflow` class; `create_new` protects `undecoded.bin` from a final-component symlink; the edits outside owned paths serve the changed interfaces. The nonbuilding intermediate commit is a bisect limitation, but no rule requires each intermediate commit to build.

## Measurement suggestions

- The stderr test’s 2.2-second wall-clock assertion has a possible false-failure window under scheduler or CI stalls. Measure repeated runs under load before treating it as a finding; the focused run passed.

## Could not verify

- Vendor identity confirmation, transcript persistence, and an in-memory generation check have no producer in the base implementation. The nullable schema and current fake behavior were reviewable; real-vendor behavior was not.
- `cargo deny check` could not acquire its advisory database lock on the read-only path. I did not rerun the release gate or the no-feature suite. Verified here: formatting, both Clippy configurations, layer check, the six-test T4-1 selector, and the full failpoint suite (**429 passed, 1 skipped**). Git status remained clean.
