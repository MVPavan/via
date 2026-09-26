# Verification

Rust 1.98.1 is the chosen toolchain. Run checks from the repo root.

## Rust gate

Until tests exist, export `NEXTEST_NO_TESTS=pass` before running the gate;
remove the setting when the first tests land. Nextest's empty-suite behavior
is a CLI setting. Run in order:

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace
cargo deny check
python3 scripts/check-layers.py
```

Testing policy: pending owner decision.
Live-vendor end-to-end sets are a separate merge gate, defined by that policy.

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
