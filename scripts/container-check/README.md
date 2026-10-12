# Container check

A release check from a new user's point of view, on Ubuntu 24.04. Inside the container:

- VIA is downloaded from a GitHub release and checksum-verified;
- Claude Code, Codex, OpenCode v2 and Pi come from their official installers;
- nothing comes from the developer's machine except logins.

Each harness then runs live through:

1. spawn and wait;
2. resume and wait;
3. a cancel during a shell command, and wait;
4. close.

At the end the check looks for leftover processes.

```bash
scripts/container-check/run.sh 0.1.1                 # all four harnesses
scripts/container-check/run.sh 0.1.1 codex opencode  # some of them
```

## What it mounts

The container needs the logins for Claude Code, Codex and Pi, so it mounts:

- `~/.claude`;
- `~/.claude.json`;
- `~/.codex`;
- VIA's private Pi directory, `~/.via/state/vendor/pi/agent`, where the Pi login lives ([Pi setup](../../docs/usage.md#pi-setup)).

OpenCode runs in VIA's private directories with free models, so it needs no login.

The mounts are read-write, so the container's Claude Code and Codex sessions land in your homes like any other session. VIA's own state is not mounted; it lives inside the container and is deleted with it.

## Models and output

The check uses the cheap live-test models: Claude Haiku, Codex `gpt-6-luna`, OpenCode `opencode/mimo-v2.6-flash-free` and Pi `openai/gpt-6-luna`.

The log is written to `container-check-<version>.log`, which is gitignored. A container shares the host's kernel, so this check does not test kernel versions.
