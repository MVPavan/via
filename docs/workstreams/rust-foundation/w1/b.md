# W1-B: client peer check (T1-I2) and strict C1 requests (T1-I3)

Model: Opus 5.5 medium. Follow `common.md` in this directory.

Owned paths: `crates/via-cli/src/client.rs`, the request-parsing and
dispatch parts of `crates/via-cli/src/server.rs`, request DTO types in
`crates/via-core/src/api.rs` (requests only; W1-C owns receipts, envelopes
and events there), and new tests.

1. **T1-I2.** The client sends protocol traffic without verifying the
   daemon's peer uid; only the server checks. The client must verify the
   peer (SO_PEERCRED) before sending `hello` or any handle, and fail with
   the C1 "daemon unreachable"-class error otherwise.
2. **T1-I3.** Requests are handled as generic JSON values: no `jsonrpc`
   check, no `id` type check, no unknown-field rejection, and read, status
   and stop parameters bypass typed DTOs. Enforce C1 §1 and §8 for every
   method that exists today (typed params with `deny_unknown_fields`,
   correct error codes and `data.kind`). Do not implement methods that do
   not exist yet.

Do not change response shapes (W1-C) or the stop/shutdown path (W1-D).
