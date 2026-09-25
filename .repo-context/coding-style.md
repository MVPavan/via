# Coding style

The implementation language and toolchain are undecided (Go leads, provisional;
see `.repo-context/invariants.md`). No formatter, linter or project layout is
set; do not add one, or any source code, before the owner approves the
prototype plan. Replace this file once the language is chosen.

Language-neutral rules that apply now:

- Match the surrounding Markdown: dense prose, tables for comparisons, short
  sections, no filler.
- Cite repo-relative paths; mark parent-repo paths as such
  (`.repo-context/repo-map.md`).
- Label claims as checked, reported, inferred or UNVERIFIED, as the design
  record does; include dates for vendor facts that change.
- Record decisions in the design record (`docs/`), with owner and date; keep
  `.repo-context/` files as routing and summaries that cite their source.
- Prototypes and experiments go in `scratchpad/` until the owner approves a
  source tree.
