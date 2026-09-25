# VIA

One CLI to run a **role + prompt** on any coding harness (Claude Code, Codex,
OpenCode, and others) and get a **structured result** back.

VIA gives one pattern for `spawn`, `resume`, `steer` and `cancel` across
harnesses. Each adapter declares which verbs it supports, and VIA refuses an
unsupported verb by name rather than faking it. A passthrough mode forwards
native arguments unchanged; its results are marked unstructured.

## Status

Exploration. Nothing is built yet.

- Workstream handoff: [docs/workstreams/handoff.md](docs/workstreams/handoff.md)
- Brainstorm record and research: [docs/brainstorms/README.md](docs/brainstorms/README.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
