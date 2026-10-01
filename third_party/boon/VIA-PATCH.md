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
   - one unit per subschema evaluation, plus one per item of the array
     evaluated, one per 64 bytes of each member name of the object
     evaluated (at least one per name), and one per ancestor the cycle
     check walks;
   - one unit per value compared by `enum` and `const`, and per value hashed
     or compared by `uniqueItems` (hash collisions included), with strings
     and member names at one unit per 64 bytes;
   - one unit per 64 bytes of each name the dependency keywords look up and
     of each `required`, `dependencies` or `dependentRequired` name scanned;
   - one unit per 64 bytes counted by `minLength`/`maxLength`;
   - one unit per scope a `$dynamicRef` or `$recursiveRef` walks;
   - for `pattern` and `patternProperties`, the subject length times the
     pattern's weight (an estimate, from its syntax tree, of how many
     automaton states can be live at once), per 16 bytes.

   The depth cap bounds the evaluations active at once, nested validations'
   included. Once the budget is spent, every pending evaluation returns at
   once and no further error is built. `Schemas::validate` is unchanged in
   behaviour (an unlimited budget).

   Audit (fix round 2): every loop the yes/no mode runs over value or
   schema data is charged as above, or is proportional to work charged
   before it: the member scan, `Uneval` collection and merging (at most the
   members of the value the evaluation already paid for), the items and
   applicator loops (one evaluation each), `unevaluated*` (lookups of names
   paid for at entry). `format` and the `content*` keywords run no code for
   a draft 2020-12 schema under `require_draft` without `assert_format` or
   `assert_content`, which VIA does not set.
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
   A `$schema` inside an instance value (`const`, `enum`, `default`,
   `examples`) is data, not a declaration, and is not checked.
4. **Bounded metaschema checks** (`compiler.rs`, `roots.rs`, `draft.rs`,
   `lib.rs`). `Compiler::set_metaschema_budget(units, max_depth)` makes
   every metaschema check of the compiler (the schema documents, and each
   value a reference reaches outside the known subschemas) share one
   budget, charged as in validation plus one unit per node of each value
   checked, in the yes/no mode. An invalid schema fails with the new
   `CompileError::SchemaInvalid` (no detail), a spent budget with
   `CompileError::LimitExceeded`.
5. **Compile limits** (`compiler.rs`). `Compiler::set_limits(schemas,
   patterns)` makes compiling fail with the new
   `CompileError::LimitExceeded` beyond that many subschemas or regular
   expressions in one `Schemas`.
6. **Lint** (`Cargo.toml`): `mismatched_lifetime_syntaxes` is allowed, so
   newer toolchains do not warn on upstream's elided lifetimes.

VIA's limits and the measurements behind them are in
`crates/via-core/src/schema.rs`.
