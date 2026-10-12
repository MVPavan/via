# W4-I: store_error data, bounded unresolved set, deadline class

Model: Opus 5.5 medium. Follow `../w1/common.md`; report to
`reports/W4-I.md`. Findings in full: `../w3/sol-review-W3-G.md` 1, 4, 5.
Runs in parallel with W4-H. Owned: `via-core` API errors and the
engine journal, `via-routes`, and in `via-cli/src/server.rs` only error
rendering. Keep shared hunks small and name them.

1. **C1 `store_error` data.** Unresolved reads return the static
   `ApiError::STORE`, so the response lacks C1 §3.8/§9 `data.session`,
   `data.turn`, `data.durable_state` and `data.terminal_persisted: false`.
   Carry the data through `ApiError` and render it. Assert the complete
   JSON-RPC response through `result` and `wait`, end to end where the
   fake agent allows, otherwise at the server boundary.
2. **Bounded unresolved set.** `Unresolved` keeps every failed turn until
   daemon exit, including turns whose terminal later becomes readable.
   Remove settled entries, bound the rest, and keep returning
   `store_error` for affected turns. Test growth and removal.
3. **Deadline during a raw append.** Route maps every `RawDeadline` to
   `Store`; when the turn's work deadline expires during an append, C1
   calls for `deadline_wall`. Keep which deadline expired (work vs
   failure cleanup) and test both dispositions.
