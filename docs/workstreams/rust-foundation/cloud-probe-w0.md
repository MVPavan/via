# W0 cloud probe: Claude Code cloud environment (Via-probe)

Investigation only; no source changes. Two probe sessions ran in the same
cloud environment on 2026-09-27. Part 2 ran on branch
`claude/w0-cloud-probe-part-2-78er2u`, identical to `origin/rust-foundation`
at `3f40425`. Part 1 ran at `ef5ba2d`; its results are recorded as reported
and were not re-run.

## Results

| # | Check | Source | Result |
|---|---|---|---|
| 1 | Machine | Part 1 | Ubuntu 24.04.4, kernel 6.18.44, 4 vCPU, 15 GiB RAM, no swap, ~30 GB disk, running as root. PID 1 is `process_api` (no systemd). Part 2 adds: PID 1 runs as `--firecracker-init`, so the session is a Firecracker microVM. |
| 2 | Network | Part 1 | `example.com`, `opencode.ai`, `models.dev`, `static.rust-lang.org` reachable (Full policy). `api.openai.com` returns 421 and `chatgpt.com` 403 from their own Cloudflare edge, not a proxy block. Egress goes through a TLS-intercepting gateway. |
| 3 | Rust gate at `ef5ba2d` | Part 1 | fmt, `cargo deny`, `check-layers` PASS. `cargo check` and nextest FAIL on `ScenarioError` Display at `crates/via-cli/tests/s1_prompt_to_result.rs:134` and `:676`. clippy FAIL on `clippy::never_loop` at `crates/via-routes/src/runtime.rs:141`. With the broken test target excluded, 49/49 tests pass. Each gate step takes 12 s or less, ~0.5 GB peak memory. Building cargo-deny from source takes 109 s. |
| 4 | musl static build | Part 1 | Needs `apt-get install musl-tools` and `CC_x86_64_unknown_linux_musl=musl-gcc`. Then builds in 51 s and produces a static-pie binary. |
| 4b | Claude child process | Part 1 | `claude -p` succeeds, including under `env -i` with a private `HOME`. |
| 4c | OpenCode child process | Part 1 | OpenCode 1.18.32 with `opencode/mimo-v2.6-flash-free` and a private `HOME`/XDG works. Hosts contacted: `opencode.ai`, `models.opencode.ai`, `registry.npmjs.org`. |
| 5 | Sandbox (bubblewrap) | Part 2 | **PASS.** `apt-get install -y bubblewrap` exit 0, bubblewrap 0.9.0. Each of these exits 0: `bwrap --ro-bind / / --dev /dev --proc /proc --unshare-all --die-with-parent true`, the same without `--unshare-*`, and with `--unshare-user` only. Under `--unshare-all` the sandbox sees only `lo` and runs as PID 2. It also works as uid 65534 via `setpriv`, so unprivileged user namespaces are available and root is not what makes it work. `/proc/sys/kernel/unprivileged_userns_clone` does not exist (a Debian/Ubuntu patch absent from this kernel). `/proc/sys/user/max_user_namespaces` = 64313. Seccomp is off for the session (`Seccomp: 0`); it runs with full capabilities as root. |
| 6 | Virtualization | Part 2 | **No KVM; Docker works.** `ls -l /dev/kvm` fails with "No such file or directory" (exit 2). `grep -c -E 'vmx\|svm' /proc/cpuinfo` prints 0 (exit 1), so nested virtualization is not available. Docker 29.3.1 is installed but not running. After starting `dockerd` manually, `docker run --rm hello-world` prints "Hello from Docker!" (exit 0; the image pulls through the gateway). Storage driver overlayfs, cgroup driver cgroupfs, **cgroup v1** (dockerd logs a v1 deprecation warning). |
| 7 | Processes and sockets | Part 2 | **PASS, with one caveat about reaping.** A Python 3.11 script started `sh -c 'sleep 300 & sleep 300 & wait'` with `start_new_session=True`. Setsid worked: child sid = pgid = pid, and it differs from the parent session. The group held 3 members. `os.killpg(pgid, SIGKILL)` killed all of them. The two grandchildren were re-parented to PID 1 and stayed briefly as zombies (`Z`, `<defunct>`) before PID 1 reaped them; no live process survived. A process-group check that uses `pgrep` right after the kill will count those zombies. `prctl(PR_SET_CHILD_SUBREAPER)` returns 0. A Unix domain socket under `/tmp` bound, connected and exchanged data, and `SO_PEERCRED` returned the right pid. `XDG_RUNTIME_DIR` is unset and `/run/user` is empty. Cgroups: `stat -fc %T /sys/fs/cgroup` prints `tmpfs`, meaning a v1 hierarchy. Its entries are `blkio cpu cpuacct cpuset devices freezer memory pids systemd unified`; the `unified` entry is a v2 mount alongside v1 (hybrid). The session sits at `/` in every controller. |

## Implications for the cloud/local split

- **S1 E2E tests (fake harnesses, process/socket behaviour): cloud.** Setsid,
  process-group kill and Unix sockets all work. Tests must not expect PID 1
  to reap orphans immediately: they should wait on their own children or
  make the daemon a child subreaper, and count only non-zombie processes.
  Tests must also not depend on `XDG_RUNTIME_DIR`; set it, or a private
  runtime directory, explicitly per test. That is also the right design
  locally. The S1 target itself was red at `ef5ba2d` (check 3), so fix it
  before relying on cloud runs.
- **bwrap-dependent OpenCode sandbox work: cloud is viable.** bubblewrap runs
  with full namespace isolation, unprivileged too. Two caveats: install it
  per session (setup script), and the session has no seccomp and cgroup v1
  only, so a sandbox design that relies on cgroup v2 delegation or on a
  host seccomp profile still needs a local or CI check.
- **Linux 5.15 platform qualification: local only.** The cloud kernel is
  6.18, and there is no `/dev/kvm` and no vmx/svm, so the 5.15 KVM runner
  (`via-pvj`) cannot run nested here. Docker runs, but containers share the
  6.18 kernel and give no kernel-baseline evidence. The cloud can still
  produce the static musl artifact (check 4) for the local runner to
  qualify.
- **Live Claude/OpenCode tests: cloud.** Both harnesses ran as isolated
  child processes (checks 4b, 4c). Codex live tests that need
  `api.openai.com`/`chatgpt.com` cannot run here, because those endpoints
  reject this egress (check 2); keep them local.
