# VIA

VIA runs a prompt on a coding-agent harness and gives you back one structured
JSON result, whichever harness ran it.

It is one static Rust binary: a CLI plus a per-user daemon that the CLI starts
on demand. You name the harness, model, prompt and sandbox bound explicitly;
VIA launches the vendor's own CLI with your existing login, tracks the
session, and returns a result envelope with the final text, state, stop
reason, token usage, cost and warnings. Roles and orchestration policy stay
with the caller.

Why: every coding agent has its own flags, session model, cancel behaviour
and output format. VIA gives one pattern (`spawn`, `resume`, `cancel`,
`close`, `wait`, `result`, ...) across all of them. Each route declares what
it supports, and VIA refuses an unsupported verb or bound by name instead of
faking it.

## Supported harnesses

| Harness | `--harness` | Route | Model names |
|---|---|---|---|
| Claude Code | `claude` | `claude-cli` | `sonnet`, `opus`, `haiku` or a Claude model ID |
| Codex | `codex` | `codex-app-server` | a Codex model name, e.g. `gpt-6-luna` |
| OpenCode | `opencode` | `opencode-serve` | `provider/id` |
| Pi | `pi` | `pi-rpc` | `provider/id`, e.g. `openai/gpt-6-luna` |

`steer` is not supported by any route in this release. Linux x86_64 only.

## Install

Requires the Rust toolchain pinned in `rust-toolchain.toml` (installed
automatically by rustup).

```bash
cargo build --release --locked
# binary: target/release/via
```

A fully static binary (musl) is described in the [user guide](docs/usage.md#install).
Install and log in to each harness CLI you want to use; VIA never handles
credentials.

## Quick start

```bash
umask 077
# Start a turn in the background; the receipt carries the session handle.
via spawn --harness claude --model haiku --bound full --network --cwd . \
  --prompt "List the files in this directory and summarise the README." \
  --background > receipt.json
jq -r .handle receipt.json > handle      # needed for resume/cancel/close
TURN=$(jq -r .turn receipt.json)          # e.g. s_7f3k9q2mzr4c/1
SESSION=$(jq -r .session_id receipt.json)

# Wait up to 10 minutes for the result envelope.
via wait "$TURN" --timeout-ms 600000 | jq '{state, stop_reason, final_text, usage, cost}'

# Ask a follow-up in the same session, then close it.
TURN2=$(via resume "$SESSION" --handle-file handle --prompt "Now count them." | jq -r .turn)
via wait "$TURN2" --timeout-ms 600000 | jq -r .final_text
via close "$SESSION" --handle-file handle
```

Use `via describe --harness <h> --model <m> --bound full --network` to see
what a route supports before running anything; it starts no agent.

## Documentation

- [User guide](docs/usage.md): every verb, models, envelope, exit codes,
  warnings, errors and known limitations.
- [Public API (C1)](docs/specs/via-api-v1.md): the JSON-RPC contract the CLI
  speaks.
- [Design record](docs/brainstorms/README.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
