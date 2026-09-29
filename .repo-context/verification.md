# Verification

Rust 1.98.1 is the chosen toolchain. Run checks from the repo root.

## Rust gate

Runtime tests now exist. Empty-suite success is not permitted. Run the gate
in order; the paused checkpoint and its actual results are recorded in
`docs/workstreams/rust-foundation/session-handoff.md`.

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace
cargo deny check
python3 scripts/check-layers.py
```

Root or CI runs (a foreign-uid listener needs CAP_SETUID) also run the
ignored peer-UID end-to-end check:
`cargo nextest run --locked --workspace --run-ignored only -E 'test(c1_client_refuses_daemon_socket_of_another_uid)'`.

Testing policy: approved; `.repo-context/coding-style.md` §10 is authoritative.
The default gate uses fake vendors, with failure-first isolated tests where
they provide sharper evidence. Small live-vendor end-to-end sets are a separate
gate before each adapter slice merges; infrastructure failures are not passes.
S1 has no live-vendor gate.

## S1 runtime acceptance, when tests and failpoints land

The following commands are required from the repository root for the S1
implementation. They are future gates, not claims that the scaffold already
has the feature, script or nonempty suite. Do not set `NEXTEST_NO_TESTS=pass`
for acceptance; record nonempty test counts, durations, actual feature graph
and fixture/artifact hashes.

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace
cargo deny check
python3 scripts/check-layers.py
cargo clippy --locked --workspace --all-targets --features via-cli/test-failpoints -- -D warnings
cargo nextest run --locked --workspace --features via-cli/test-failpoints
cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08|09|10|12)_/)'
cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_(f2[4567]|bounds|store)_/)'
cargo build --locked --release -p via-cli --no-default-features
python3 scripts/check-release-features.py target/release/via
```

The `s1_(f2[4567]|bounds|store)_` selection is Task 4's scenario set
(`via-jm4.7.8`). Until those scenarios exist it exits 4 with "no tests to
run"; that is expected and is not a pass.

`test-failpoints` is default-off and test-only; its feature wiring, code and
environment parsing must be absent from the release feature graph.
`scripts/check-release-features.py` inspects that graph, launches release VIA
with every known activation input (all Task 2 points armed, plus a partial
configuration) and verifies they are ignored, then scans for unique control
marker strings as supporting evidence. A string scan alone is insufficient. It
drives the fake agent, so `target/debug/via-fake-agent` must exist (the
nextest lines build it). Points added by later tasks join its `POINTS` list.
Scenario tests `s1_f01_...` through `s1_f30_...`, `s1_bounds_...` and
`s1_store_...` must use real daemon/SQLite paths and emit a summary, sha256
manifest, consistent SQLite backup, event logs, the turns' evidence folders
and report under `scratchpad/`. The gate fails for missing evidence; never treat a
copy of a live WAL file as a consistent backup.
F22/P-I2 requires both positive cleanup paths and negative identity refusals
specified in `docs/specs/runtime-contracts.md` §5.2 and §11. A result that
only records uncertainty does not pass that positive gate. P-OWNER-1's narrow
macOS system-library linkage exception is owner-approved and recorded in
`docs/specs/platform-packaging.md` §1 and invariant #4. The current release
gate requires the fully static Linux artifact on an actual kernel 5.15
baseline and current Linux configuration; macOS artifact production,
linkage inspection and native qualification are deferred together under
`via-pvj.4`, not passed by Linux/WSL or cross-build evidence.
Path/bootstrap cases from runtime §6.1/§11 cover precedence, unsafe targets,
same-State/different-runtime writer refusal, client Store mismatch and
isolated scenarios. Claude and Codex vendor qualification cases remain
separate live gates; a reviewed design or zero-test selection cannot pass
them.

## Checks that apply now

1. **Skill and path catalog** (after changing `.claude/`, `AGENTS.md` or
   `.repo-context/`): must report no `FAIL`; `WARN` lines are advisory.

   ```bash
   python3 .claude/scripts/skill-catalog.py --check
   ```

2. **Markdown link integrity** (after changing any `.md`): relative links must
   resolve. Expect `broken links: 0`.

   ```bash
   python3 - <<'EOF'
   import re, pathlib
   bad = 0
   for f in pathlib.Path('.').rglob('*.md'):
       if {'.git', 'scratchpad'} & set(f.parts): continue
       text = re.sub(r'```.*?```', '', f.read_text(), flags=re.S)
       for m in re.finditer(r'\]\(<?([^)>#\s]+)', text):
           t = m.group(1)
           if not re.match(r'[a-z]+:', t) and not (f.parent / t).exists():
               print(f'{f}: {t}'); bad += 1
   print('broken links:', bad)
   EOF
   ```

3. **Cited paths exist**: every repo-relative path cited in plain text in
   `.repo-context/` or `docs/` resolves in this repo, unless marked as a
   parent-repo path (`.repo-context/repo-map.md`).
4. **Public-repo hygiene**: the diff has no secrets, credentials, personal data
   or machine-local absolute paths.
5. **Git state**: inspect `git diff` and `git status`; stage explicit paths only.
