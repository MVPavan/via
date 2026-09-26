# Platform and packaging contract

Status: **owner-approved packaging scope, 2026-09-26; Linux priority;
macOS artifact production, linkage inspection and native qualification deferred
for the current goal**. Bead: `via-pvj.1.1`;
review: `via-pvj.1.2`. Owner accepted P-OWNER-1 and the coordinator recorded
delegation to make macOS work optional. The coordinator selected Linux as the
only required current-goal release target. Scoped Sol
review of this disposition remains required. No artifact or runtime pass is
claimed. Shared invariant/goal wording is for coordinator integration.

Authority: [invariants](../../.repo-context/invariants.md) #4, #7 and #8,
[goal](../workstreams/rust-foundation/goal.md),
[coding style](../../.repo-context/coding-style.md) §§6, 10 and
[C1](via-api-v1.md) P10. Rust remains pinned to **1.98.1**, edition 2024.

## 1. Owner disposition and exact common wording

**P-OWNER-1 accepted, 2026-09-26:** the owner approved the narrow macOS
packaging exception and prioritized Linux, delegating optional macOS work to
the coordinator. The coordinator chose to defer macOS artifact production,
linkage inspection and native qualification together for this goal. The owner
did not explicitly enumerate those artifact tasks; this is the coordinator's
choice within that delegation. This replaces
the earlier pending decision; the original review finding remains valid as the
reason an exception was necessary. It is permission, not execution evidence.

Proposed invariant #4 / common packaging wording for coordinator integration:

> One installed VIA executable contains the CLI and daemon. Linux artifacts
> are fully statically linked. macOS artifacts statically include VIA, Rust
> dependencies and SQLite, and may dynamically link only the Apple-provided
> system libraries explicitly allowed by this contract. No separately
> installed language runtime, VIA helper executable or third-party shared
> library is required.

Proposed current-goal qualification wording:

> Linux is the only required release target for this goal. Its fully static
> `x86_64-unknown-linux-musl` artifact, installation, actual Linux execution,
> kernel 5.15 baseline/current-kernel tests and all required platform/vendor
> gates remain mandatory. macOS packaging design and compatibility guidance
> are retained, but ARM64 artifact production, linkage inspection and native
> qualification are deferred together to a separate follow-up. Missing macOS
> artifacts/runners do not block this goal and are not passes. No Linux, WSL
> or cross-build result establishes macOS compatibility.

Vendor binaries remain external user-installed/authenticated prerequisites.
The macOS exception does not permit bundled third-party dylibs or broaden the
§3 allowlist. The deferred macOS follow-up must produce and inspect an artifact
before accepting its packaging, then complete native qualification before
claiming compatibility. Optional work performed sooner retains those same
checks; an uninspected/unexecuted candidate is never an accepted macOS release.
Do not describe a Mach-O artifact as fully static or runtime-qualified.

Apple's archived QA1118 says fully static macOS executables are unsupported;
dynamic system libraries are its supported compatibility boundary. Rust's Apple
target enables dynamic linking. This was a support/contract conflict, not proof
of mathematical impossibility; no fully static macOS build was demonstrated.
[Apple QA1118](https://developer.apple.com/library/archive/qa/qa1118/_index.html),
[Rust Apple target implementation](https://github.com/rust-lang/rust/blob/main/compiler/rustc_target/src/spec/base/apple/mod.rs).

The deferred macOS row reopens before advertising native macOS compatibility
or removing the unverified label, or when an appropriate native runner becomes
available and qualification is scheduled. Publication/signing authority remains
separate. No test result may be fabricated or converted from missing to pass.

## 2. Minimal first-release matrix

These are the selected VIA policy targets, not compiler minimums. Limit the
matrix to two architectures; Linux requires a complete native proof row now.
The macOS row is deferred design/compatibility guidance, not a required
current-goal artifact or qualification deliverable.

| Artifact target | VIA minimum / baseline execution | Linkage | Additional execution |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | Linux kernel 5.15; Ubuntu 22.04 x86-64 with a 5.15 kernel is the baseline test image | Fully static ELF; baseline x86-64 ISA, no `target-cpu=native` | Current supported x86-64 Linux distribution, recorded by exact image/version |
| `aarch64-apple-darwin` | macOS 13.0 on Apple Silicon; build with `MACOSX_DEPLOYMENT_TARGET=13.0` | Deferred artifact/inspection/native qualification; Mach-O uses the owner-approved §3 system-library exception | Current supported macOS on Apple Silicon, exact version recorded |

Intel macOS, ARM64 Linux, universal macOS archives, WSL and native Windows are
not first-release artifact targets. WSL product/integration qualification remains
later scope; WSL2 can provide Linux-kernel execution evidence under §7 without
creating a separate supported artifact or proving Windows integration. The
observed 6.6 kernel cannot close the 5.15 baseline or any macOS row.
The architecture restriction is explicit because the existing family-level
scope does not specify CPU coverage. Scoped review must confirm the owner disposition and unchanged packaging
checks before integration.

Linux 5.15 and macOS 13 are conservative proposed product floors, chosen to
bound the test matrix; neither floor is evidence of vendor compatibility.
Each adapter's pinned vendor version must run at that floor or name a higher
adapter-specific requirement in user documentation. All three adapters must
pass their small live sets on Linux on at least one supported OS version; the
platform fake-vendor suite must also pass on baseline Linux. The corresponding
macOS suites remain the deferred qualification inventory, not current-goal gates.
No claim that a vendor runs on every VIA-supported version follows from this.

Checked 2026-09-26: Rust documents Apple Silicon's compiler minimum as 11.0
and supports an explicit deployment target. The locally installed 1.98.1
reports 11.0 for ARM64 and 10.12 for Intel; these are compiler defaults, not
the proposed VIA floors. Rust lists both proposed target triples; listing a
target proves neither dependency compatibility nor VIA execution.
[Rust macOS support](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html),
[Rust target table](https://doc.rust-lang.org/rustc/platform-support.html).

## 3. Build, linkage and artifacts

These checks are current-goal requirements for Linux. macOS production,
linkage inspection, installation and execution checks belong to the deferred
follow-up; they remain the acceptance rules when that work resumes.

Build the CLI package with the pinned toolchain, committed lockfile, explicit
target and a dedicated clean output directory:

```text
cargo +1.98.1 build --locked --release -p via-cli --bin via --target <target> --no-default-features
```

The release feature allowlist is initially empty. A required production feature
must be explicitly reviewed and added to the build command; `--all-features`
is forbidden for shipping. Record compiler, Cargo, linker, C compiler, SDK,
target, build flags, feature resolution and source-tree identity. Bundled
SQLite needs a target-correct C toolchain; the current workspace declares
`rusqlite` with `bundled`, which must remain statically included. A musl Rust
target alone does not supply every native dependency's cross C compiler.

| Artifact | Required inspection and rejection rules |
|---|---|
| Linux | `file`, `readelf -h -l -d`: architecture is x86-64; no `PT_INTERP`, no `DT_NEEDED`; no runtime shared-library dependency. Static PIE is allowed. Do not use `ldd` as the decisive static-link test |
| macOS | `file`, `otool -L`, `otool -l`: architecture is ARM64; deployment minimum equals 13.0; direct dylib allowlist is exactly `/usr/lib/libSystem.B.dylib`, `/usr/lib/libiconv.2.dylib`, `/usr/lib/libresolv.9.dylib`. Entries may be absent; any additional entry blocks acceptance pending review |
| macOS loader | `/usr/lib/dyld` is the allowed system loader; no `LC_RPATH`, `@rpath`, `@loader_path`, `@executable_path`, Homebrew/build-tree paths or bundled dylibs. Apple's system libraries may themselves load Apple OS components; no non-Apple transitive library is permitted |

The small macOS list is the accepted allowance, not an observed link result;
final artifact inspection decides what is actually imported. In particular,
SQLite, OpenSSL, Rust `std` as a dylib and package-manager libraries must not
appear. Do not broaden the allowlist automatically to make a build pass.
Rust's linkage reference explains target-dependent C runtime linkage and why
static Rust dependencies alone are insufficient evidence.
[Rust linkage](https://doc.rust-lang.org/reference/linkage.html).

Produce `via-<version>-<target>.tar.gz` with one executable `via`, `LICENSE`
and a short installation/readme file; no absolute paths, traversal entries,
symlinks, setuid bits or runtime sidecars. Documentation/license files do not
become runtime requirements. Alongside the archive, retain a JSON manifest and
SHA-256 sums for both the archive and extracted executable. Manifest includes
source commit plus dirty diff hash when applicable, `Cargo.lock` hash, exact
toolchain/flags/features, target/floor, observed imports and signing state.
A dirty-tree local candidate is allowed if completely identified; it is not
a published release. Hash only after stripping/signing; no post-test mutation.

## 4. Install and execution contract

Install the extracted executable as `~/.local/bin/via` or another explicitly
chosen user-owned directory, mode `0755`, without privilege escalation, a
package manager, a system service or runtime download. Tests install into an
isolated directory and invoke that installed file, never a development-tree
fallback. Installation does not edit shell profiles or vendor login/config.
An upgrade stages the replacement in the destination directory and replaces
atomically; do not overwrite a running daemon silently. Existing C1 version
mismatch refusal remains mandatory, with an explicit drain/stop/restart path.
Uninstalling the binary preserves user state unless separately requested.

Preserve C1 P10: `$XDG_RUNTIME_DIR/via/via.sock`, otherwise
`~/.via/run/via.sock`; no Linux abstract-socket substitute on macOS. The
selected runtime directory must satisfy the same owner/type/mode checks on
both OSes. Reject an overlong encoded socket path with a useful error; do not
truncate it or fall back to a publicly accessible directory. State and logs
are user-only runtime data, never files bundled into the install archive.

For the local candidate, record macOS signature details and validate whatever
signature is present with `codesign --verify --strict`; a required ad-hoc
signature is applied before hashing. Developer ID signing, notarization and
download/Gatekeeper distribution are publication decisions outside this goal.
Local successful execution must not be represented as proof that an unsigned
download will pass Gatekeeper. Never disable Gatekeeper or remove quarantine
as an acceptance workaround.

## 5. Platform behavior and failure-first tests

Keep OS discovery/identity and vendor supervision in `via-host`; keep the
CLI's authorized daemon-start exception and peer checks at the client
boundary. Core sees verified identity/uncertain identity and lifecycle
results, not platform API details. Use approved safe wrappers: workspace
`unsafe_code = "forbid"` remains binding. If existing dependencies lack a
macOS API, resolve and review the narrow dependency before implementation;
shelling out to parse `ps` is not identity proof.

Linux peer identity uses kernel Unix-stream peer credentials (`SO_PEERCRED`);
macOS uses `getpeereid` or an equivalent documented safe wrapper. Both client
and daemon reject a peer whose effective uid differs before protocol traffic.
Failure to obtain credentials is a refusal, never permission to skip the check.
[Linux Unix sockets](https://man7.org/linux/man-pages/man7/unix.7.html),
[Apple getpeereid](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/getpeereid.3.html).

Linux anchor identity discovery obtains process start ticks/group and uid
from non-environment procfs metadata. macOS supplies anchor process metadata
through native APIs; it is not Linux `/proc` with different paths. Only a
verified **live anchor** can authorize an anchor-issued own-group cleanup.
Check its live control marker against its persisted marker and match uid,
start identity, boot identity, process group and generation as specified below.
Vendor child identity is separate observational evidence; the vendor
environment marker is launch data, never a recovery lookup or signal authority.
Neither a pid, executable name nor persisted marker proves current ownership.
Failed identity/peer checks, partial records or a disappearing anchor refuse
cleanup requests. An absent/unverified anchor supplies no signalling authority;
report orphan/uncertain cleanup unless the independent no-signal absence test
below proves the recorded group absent. Never inspect process environments or
vendor credentials to recover a marker.
[Linux procfs](https://www.kernel.org/doc/html/latest/filesystems/proc.html),
[Apple process-info definitions](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc_info.h).

Identity reads followed by `kill`/`killpg` are not automatically race-free.
The selected design candidate below uses an in-group Host anchor; repeated
snapshots alone are insufficient proof.
When that mechanism cannot be established after daemon death, report the
survivor and refrain from signalling. This conservative path does not satisfy
the positive recovered-survivor cleanup test by itself: platform acceptance
requires both positive verified cleanup and negative identity refusal.
Live child handles are retained/reaped; reused pids and unowned servers are
never signalled. Group escape remains outside containment: tests must preserve
`uncertain` cleanup when descendants escape, regardless of leader exit.

### 5.1 Reviewed-design candidate: same-binary Host group anchor

Disposition of independent review findings 1 and 2: select an internal Host
anchor process, implemented by the same `via` executable, for further design
review. This is a process-lifetime mechanism, not a second daemon, separate
installed helper or additional layer. The detailed S1 Host protocol belongs in
[runtime contracts](runtime-contracts.md); both packets must agree before code.
**This design packet supplies no execution proof; use actual scenario
evidence for Linux acceptance and keep native macOS qualification deferred.**

The floor rules out the proposed direct group-pidfd shortcut: Linux's
`PIDFD_SIGNAL_PROCESS_GROUP` starts at 6.9, whereas the supported floor here is
5.15. Checked 2026-09-26, rustix 1.1.5's safe `pidfd_send_signal` uses flags
zero and targets a process. Do not pass a numeric pgid through it, claim an
individual pidfd pins a group, invoke raw unsafe syscalls or raise the OS floor
silently. Linux pidfds may target the anchor individually, but platform
correctness does not depend on this Linux-only optional path.
[Linux pidfd signalling](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html),
[rustix API](https://docs.rs/rustix/1.1.5/rustix/process/fn.pidfd_send_signal.html).

The proposed ownership protocol is:

1. Before launch, the daemon persists anchor intent with a fresh generation
   and private marker. Host launches an internal anchor as the leader/member
   of a new private process group. It remains in that group until exit and owns the vendor
   child handle. It never joins another group. A separate private control
   socket carries only Host messages; Wire retains vendor pipe ownership.
2. The anchor reports its own identity before starting a vendor; the daemon
   verifies and persists it. Configure is accepted once
   on the bootstrap connection. The daemon then commits
   `ArmIntent {anchor_id, generation, expected_version}`: a Store transaction
   requiring the full identity and matching generation/version, setting
   `arm_intent`. Only a positively committed transaction permits exactly one
   `Arm {generation}` send. Failed/uncertain commit permits no ARM send; a lost
   ARM acknowledgement never permits a resend or second vendor spawn. Recovery
   loads generation and arm intent from Store and can only inspect/stop, never
   Configure or ARM. Pre-ARM EOF means no vendor; post-ARM EOF starts bounded
   cleanup. Tests distinguish commit-before-send from send-before-lost-ACK.
   After vendor spawn, the anchor safely redirects **all three of its own**
   vendor-facing descriptors (stdin/stdout/stderr) to an opened `/dev/null`
   using safe `rustix` stdio `dup2_*` wrappers before acknowledging spawn.
   Vendor descriptors remain inherited; Wire owns their daemon ends. The
   anchor retains only its child handle/control path for supervision, never
   reads vendor bytes, and fails closed into cleanup if detachment fails.
   A vendor exit must yield both output EOFs and observable input closure
   while the anchor is still alive, so Wire can seal the raw logs.
3. Marker proof uses this live control endpoint, never process environments.
   Both ends validate the peer uid; the daemon supplies a fresh challenge
   nonce, and the anchor replies with the nonce and its own resident marker
   and identity. The daemon does not supply the expected marker for the
   anchor to echo. A stale response, wrong marker, closed endpoint or failed
   peer/identity/generation check refuses the cleanup request. Marker/control data is
   private, bounded, absent from logs and never inherited by the vendor.
   This handles accidental/stale identity; it does not add a guarantee against
   malicious same-user processes, which C1 already excludes.
4. Verified cleanup requests run inside the anchor: it signals its **current
   own group**, using `kill(0, signal)` semantics through a reviewed safe API,
   while it is a member. There is no daemon-side recovered numeric `killpg`.
   Its live membership is the lifetime reference; if it dies before processing
   the request, no other process replays that numeric target. Both Linux and
   Apple's documented `kill` behavior provide the own-group operation.
5. Anchor TERM handling must keep it alive long enough to apply the bounded
   KILL escalation to its own group. KILL can terminate the anchor itself;
   missing final acknowledgement is uncertainty, not evidence of successful
   cleanup. Only the independent absence predicate below can subsequently
   settle group cleanup without that acknowledgement.
   Reap children while the anchor is alive; record forced anchor death and
   OS adoption of remaining children rather than inventing a reap result.
   If the anchor is missing, hung or unverified, it supplies no cleanup
   authority; never fall back to numeric group signalling.

[Linux own-group signalling](https://man7.org/linux/man-pages/man2/kill.2.html),
[Apple own-group signalling](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kill.2.html).

**Positive cleanup evidence after anchor death.** For a fully persisted
anchor identity in the same boot and PID namespace, with a checked pgid greater
than 1, Host may call safe `rustix::process::test_kill_process_group(pgid)`.
This implements `kill(-pgid, 0)`: an existence/permission check that sends
**no signal**. Only `ESRCH` establishes `GroupAbsent` at observation time.
Success, `EPERM`, any other error, namespace/boot mismatch or incomplete
identity remains `Uncertain`; never interpret inability to inspect as absence.
Numeric reuse can therefore delay proof, but cannot authorize a signal to the
new group. Repeated checks are bounded by the cleanup deadline.

A missing final acknowledgement remains uncertain until this independent
predicate succeeds. `GroupAbsent` establishes absence of the recorded group,
not who caused its exit, Host reaping, vendor acceptance or escaped descendant
quiescence. Neither leader exit, socket EOF nor successful TERM/KILL delivery
is sufficient. The no-signal numeric query is explicitly permitted for
observation only; nonzero numeric group signalling remains forbidden.
Linux and Apple document signal zero and `ESRCH` semantics in the preceding
manual links; the safe wrapper is verified in
[rustix signal source](https://docs.rs/rustix/latest/src/rustix/process/kill.rs.html).
Native tests must prove this on Linux now and on macOS when deferred
qualification resumes. Ordinary daemon-crash
positive P-I2/F22 acceptance is retained: autonomous EOF cleanup and verified
reconnect cleanup must each reach the absence predicate, while lost-ACK without
absence evidence stays uncertain.

This explicitly changes the Host supervision shape: anchor identity/marker
proof and vendor child identity are distinct records. The existing vendor
environment marker remains set as required, but is **not** read back as live
proof. Linux `/proc/<pid>/environ` and macOS `KERN_PROCARGS2` are forbidden
marker-discovery routes, even with filtering after reading: their result can
contain credential values. Apple process-info fields do not prove a marker.
No persisted marker is promoted into evidence that a process is currently live.

Required contract amendments for independent review, not applied by this
platform worker: C1 §7.5 must replace direct vendor marker discovery/group kill
with verified live-anchor own-group cleanup, separate vendor child evidence
and no-anchor uncertainty subject to the no-signal absence predicate. Coding
style §6 must allow signal initialization in the executable's internal Host
anchor entrypoint as well as daemon main, describe Host-owned anchors and
their child records, and distinguish normal/TERM Host reaping from OS adoption
after forced anchor death. The daemon reaps the anchor while alive; the anchor
reaps its vendor while alive. The vendor inherits the anchor's existing group:
do not have the daemon spawn a child into a separately looked-up numeric pgid,
which reintroduces a join/reuse race if the daemon and anchor die during spawn.
The one per-user Store-writing daemon, Wire's exclusive
vendor I/O, single installed binary and `unsafe_code = "forbid"` remain intact.
This design choice is within material-design authority; it is not a new owner
approval gate. Any proposed relaxation of safety or positive-cleanup acceptance
would separately require an explicit owner decision.

For Linux implementation-ready status, S1/Sol review must pin safe APIs for
own-group signalling, signal handling, control endpoint setup and native
identity metadata at the Linux floor; retain the macOS API design/candidate
without claiming native validation before its deferred qualification; distinguish the marker exchange from
authentication against an adversarial same user; and specify teardown when
the anchor dies or hangs. Native P-I2/P-I4 proof must include daemon death
before/after the committed arm intent and single ARM send, duplicate ARM,
stale generation/marker/challenge, anchor death between challenge and request,
lost cleanup acknowledgement with and without subsequent `ESRCH`, TERM-surviving
vendor, forced anchor death and unrelated sentinel liveness. Tests must also
prove all three descriptor copies detached before spawn ACK and EOF reaches
Wire while the anchor lives. No passing mock or prose
argument alone closes these tests. If the anchor mechanism fails review or
execution, keep P-I2/F22 incomplete and return the exact failing guarantee;
neither a Linux floor increase alone nor no-signal fallback resolves the
deferred macOS marker/cleanup requirements.

| ID | Required on native Linux now; deferred native macOS qualification | Decisive observation |
|---|---|---|
| P-S1 | Correct peer; wrong-uid peer in both directions; credential-query error | Valid peer can handshake; rejected peer receives/sends no C1 payload. Wrong-uid case uses two test accounts or an OS-supported isolated equivalent; mocks alone do not close it |
| P-S2 | Directory/socket symlinks, wrong owner/mode/type, permissive caller umask | Refusal without deleting/replacing foreign files; private socket is never exposed permissively during creation |
| P-S3 | Concurrent auto-start, daemon crash and stale socket; overlong UTF-8 path | One daemon/Store writer; stale cleanup only while holding stable lock; lock file not unlinked; long path named refusal |
| P-I1 | Owned child and in-group descendant; timed TERM/KILL escalation | Only owned processes signalled, children reaped, terminal result and cleanup match actual group observation |
| P-I2 | Ordinary daemon crash after committed arm intent and ARM; test both autonomous anchor EOF cleanup and verified reconnect cleanup | No turn/ARM resend; live anchor performs own-group cleanup; same-boot/namespace persisted group yields `ESRCH` in the no-signal absence query, including after anchor self-KILL. Lost ACK alone stays uncertain; fresh absence evidence settles only group cleanup |
| P-I3 | Wrong anchor uid/start/group/live marker/generation separately; stale boot/namespace; partial/missing identity; absent anchor; permission denial | Refuse anchor cleanup request and every numeric signal; unrelated sentinel remains alive. Existence-query success/`EPERM`/errors never become absence; vendor environment is never read |
| P-I4 | Crash before/after arm-intent commit and ARM send; duplicate/stale generation; anchor dies after challenge; descriptor-detach failure; vendor exits while anchor lives; reused pgid | Exactly one vendor at most, none before durable arm intent; failed detachment cleans up; both output EOFs/input closure observable before anchor exit. Lost ACK without `ESRCH` stays uncertain; no leader-death shortcut or signal to reused group; native stress complements deterministic seams |
| P-I5 | Descendant escapes group; shared server hosts two sessions | No false containment/quiescence claim; cancelling one session does not kill shared server or sibling work |
| P-A1 | Install archive on baseline and current OS; CLI/socket/stdio, daemon restart, fake spawn/resume/result and background/wait | Same extracted hash runs each surface; version handshake and state permissions correct; no missing loader/library/helper |

A native wrong-uid fixture may need a provisioned test account and runner
privilege unavailable to the agent. Record that as infrastructure failure,
not a skipped-pass. Do not create accounts or escalate privileges merely
because this design lists the required scenario.

## 6. Failpoint exclusion

Test-only failpoints require positive coverage in an explicitly separate test
build and proof of absence in each final release artifact. The reviewed S1
design owns the exact feature names and hook inventory; release packaging
consumes that inventory rather than creating another mechanism. Linux execution
checks remain current-goal requirements; macOS artifact production,
feature/linkage inspections and native behavioral checks are deferred together
and remain required for the future macOS qualification row.

1. Build release in a fresh output directory using the §3 command. Record
   resolved features for all normal/build dependencies; reject every test
   hook feature and dependency, including transitive activation. Merely using
   `--no-default-features` on `via-cli` is insufficient.
2. Require compile-time exclusion of hook parsing/dispatch, test transports,
   artificial clock controls and fault handlers. Reject a release build if a
   forbidden feature/configuration is enabled. Review source guards and actual
   rustc invocations; do not infer removal just from `debug_assertions`.
3. Use the positively exercised test build as a control. Against the final
   binary, attempt every inventoried hook via its exact environment/CLI/RPC
   activation input in an isolated fake scenario. Fault injection must be
   unavailable and normal behavior must hold; hidden RPC/CLI controls must
   refuse. Required fault checks must have nonzero scenario counts.
4. Inspect unstripped link/symbol evidence and the final binary for inventoried
   hook identifiers as supporting evidence. Absence of strings alone is not
   proof; stripping and optimization can erase names without removing behavior.
   A hook requiring only a different environment variable cannot ship dormant.

The evidence identifies separate test and release hashes. Never substitute a
failpoint-enabled binary for the published/local release candidate after its
feature check.

## 7. Required execution evidence and current availability

**Runner meaning and admissible evidence.** Native Linux execution means the
x86-64 target executable running under an actual x86-64 Linux kernel, not a
bare-metal requirement. A physical host or hardware-virtualized Linux guest
can qualify; CPU/syscall emulation cannot substitute for the platform row.
Concrete baseline: Ubuntu 22.04 x86-64 with kernel 5.15 in such a host/guest,
Linux filesystem storage, the exact extracted musl release hash and the
required test-account/namespace facilities. A supported current x86-64 Linux
distribution/kernel is the second row. Record both userland and actual kernel;
an Ubuntu 22.04 container on 6.6 does not test the 5.15 floor.

WSL2 executes a real Linux kernel in a managed VM, unlike WSL1 translation.
It may supply Linux execution evidence for the exact recorded kernel,
filesystem and available fixture facilities; do not discard those true tests.
It cannot establish a different kernel floor, Windows integration support,
macOS behavior or any fixture it cannot actually execute. The current local
WSL2 6.6.87.2 environment is eligible for current-kernel Linux checks once the
exact musl artifact and required fixtures exist; it is not the baseline 5.15
runner. This clarifies the earlier blanket “WSL development evidence” wording
without changing the selected Linux target/floor. The approved goal requires
actual target execution and excludes WSL as macOS proof; it does not impose
a physical-hardware-only Linux rule. [Microsoft WSL architecture](https://learn.microsoft.com/en-us/windows/wsl/compare-versions).

For the Linux target, run the full nonempty deterministic suite on
the native target OS, the P-S/P-I scenarios, P-A1 on baseline and current OS,
and release failpoint-exclusion checks. The macOS artifact-production,
inspection and native-execution row is explicitly deferred for the current
goal; record it as coordinator-deferred under owner delegation/unverified,
never pass or infrastructure-pass. Before macOS qualification, execute that
same inventory on its baseline and current native OS. Run the prescribed repository verification
gate as well; attach exact commands, exits and scenario counts. Cross-compiling,
emulation, Rosetta, a target's Rust tier and a green `--version` alone do not
replace native OS/architecture execution. Containers do not supply an older
kernel; baseline Linux proof needs an actual 5.15 guest/host kernel.

Retain coding-style §10 artifacts: `summary.json`, per-scenario raw/event/log
evidence, consistent SQLite backup, verified SHA-256 manifest and `REPORT.md`.
Record OS/kernel/CPU, baseline/current designation, SDK/linker, fixture/vendor
versions, source/lock/feature identity, exact tested binary hash and test-account
capability. Re-hash the installed binary; artifact mismatch invalidates the row.
Classify pass, fail, timeout and infrastructure failure separately. Missing
Linux runners, accounts, credentials or quotas keep the corresponding current
gate open. Missing macOS artifacts, linkage inspection or execution remain
explicit deferred limitations; none blocks this goal under the coordinator's
owner-authorized scope choice.

Checked local inventory, 2026-09-26: x86-64 WSL2, Linux kernel 6.6.87.2,
Ubuntu 24.04.3; Rust/Cargo 1.98.1; only `x86_64-unknown-linux-gnu` std installed.
`cc`, `readelf`, `objdump`, Docker and GitHub CLI executables are present;
musl C tooling, Apple tooling and local QEMU executables were not found.
No repository CI workflow was present. Command presence proves neither daemon
access nor remote runner availability. No auth/config/credential files or
remote runner inventory were read. This inventory is historical availability
evidence, not a fresh toolchain check or artifact test. It establishes a real
Linux-kernel execution environment at the recorded version, not baseline or
release qualification. Native macOS capability is not demonstrated. Musl
std/C tooling availability must be refreshed before the build; no current
artifact/linkage/runtime pass is inferred from this inventory.

Coordinator handoff: integrate the owner-approved wording in §1 after scoped
review, and identify native Linux runner access with baseline/current images
and wrong-uid fixture capability. Retain the macOS unverified label and deferred
qualification inventory. No remote provisioning, publication or spend is
authorized by this candidate. Shared Rust/Linux implementation and test design
continue; no new owner decision is needed for the accepted packaging exception.

## 8. Exact goal and tracking synchronization proposal

Coordinator owns these shared edits and Beads changes; none was applied by
this platform worker. Keep invariant #7's Linux/macOS family intent and retain
§1's accepted narrow linkage exception in invariant #4.

| Shared location | Exact replacement / action |
|---|---|
| Goal current checkpoint, platform/password sentences | “The owner approved P-OWNER-1 and delegated optional macOS work; the coordinator selected Linux as the only required current-goal release target. macOS artifact production, linkage inspection and native qualification are deferred together. OpenCode's generated-password inheritance has an authorized temporary exception; selected-control tests and successful free-model live qualification remain open. These dispositions are not execution/security passes.” Preserve unrelated S1/live tracking facts. |
| Goal finish: Platforms and packaging | “Reviewed Linux target/linkage/install contract; produced fully static x86_64-unknown-linux-musl artifact; actual target execution at the 5.15 baseline and current Linux configuration; platform socket/process/install tests and release failpoint exclusion. macOS design/compatibility guidance retained; macOS artifact production, linkage inspection and native qualification deferred to a follow-up, not finish gates.” |
| Goal finish: Three real adapters | Retain all three adapters and controls; add “OpenCode successful result and conversation continuity must use a free model; no paid-model/provider substitution without a new owner decision.” |
| Goal finish: Verification / Evidence and docs | Qualify target-artifact/native-platform requirements as current-goal Linux requirements; require explicit deferred/unverified macOS reporting. Keep every code, live, evidence and independent-review gate otherwise unchanged. |
| Goal platform paragraph | Replace its pending-owner language with §1's current-goal wording; retain Linux5.15, exact target, static requirement, macOS allowlist guidance and truthful WSL evidence under §7. |
| Goal unavailable-runner / blocked paragraph | “Unavailable required Linux runners, fixtures or free-model/provider access leave their corresponding current-goal gates incomplete. Missing macOS artifacts/runners are recorded in the deferred follow-up and do not block this goal. Never count infrastructure failure or deferred work as a pass; all remaining current-goal gates must hold before completion.” |
| Goal finish: Tracking and handoff | Keep all in-scope leaves/evidence; explicitly move macOS production/inspection/native acceptance to a separately linked deferred task rather than leaving mixed leaves as hidden current-goal blockers. |

Task mapping selected by the coordinator (recorded here; this worker made no
Beads changes):

- `via-pvj.2` and `via-pvj.3`: retain the existing applicable Linux packaging,
  static/linkage/install/failpoint and native socket/process acceptance in the
  current goal. Preserve true prior evidence and the 5.15/current-kernel rows.
- `via-pvj.4` (deferred macOS qualification follow-up): own
  ARM64/macOS13 artifact/toolchain production, exact Apple allowlist inspection,
  archive/signature/install evidence, baseline/current native P-S/P-I/P-A and
  failpoint tests, plus three-adapter live qualification. Record dependencies
  on the retained design and a suitable runner/toolchain. It is not a blocking
  dependency of current Linux goal completion and may not be closed as passed
  merely because it was deferred.
- `via-4sw.1.2` and OpenCode live qualification leaves: require successful free
  model result, two-turn continuity, supported controls/usage; paid substitution
  needs new owner authority. The observed403/timeout remain failed infrastructure
  evidence. `via-4sw.4` retains generated-password child-env hardening follow-up.

Before synchronization is complete, search all goal finish, scope, checkpoint,
runner and blocked language for any remaining mandatory macOS production or
execution wording. Deferred packaging is not an implicit delivered artifact.
