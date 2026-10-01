# boon, vendored for VIA

- Upstream: boon 0.6.1 from crates.io (repository
  https://github.com/santhosh-tekuri/boon, commit
  9b7b7bffc2f4fd064baca141e3557849c88e0b4f per `.cargo_vcs_info.json`).
- crates.io checksum (the `Cargo.lock` entry before vendoring):
  `baa187da765010b70370368c49f08244b1ae5cae1d5d33072f76c8cb7112fe3e`.
- This directory is the published `.crate` archive, extracted unchanged and
  checked against that checksum, with this file added. The upstream licence
  files (`LICENSE-MIT`, `LICENSE-APACHE`) are kept.
- The workspace uses it through `[patch.crates-io]` in the root `Cargo.toml`
  and excludes it from the workspace members.

## Patch

Every change is marked `VIA patch` in the source. `git diff` from the
commit that vendored the unchanged copy shows the whole patch.

1. **Validation budget** (`validator.rs`, `util.rs`, `lib.rs`).
   `Schemas::validate_within(value, index, units, max_depth)` answers
   `Validation::{Valid, Invalid, BudgetSpent}` and builds no error detail
   (it runs boon's existing yes/no mode, which now also keeps no error
   tree). One `Budget` is shared by every evaluation of the validation,
   including the values `propertyNames` and `contentSchema` build. It is
   charged:
   - one unit per subschema evaluation, plus one per member or item of the
     value evaluated, plus one per ancestor the cycle check walks;
   - one unit per value compared by `enum` and `const`, and per value hashed
     or compared by `uniqueItems`, with strings at one unit per 64 bytes;
   - the names `required`, `dependencies`, `dependentSchemas` and
     `dependentRequired` scan;
   - one unit per 64 bytes counted by `minLength`/`maxLength`;
   - for `pattern` and `patternProperties`, the subject length times the
     pattern's weight (an estimate, from its syntax tree, of how many
     automaton states can be live at once), per 16 bytes.

   Evaluations nested deeper than `max_depth` spend the budget. Once it is
   spent, every pending evaluation returns at once and no further error is
   built. `Schemas::validate` is unchanged in behaviour (an unlimited budget).
2. **Bounded regular expressions** (`util.rs`, `compiler.rs`). Patterns
   compile with a 1 MiB program limit and no lazy DFA (its per-pattern cache
   and its give-up path made memory and time depend on the pattern rather
   than the budget). A `pattern` that does not compile is
   `CompileError::InvalidRegex`, as `patternProperties` already was, instead
   of `CompileError::Bug` (which asserts in debug builds).
3. **Draft pin** (`compiler.rs`, `roots.rs`, `draft.rs`).
   `Compiler::require_draft(d)` makes compiling fail with
   `CompileError::UnsupportedDraft` when any `$schema`, at a root or in any
   subschema position (embedded resources included), names another draft.
4. **Compile limits** (`compiler.rs`). `Compiler::set_limits(schemas,
   patterns)` makes compiling fail with the new
   `CompileError::LimitExceeded` beyond that many subschemas or regular
   expressions in one `Schemas`.
5. **Lint** (`Cargo.toml`): `mismatched_lifetime_syntaxes` is allowed, so
   newer toolchains do not warn on upstream's elided lifetimes.

VIA's limits and the measurements behind them are in
`crates/via-core/src/schema.rs`.
