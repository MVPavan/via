# VIA

One CLI to run an **explicitly configured agent + prompt** on coding harnesses
and get a **structured result** back. Roles are caller policy. The first
release targets Claude Code, Codex and OpenCode.

VIA gives one pattern for `spawn`, `resume`, `steer` and `cancel` across
harnesses. Each adapter declares which verbs it supports, and VIA refuses an
unsupported verb by name rather than faking it. The full first-release API
also includes `close`, `status`, `wait`, `result`, `list`, `events`, `logs`,
`describe`, `models`, and daemon management.

## Status

Development is paused at an incomplete S1 checkpoint. Partial CLI, daemon,
Store and fake-agent runtime tests are preserved; integration fixes and
acceptance remain open, and the current all-target build fails in the test
harness. Architecture and testing policy are approved. The three vendor
adapters and release qualification remain to be implemented.

- Current handoff and workflow: [docs/workstreams/rust-foundation/session-handoff.md](docs/workstreams/rust-foundation/session-handoff.md)
- Paused first-release goal: [docs/workstreams/rust-foundation/goal.md](docs/workstreams/rust-foundation/goal.md)
- S1 implementation plan: [docs/workstreams/rust-foundation/s1-plan.md](docs/workstreams/rust-foundation/s1-plan.md)
- Public API: [docs/specs/via-api-v1.md](docs/specs/via-api-v1.md)
- Brainstorm record and research: [docs/brainstorms/README.md](docs/brainstorms/README.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
