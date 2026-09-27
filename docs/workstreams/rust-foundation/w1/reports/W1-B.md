# W1-B report: client peer check (T1-I2) and strict C1 requests (T1-I3)

Branch `claude/w1-b-task-9h00gm`, based on `639e77f`. Regressions are in
`crates/via-cli/tests/c1_protocol.rs`. They run the real `via` binary and
daemon against private state and runtime directories. Before any fix, all
three failed for the reasons below.

## T1-I2: client peer uid check

**Failure mode.** `client::request` connected to `via.sock` and wrote `hello`
right away. Only the daemon checked `SO_PEERCRED`. If another uid owned the
listener on the runtime socket path, it received the client's protocol
traffic, and for mutations that traffic includes the session handle.

**Regression.** `c1_client_refuses_daemon_socket_of_another_uid`. A helper
thread drops its own thread credentials to uid 65534 with `setresuid`, which
changes only that thread, then binds and listens on `via.sock`. The test then
runs `via daemon status --json`. It asserts three things: the listener
received zero bytes, the exit code is 4, and stderr has
`data.kind: daemon_unreachable`. Output before the fix:

```
assertion `left == right` failed: client sent 113 protocol bytes to a foreign-uid peer
  left: 113
 right: 0
```

**Fix.** `crates/via-cli/src/client.rs`: the new `verified_peer` reads the
listener's kernel peer uid and compares it with the client's euid right after
connecting, before anything is written. This applies to both the direct
connect and the connect after auto-start. The uid is read through Tokio's
`peer_cred`, the same source the daemon's check uses, so no new dependency or
feature was needed. A mismatch is an error. `main` already maps any client
error to exit 4 with `daemon_unreachable`. An isolated test,
`verified_peer_compares_the_kernel_peer_uid`, covers the comparison on a
socket pair.

## T1-I3: strict C1 request envelopes and parameters

**Failure mode.** The daemon handled each request line as a generic
`serde_json::Value`:

- It checked neither `jsonrpc` nor the type of `id`.
- It accepted unknown envelope members and requests without an `id`.
- It treated arrays and scalars as `method_not_found` instead of
  `invalid_request`.
- `daemon/status`, `daemon/stop`, `result`, `wait`, `events` and `logs` read
  fields out of the `Value` directly and ignored any other fields. Unknown
  members and wrong types were therefore accepted. For example,
  `daemon/stop {"force":"yes"}` was treated as `force:false` and stopped the
  daemon.
- `serde` accepted by-position `params` arrays for the typed DTOs.
- `invalid_params` never carried `data.kind2: unknown_field`.

**Regressions.**

- `c1_request_envelope_is_strict` sends 20 bad envelope lines, some before
  and some after `hello`. Two of them are controls that already passed:
  malformed JSON and an unknown method.
- `c1_request_params_are_typed_and_reject_unknown_fields` sends an unknown
  member to each of the 9 current methods, plus wrong-type and missing-field
  cases. It then checks that a well-formed request still succeeds.

Output before the fix, abridged: 18 envelope cases got `result` or the wrong
error, for example:

```
missing jsonrpc: expected id 7 code -32600 kind invalid_request, got {"id":7,...,"result":{...}}
object id: expected id null code -32600 kind invalid_request, got {"id":{"n":7},...,"result":{...}}
batch: expected id null code -32600 kind invalid_request, got {"error":{"code":-32601,...}}
status params by position: expected ... -32602 invalid_params, got {"id":7,...,"result":{...}}
daemon/status: expected ... kind2 "unknown_field", got {"id":7,...,"result":{...}}
wait: no reply (Resource temporarily unavailable (os error 11))
daemon/stop: expected ... kind2 "unknown_field", got {"id":7,"jsonrpc":"2.0","result":{"stopping":true}}
daemon/stop force not a boolean: no reply (Connection reset by peer (os error 104))
```

The `wait` case blocked because the unknown field was ignored and the
nonexistent turn was then waited on until the 5 s client timeout. `hello`,
`spawn` and `steer` refused the unknown member but without `kind2`.

**Fix.**

- `crates/via-cli/src/server.rs`: `parse_request` enforces the C1 §1
  JSON-RPC envelope:
  - The request must be an object with exactly `jsonrpc:"2.0"`, a string or
    integer `id`, a string `method` and optional `params`.
  - `params` must be an object when present; absent `params` is treated as
    `{}`.
  - Violations are `-32600 invalid_request`. The reply echoes the `id` only
    when it is valid; otherwise it is `null`.
  - A non-object `params` is `-32602 invalid_params`.

  Every method decodes its parameters through `typed::<Dto>()` into a
  `deny_unknown_fields` DTO. An unknown member is reported as `invalid_params`
  with `data.kind2: "unknown_field"` (C1 §8.1). A local `Refusal` wrapper
  carries `kind2`, so `ApiError` is unchanged.
- `crates/via-core/src/api.rs`: new request DTOs:
  - `SessionReadParams {session: SessionId}` for `events` and `logs`.
  - `DaemonStatusParams {}`.
  - `DaemonStopParams {force: bool = false}`.

  `result` and `wait` now use the existing `ReadParams`.

## Files changed

- `crates/via-cli/src/client.rs`: peer check and its isolated test.
- `crates/via-cli/src/server.rs`: envelope validation, typed dispatch and
  error `kind2`.
- `crates/via-core/src/api.rs`: request DTOs.
- `crates/via-cli/tests/c1_protocol.rs`: new end-to-end regressions.
- Outside my owned paths, kept minimal:
  - `crates/via-core/src/lib.rs`: re-exports the three new DTOs; the
    one-line `pub use` changed.
  - `crates/via-cli/Cargo.toml`: dev-dependency
    `rustix = {workspace, features = ["process","thread"]}` for the
    foreign-uid test listener. It adds no new crate and leaves `Cargo.lock`
    unchanged.

## Gate (Rust 1.98.1, `XDG_RUNTIME_DIR` set to a private 0700 directory)

| Step | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | pass |
| `cargo nextest run --locked --workspace` | 56 passed, 0 skipped (baseline was 52; 4 added) |
| `cargo deny check` | `advisories ok, bans ok, licenses ok, sources ok` |
| `python3 scripts/check-layers.py` | pass |

The prebuilt cargo-deny 0.18.4 could not parse the current advisory DB
(`unsupported CVSS version: 4.0`); version 0.20.2 passes. The advisory DB
also had to be cloned with the git CLI once, because the fetch built into
cargo-deny failed through this machine's proxy. Neither issue involves this
repository's code.

## Open or uncertain

- **Root-only end-to-end peer test.** Creating a listener with a different
  uid needs `CAP_SETUID`. The test passes without effect when not run as root
  (it cannot use `eprintln!` under the lint policy). On an unprivileged local
  gate, only the isolated `verified_peer` test runs; the cloud gate runs as
  root. macOS behaviour is untested here (Tokio's `peer_cred` supports it).
- **Not in T1-I3's scope, still open.** C1 §1 also requires JSON depth ≤ 64
  and ≤ 65,536 nodes per document before building an unbounded value.
  serde_json's default recursion limit is 128, and nodes are not counted.
- **Spec parameters rejected as unknown.** These are C1 parameters that the
  current code does not implement: `wait.timeout_ms`, `events`
  `turn/after/limit/follow/types`, `logs` `turn/after/limit` and
  `daemon/stop.drain`. They are now refused as `unknown_field` rather than
  silently ignored. Adding them belongs to the slices that implement them.
  W1-D owns the stop path; I changed only how `daemon/stop` reads `force`.
- **Wire field names.** `result` and `wait` keep the existing `address` field
  name. C1 §3.8 does not name the field.
- **`unknown_field` detection.** It matches serde's stable "unknown field"
  error prefix.
