# S1 internal runtime contracts

Status: **design for Sol-high review**, 2026-09-26; Bead `via-jm4.7.1`.
These are implementation decisions proposed within the approved S1 scope,
not measured runtime results. Dependent code waits for the review gate.
Amended 2026-09-30 by the adapter design's AR1 and AR3–AR6 and its AD20
report delivery ([adapter design](../workstreams/rust-foundation/adapters/design.md)
§3.5, revision 9); leftover detection (AR2) is pending an owner decision
(adapter design, conflict 4).
Authority: [S1 plan](../workstreams/rust-foundation/s1-plan.md) §4,
[C1](via-api-v1.md), [C2](adapter-contract.md),
[invariants](../../.repo-context/invariants.md), and
[coding standard](../../.repo-context/coding-style.md).

## 1. Scope and guarantees

Implement only the fake adapter's private process, one child per turn,
NDJSON route, pipes, Linux supervision, SQLite and per-turn evidence folders.
The fake session preserves a synthetic conversation identifier between child
processes. Fake declares spawn/resume/result/cancel/close native and steer
unsupported; S1 tests the named steer refusal. It does not establish vendor
conversation continuity. No generic
RPC router, shared-server registry, HTTP/SSE implementation, plugin system or
new crate is needed for these contracts. Vendor slices extend typed C3
families when their evidence requires it.

Core is the only lifecycle authority. A receipt means the queued work and
retry identity committed. Submission intent means input might have reached
the agent; neither a timeout nor missing acceptance permits automatic resend.
An event/result means its transaction committed.

Three limits on these guarantees must remain visible:

1. Persistent storage failure may prevent a terminal result from being saved.
   Return `store_error`, close admission and clean up; never manufacture a
   durable envelope. Restart reconciles the last durable facts to `unknown`.
2. A blocked peer cannot be guaranteed a reply. The daemon closes the socket
   after the 10 s reply deadline; the caller retries the read.
3. Group signalling covers processes that remain in the VIA-owned group.
   Escaped descendants and uninterruptible kernel waits prevent an absolute
   all-descendants-gone guarantee. Report uncertainty, never false quiescence.

## 2. Owners, types and task lifetime

The dependency graph stays exactly as enforced by
`scripts/check-layers.py`:

```text
CLI -> Core -> Adapters -> Routes -> Wire -> Host
         |                           |       |
         +-------------> Store <-----+-------+
```

Public lower-layer contract types are re-exported through the immediate
parent's public facade where needed. Re-exporting is not permission to give
Core a process or pipe handle. Types an adapter produces (capabilities,
route plan, refusal, observations, class hints) are `via-adapters` DTOs;
Core keeps C1 request, envelope and event DTOs and converts them.
Store defines storage DTOs and shared durable IDs (`SessionId`, `TurnNumber`,
`ConnectionId`, `ProcessId`). These types contain no Core dependency
or business transitions. Core serializes validated C1 payloads into bounded
Store documents; Store validates storage constraints, not vendor semantics.
The adapters' facade re-exports the same lower-layer identity types, not new
incompatible copies. No dependency edge or extra shared-types crate is added.

Each owner has a cancellation token and a `JoinSet`; spawned tasks return
typed outcomes and are collected promptly. A parent observes child failure
through a reserved health path even when normal observations are full.
`TaskTracker` alone is insufficient unless each task also reports its result.
The SQLite thread has a retained join handle. Dropping a requester or
timing out a response does not cancel an already admitted mutation.
While the daemon lives, no caller deadline or cancelled shutdown future
abandons an owner: unfinished joins stay in the owner's registry until their
result is collected. Final daemon process exit is a separate boundary with
its own clean/incomplete policy (§6.2).

| Owner | Resources and hidden complexity | Explicit upper-layer surface |
|---|---|---|
| Core | Session actors, dispatch slots, monotonic timers, seq/state decisions, progress snapshots | C1 request, envelope and event DTOs |
| Adapter | Per-harness protocol mapping and normalizer; C2 observation queue | `via-adapters` DTOs (capabilities, route plan, refusal, observations, class hints), turn end results, health |
| Route | Fake protocol parsing, start correlation, command serialization | Typed fake requests/messages |
| Wire | Pipe reader/writer tasks, byte message splitting, evidence files | Vendor messages, transport health |
| Host | Child handle, process group/identity, reap and escalation timers | Exclusive pipe endpoints, verified exit/cleanup |
| Store | SQLite connection/thread, evidence root, migration and backup; Core retains sole owner | `StoreClient` for Core lifecycle/reads; unopened `RuntimeResources` forwarded to Wire |

Core alone opens and retains one Store owner and its `StoreClient`. It obtains
one unopened, non-cloneable `RuntimeResources` bundle from that owner and
passes it through Adapter and Route constructors without splitting it or
calling Store again. Only Wire bootstrap consumes the bundle via
`into_wire_parts(self)`, privately retaining `EvidenceRoot` and moving the
restricted `ProcessJournal` to Host. Rust does not enforce caller-specific
visibility across crates; Wire-only split is an architectural no-call rule,
not a claim that arbitrary Core code cannot misuse its Store dependency.
Bundle construction clones bounded senders only: no second Store writer,
database connection, worker, path reopen or I/O. Adapter/Route have no
operational journal access.
`SessionCx` contains a clone of the opaque runtime, limits, tracker/token and
absolute deadlines; remove its exposed `RawLogHandle` sketch (C2 amendment
in §10). A session driver must not be able to query SQLite or handle hashes.

Contract sketches below omit only ordinary DTO fields enumerated in the text.
Methods implemented asynchronously use `fn -> impl Future + Send`; handles
own bounded command senders, not resource mutexes held across `.await`.
All `Deadline` values wrap `tokio::time::Instant`; wall timestamps are separate.
Task 1 construction uses one Wire-defined `RuntimeConfig` (`anchor_binary`,
`anchor_dir`), re-exported through Route/Adapter; per-harness and fake
fixture settings stay Adapter-owned in `AdapterConfig` (§11.1). The
constructor chain is
`AdapterSet::new(config, runtime_config, resources)` →
`FakeRoute::new(runtime_config, resources)` →
`WireRuntime::new(runtime_config, resources)` →
`Host::new(journal, anchor_binary, anchor_dir)`. Only Wire splits resources.
Forwarding constructors validate config but start no vendor/anchor process
and submit no prompt. Core supplies canonical IDs, prompt and deadline to
Adapter; Adapter builds the private process/fake start from immutable fixture
config and returns C2 evidence, not a committed turn/envelope. Existing
acceptance/observation correlation remains required.

## 3. C3: fake typed route

```rust
pub struct FakeRoute { /* private owned WireRuntime and paired start state */ }
pub struct RouteOpen { pub process: PrivateProcessSpec, pub deadline: Deadline }
pub struct RouteSession { pub route: FakeRoute, pub messages: RouteMessages }
pub enum SendOutcome { Written, NotWritten, Indeterminate }
pub enum RouteHealth { Open, Failed(RouteError), Closed }

impl FakeRoute {
    pub fn new(config: RuntimeConfig, resources: RuntimeResources)
        -> Result<Self, RouteError>;
    // Task 1 entrypoint; C2 still owns acceptance/observation delivery.
    // The per-turn stop watch carries cancel, close and the idle deadline.
    pub fn execute(&self, connection_id: ConnectionId, process: PrivateProcessSpec,
                   start: FakeStart, deadline: Deadline, stop: StopWatch)
        -> impl Future<Output = Result<FakeRouteResult, RouteError>> + Send;
    pub fn open(spec: RouteOpen, cx: RouteCx)
        -> impl Future<Output = Result<RouteSession, RouteError>> + Send;
    pub fn start(&self, input: FakeStart, deadline: Deadline)
        -> impl Future<Output = Result<FakeAcceptance, RouteError>> + Send;
    pub fn close(&self, request: CloseRequest)
        -> impl Future<Output = CloseReport> + Send;
    pub fn health(&self) -> RouteHealth;
}
pub struct RouteMessage { pub payload: FakeMessage }
```

The route result and each server route's close and loss paths carry Host's
`CloseReport.leftovers` (§5) upward unchanged, to the adapter's `TurnEnd` or
driver `CloseReport` (C2 §4.2).

`FakeStart` contains synthetic vendor session/turn IDs and prompt; the fake
fixture validates these and emits an explicit acceptance message before
progress/terminal messages. `FakeMessage` is an exhaustive enum: acceptance,
text, terminal, tool-start/tool-end, interrupt acknowledgement and bounded
unknown notification. Known malformed messages become protocol errors. The
test-only scenario selection is separate from the prompt. No shell snippets
or untyped universal method map cross C3.

`FakeRoute::execute` with a per-turn stop watch supersedes the separate
`interrupt` entrypoint (amendment A12 in the Task 3 design).

There is one start awaiting acceptance per private connection. Route assigns
the protocol request ID and pairs a response once; unsolicited or duplicate
acceptance is a protocol failure. Start's response and its corresponding
observation carry the same correlation token; Adapter emits acceptance once
before subsequent turn observations. Core deduplicates this token if the C2
reply races the observation. Terminal messages cannot overtake earlier data
observations. A typed control acknowledgement can bypass the data queue, but
cannot commit a terminal envelope ahead of the earlier data.

Open is called only after Core has committed submission intent. It starts
the per-turn process and performs transport setup, never submits a prompt.
The fake adapter allocates its synthetic vendor session ID without child I/O.
A cancelled `start` future removes its local waiter, never resends input.
Once any bytes may have been written, timeout/loss is `Indeterminate` and
C2 `Unknown`. Only a definite protocol refusal can claim vendor rejection;
`NotWritten` is transport evidence, not permission to redispatch a turn whose
intent already committed.

Adapter command admission has separate data and control lanes (see §8).
Route control and health handling do not await the normalizer. A control
write can follow an in-progress input message but cannot interleave its bytes. If the
peer does not read stdin, Host escalation stays available without stdin.
Route errors preserve `Protocol`, `TransportLost`, `ProcessExited`, `Overflow`
and `Store` causes and evidence; Core alone selects C1 disposition.
For this one-child-per-turn fake route, decoding its terminal message ends
start/control admission on that connection and immediately requests Wire's
`close_input` (§4), before awaiting Core's durable result, process exit or
stdout EOF. Output drains remain open. A duplicate
`StartTurn` reaching Route is refused before any bytes are written;
already accepted input is never replayed. This finite input lifetime lets
the fake independently validate that no second start was sent. No fake
protocol tag or generic vendor finalization rule is added.

### 3.1 Private fake wire v1

The S1 fake uses UTF-8 NDJSON, one JSON object per line. There is no JSON-RPC
wrapper or `method`/`params` nesting. Every message requires a `type` tag.
Route encodes requests and parses vendor responses; the fake validates
requests. Known response types ignore extra fields but reject missing or
wrong-type required fields. An unknown notification tag without `id` is
activity only. An unknown tag carrying `id` is an
unexpected protocol message and fails parsing: this private protocol defines
no fake-to-VIA requests to answer. §4 message splitting and §8
structure/payload limits still apply.

One child/connection serves one turn. Request IDs are positive JSON integers:
`start.id = 1`, `interrupt.id = 2`. A repeated logical cancel coalesces to
the same pending interrupt and sends no second interrupt. There is no second
start on the connection. `session_id` is the adapter's stored synthetic vendor
session ID, reused on a new connection for resume. `turn` is the positive
canonical turn number. Vendor turn ID is exactly `fake-turn-` followed by
decimal `turn` without leading zeros; it is scoped to the fake session and
connection, not a global routing key.

| Direction | Exact required message fields |
|---|---|
| VIA → fake | `{"type":"start","id":1,"session_id":"s_example","turn":1,"prompt":"hello"}` |
| fake → VIA | `{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}` |
| fake → VIA | `{"type":"text","vendor_turn_id":"fake-turn-1","text":"reply"}` |
| fake → VIA | `{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"reply","stop_reason":"end_turn"}` |
| VIA → fake | `{"type":"interrupt","id":2,"vendor_turn_id":"fake-turn-1"}` |
| fake → VIA | `{"type":"interrupt_ack","id":2,"vendor_turn_id":"fake-turn-1"}` |

`s_example` illustrates the field only; it is not a valid C1 session ID or
required fake ID. `terminal.status` is `completed`, `interrupted` or `failed`.
`stop_reason` is a required string; fixture values for those statuses are
`end_turn`, `cancelled` and `vendor_error`, respectively. `final_text` is a
required string, possibly empty. Optional string `vendor_code` on a failed
terminal is retained as vendor evidence but does not select Core's final
class. The tool variants in §3 use two more tags when needed:
`tool_started {vendor_turn_id, tool_id, name, input_summary}` and
`tool_ended {vendor_turn_id, tool_id, status, output_summary, exit_code?}`.
All these fields are strings except optional signed-integer `exit_code`;
tool status is `completed`, `failed` or `cancelled`. VIA reads only
`vendor_turn_id` from `text`, and `tool_id` and `name` from the tool
messages; the other fields are optional and not read. A fourth progress tag
is `{"type":"usage","vendor_turn_id":"fake-turn-1","total_tokens":120}` (a
non-negative integer): one interval sample. These map to C2 `progress` items
(C2 §4), not C1 events.

Acceptance must match request ID and the derived vendor turn ID. Progress,
terminal and interrupt evidence must match the connection's turn ID. Driver
rejects duplicate acceptance or terminal, wrong IDs and terminal before
acceptance as protocol errors. `interrupt_ack` confirms command receipt only:
it cannot commit cancelled, acknowledged or quiescent by itself. A fixture
emits `terminal.status = interrupted` for protocol cancellation evidence;
Host group evidence determines cleanup. Interrupt may run while start awaits
acceptance. An early interrupt ACK is paired as control evidence without
inventing turn acceptance. A race fixture emits accepted before interrupted
terminal, or leaves Host/loss to resolve the unaccepted start. Unknown or
rejected submission is never automatically resent.

`text` is incremental; terminal `final_text` is authoritative final output,
not appended again to chunks. Fake steer is unsupported and has no wire
request. Close uses existing process cleanup, not a fake protocol message.
This seam adds no public C1 method or test CLI surface.

On a normal fixture, the fake emits its terminal message **before** waiting
for finalization, then drains and validates all remaining stdin bytes through
EOF within 2 s. It exits zero only after EOF with no protocol violation.
A second start, duplicate/invalid interrupt, unknown request, partial final
message or timeout gives a named diagnostic and nonzero exit. A permitted id-2
interrupt racing terminal is consumed and validated even if no longer
actionable; it cannot trigger another terminal or cancel completed work.
The fake retains one parser/buffer for its entire input lifetime, so read-
ahead cannot hide a second line. A bounded reader thread is optional, but
its completion/error belongs to normal finalization; a detached blocked
reader is not success. Explicit exit/crash/hang fixture steps remain fault
paths, not successful normal finalization. Existing input caps still apply.

## 4. C4: message splitting, evidence folder and transport

```rust
pub struct WireConnection { /* exclusive pipe/task ownership */ }
pub struct WireRuntime { evidence: EvidenceRoot, host: Host }
pub struct RuntimeConfig { pub anchor_binary: PathBuf, pub anchor_dir: PathBuf }
pub struct WireParts { pub sender: WireSender, pub messages: WireMessages }
pub struct VendorMessage { pub bytes: BoundedBytes }
pub enum WireHealth {
    Open,
    Failed { cause: WireFailure },
    Exited(ExitReport),
    Closed,
}
impl WireRuntime {
    pub fn new(config: RuntimeConfig, resources: RuntimeResources)
        -> Result<Self, WireError>;
    pub fn open_connection(&self, connection_id: ConnectionId,
                           spec: PrivateProcessSpec, evidence: EvidenceFolder,
                           deadline: Deadline)
        -> impl Future<Output = Result<WireConnection, WireError>> + Send;
}
impl WireConnection {
    pub fn into_parts(self) -> WireParts;
}
impl WireSender {
    pub fn write(&self, message: InputMessage, deadline: Deadline)
        -> impl Future<Output = Result<SendOutcome, WireError>> + Send;
    pub fn close_input(&self, deadline: Deadline)
        -> impl Future<Output = Result<(), WireError>> + Send;
    pub fn close(&self, request: CloseRequest)
        -> impl Future<Output = CloseReport> + Send;
}
impl WireMessages {
    pub fn next_message(&mut self)
        -> impl Future<Output = Result<Option<VendorMessage>, WireError>> + Send;
}
```

Wire's close report (`WireCloseReport`) carries Host's `leftovers` unchanged
(§5, C2 §4.2).

The clonable sender/control handle and unique message receiver allow reads and
control writes concurrently without borrowing one object mutably twice.
`WireRuntime::open_connection` creates the turn's evidence folder
and invokes Host acquisition. Direct `WireConnection::open(&Host, spec,
deadline)` is private to Wire; `WireConnection::control()` is
private or removed. No public runtime/connection getter or facade re-export
exposes Host, ProcessControl or ProcessJournal. Route
receives only narrow transport, close/control and health operations. Recovery
follows Adapter → Route → Wire → Host with passive owner/session correlation,
without exposing journal or Host getters; the separately reviewed outer
test-supervisor identity snapshot/cleanup remains unchanged.
`close_input` is idempotent, closes only vendor stdin and acknowledges only
after its endpoint is dropped. The one stdin writer settles any previously
admitted complete input message within the remaining 2 s finalization
budget, then drops the endpoint. It starts no new write after close request
and never interleaves bytes. A partial write that cannot finish closes input
and reports the existing indeterminate transport condition; it never reuses
the connection or extends the deadline. This operation does not request
Host group cleanup, stop output readers or fabricate a
successful send. The future C4 owner implements it with the real WireSender;
the frozen contract-only increment needs no unimplemented stub.
One task drains stdout. It never waits for Route, Core or SQLite. It
splits bytes into vendor messages at LF, at most 1 MiB each including LF,
and queues each with a nonblocking send; a full queue fails the connection
`overflow`. At the cap without LF it fails `MessageTooLarge`. After a
failure it reads to EOF in 64 KiB units and discards them, counting the
bytes, so the vendor never blocks on a full pipe. The splitter retains
split UTF-8 without interpreting it; only Route decodes UTF-8/JSON. EOF
with an unfinished message is the in-band end `Unterminated`.

VIA keeps no copy of vendor traffic (T4 requirements R8). Each submitted
turn has an evidence folder, `<state>/evidence/<session_id>/<turn>/`. The
vendor's stderr is the file `stderr.log` there: Host opens it and gives
it to the anchor as stderr, the vendor inherits it, and the operating
system writes it; no VIA task reads it. When Route cannot decode a
message, and when a message exceeds 1 MiB or ends unterminated, Wire
writes its first 64 KiB to `undecoded.bin`, and the turn's failure names
the file and the message's length. A final text too long for the
envelope is written there as `final_text.txt` (C1 §5). The vendor's stderr
is not capped. The vendor's own transcript keeps the
conversation; SQLite keeps its path as a hint with the vendor session ID.

## 5. C5: private process supervision

```rust
pub struct PrivateProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: EnvAllowList,
    pub owner: ProcessOwner,
}
pub struct AcquiredProcess {
    pub pipes: OwnedPipes,       // moved once into Wire; Host never reads them
    pub control: ProcessControl,
    pub exits: ExitReceiver,
}
pub struct ProcessIdentity {
    pub pid: u32, pub pgid: u32, pub uid: u32,
    pub boot_id: String, pub pid_namespace: PidNamespaceId,
    pub start_ticks: u64, pub marker: ProcessMarker,
}
pub enum CleanupEvidence { GroupAbsent(GroupAbsenceProof), Uncertain(CleanupReason) }
pub struct CloseRequest { pub mode: CloseMode, pub deadline: Deadline }
impl Host {
    pub fn new(journal: ProcessJournal, anchor_binary: PathBuf, anchor_dir: PathBuf)
        -> Result<Self, HostError>;
    pub fn acquire(&self, spec: PrivateProcessSpec, deadline: Deadline)
        -> impl Future<Output = Result<AcquiredProcess, HostError>> + Send;
    pub fn recover(&self, intents: Vec<ProcessIntent>, deadline: Deadline)
        -> impl Future<Output = Vec<RecoveryReport>> + Send;
    /// Returned on every path, including an expired deadline or journal failure.
    pub fn shutdown(&self, deadline: Deadline) -> impl Future<Output = ShutdownReport> + Send;
}
pub struct ShutdownReport {
    pub recovery: Vec<RecoveryReport>, // facts established before any failure
    pub pending_tasks: usize,          // retained tasks whose result was not collected
    pub failed_tasks: usize,           // panicked/cancelled tasks and failed child waits
    pub failure: Option<HostError>,    // named deadline, Store or recovery failure
}
impl ProcessControl {
    pub fn close(&self, request: CloseRequest)
        -> impl Future<Output = CloseReport> + Send;
}
```

Host starts argv arrays with explicit cwd and an environment from the fake
allow-list (`PATH` only if required, explicit test variables and vendor VIA
marker). It never inherits the complete daemon environment. Wire exclusively
owns vendor pipes. Host control bypasses data and SQLite queues.

**Leftover report (C2 §4.2).** Host's `CloseReport` gains
`leftovers: Option<LeftoverReport>`, produced after Host's close of the
connection completes, within its existing bound (unchanged), and ready
before its destination commits. VIA never signals or manages leftovers.
Detection is pending an owner decision (adapter design, conflict 4); until
it is decided, `leftovers` is `null`.

**Non-turn owners (AR6).** `ProcessOwner {session_id, turn}` and the Store
`anchors` table (which references `turns`) fit per-turn processes only. A
persistent server's anchor has no turn owner. The Codex and OpenCode slices
design a non-turn owner (server or session) with its Store schema, evidence
folder, admission, recovery and shutdown, in `crates/via-store`,
`crates/via-host` and `crates/via-wire`.

### 5.1 Group anchor: selected design, native proof required

**Identity checks followed by numeric `killpg` have a reuse race and are not
an approved implementation.** Use one small Host anchor per private process
group, running an internal entrypoint of the same `via` binary. This is a
Host implementation detail, not a second installed binary or vendor adapter.
The daemon starts the anchor as group leader with the explicit `process-wrap`
group wrapper. The anchor remains a member until it exits; it never changes
session/group. Only the anchor signals that group, using its own current
group via safe `rustix` process signal APIs. Its membership keeps the numeric
group alive for the duration of that syscall. A signal cannot be redirected
to a recycled group after the signalling process has ceased to exist.

The Host anchor starts the vendor, which inherits the anchor's current group;
no numeric group-join operation races an exiting anchor. Group membership is
inherited at child creation, before exec. Anchor's standard streams were
created as pipes by the daemon and are inherited by the vendor using
`Stdio::inherit`; Wire exclusively reads/writes their daemon ends. Inheritance
does not detach the anchor's copies. Before spawning, the anchor opens
`/dev/null` read/write; immediately after successful spawn, it redirects its
own fd 0, 1 and 2 to that file using safe `rustix::stdio::dup2_stdin`,
`dup2_stdout` and `dup2_stderr`, then drops the extra file handle. Do not use
unsafe `take_*` or raw close operations. No other anchor-owned clone of a
vendor pipe may survive: initialize neither stdio-based diagnostics nor
async stdio readers/writers in this entrypoint. Temporary launch descriptors
are dropped before acknowledging spawn. These wrappers replace each file
description without leaving Rust's standard descriptors invalid.
[rustix stdio API](https://docs.rs/rustix/latest/rustix/stdio/index.html).

Only after all three redirections succeed may the anchor acknowledge spawn.
On any failure, report `PipeDetachFailed` through the separate control socket,
start bounded own-group cleanup and return no successful acquisition. If
spawn itself fails, detach the three copies before its failure reply as well;
failure to detach follows the same cleanup path. While the anchor remains
alive after vendor exit, stdout/stderr EOF and stdin reader disappearance
must therefore reflect the vendor and its actual descendants, not the anchor.
The anchor never reads/writes vendor bytes or logs to those streams. It reports
diagnostics and vendor exit over its separate Host socket. The anchor owns
the vendor child handle and reaps it while alive; the daemon owns/reaps the
anchor. Self-group KILL necessarily kills the reaper too: remaining children
are adopted/reaped by the OS, and reports must not claim Host reaped them.
If an anchor is externally killed, surviving members retain the group, but
no replacement anchor or numeric kill is guessed; report cleanup uncertain
when the control path is gone unless independent absence proof below succeeds.

Startup protocol, on a 0600 Host-only Unix socket in the validated directory:

1. Commit anchor intent with random private marker, immutable random generation,
   uid/boot ID, current PID-namespace identity and owner. Generation identifies
   this one launch attempt, is never reused and is recovered from the Store.
   Start anchor with only bootstrap config; its private marker/control token
   is never inherited by the vendor or copied from vendor environment.
2. Anchor sends `Ready {identity, own_marker}`. Daemon checks peer uid/pid,
   Linux start ticks/boot/group and marker, then commits the full **anchor**
   identity. This is separate from vendor child identity facts.
3. Send immutable `Configure {vendor_spec}`. Commit
   `ArmIntent {anchor_id, generation, expected_version}`: it requires full
   durable anchor identity and atomically changes `launch_phase` from
   `identified` to `arm_intent`. Only its positive commit receipt permits
   exactly one `Arm {generation}` send on the original connection. This is
   the **durable ARM intent**, not a claim that the anchor received ARM.
   The anchor starts at most one vendor after receiving that command, then
   acknowledges with vendor child facts only after descriptor detachment.
   It rejects duplicate/wrong-generation ARM and never spawns again.
   Before receiving ARM, controller EOF or a 5 s bootstrap deadline makes the anchor
   exit; no vendor was started. After ARM, EOF starts own-group cleanup.
4. Commit vendor child facts before handing pipes to Wire. If that write
   fails or the daemon dies, the already-durable anchor can clean its group;
   no missing vendor-identity row authorizes a numeric signal.

Uncertain ArmIntent commit means do not send ARM. A failed/partial ARM write,
lost acknowledgement or daemon restart means do not resend ARM or Configure;
the launch/turn is indeterminate and follows conservative recovery. Recovery
may use the persisted generation only for Challenge/Status/Stop. A durable
arm intent without vendor facts is evidence of possible launch, not evidence
of either launch or non-launch. The 5 s pre-ARM timer is not restarted by
Configure, failed storage or reconnect.

Recovery reconnects (Challenge, Status, `Stop`) only to an anchor whose
durable phase is `arm_intent`. A pre-ARM anchor (`intent`, `identified`)
serves only its bootstrap controller. Recovery opens no control connection
to it and proves cleanup by the absence predicate (§5.2) after its EOF exit
(amendment A20 in the Task 3 design).

The control protocol is a closed enum of `Challenge`, `Configure`, `Arm`,
`Stop`, `Status` and replies. Configure is accepted once, before ARM, only
on the original bootstrap controller connection; its validated argv/env/cwd
spec is <=64 KiB. Other control messages are <=1 KiB, with at most one outstanding
request and bounded integer fields. Restart cannot configure or start a
vendor; it connects to the stored private socket and sends a
fresh random challenge; the anchor returns the challenge plus its own private
marker and identity, never an expected marker echoed from the request.
Validate peer uid/pid and all durable anchor identity fields. Read Linux
identity from non-environment process metadata; never open `/proc/*/environ`
or inspect vendor credential values. The connection itself targets the same
live anchor after verification; if it disappears, the socket fails rather
than selecting a new process with the same pid. Unknown or malformed control
commands close the control connection and trigger cleanup when armed.

`Stop {generation, deadline_monotonic_ns}` is idempotent and can only shorten
an existing stop deadline. Both processes use the same OS monotonic clock;
convert its ticks with checked arithmetic, never wall-clock timestamps or a
serialized Rust `Instant`. Stop grants no authority to select a different
pid/group. The anchor signals its own group with TERM, retains its TERM
handler so it can escalate, and then signals its own group with KILL. Its
signal handling is initialized only in the internal Host entrypoint (the
precise coding-standard exception is in §10). There is no vendor signal
handler or user command execution in this protocol. Control sockets/handles
are close-on-exec and never inherited by the vendor.

Recovery without a verified live anchor does not signal numeric vendor pid/pgid.
If the independent absence predicate below fails, it reports orphan/uncertain
cleanup. Externally killed anchors can
therefore leave an unmanageable surviving group; this is an explicit degraded
case, not falsely successful F22 cleanup. Ordinary daemon crash leaves the
anchor alive long enough to clean its group via EOF or verified reconnect.
Anchor death, spawn/EOF races, challenge forgery, PID reuse and parent EOF
before/after ARM require native tests. Linux 6.9 pidfd group flags are not
required; no unverified Rust wrapper or unsafe implementation is assumed.
Host returns process facts, never `failed`, `cancelled` or vendor acceptance.

### 5.2 Positive absence after anchor death

Signalling authority and existence evidence are different. Even without a
live anchor, Host may perform the **non-signalling** kernel existence check
`rustix::process::test_kill_process_group(pgid)` (`kill(-pgid, 0)`). This safe
API sends no signal; it must never be replaced with numeric TERM/KILL.
[rustix process implementation](https://docs.rs/rustix/latest/src/rustix/process/kill.rs.html).

`GroupAbsent` requires all of:

- The stored anchor record has a validated full identity, generation and
  group ID greater than 1; the group was created by Host for that generation.
- The current boot and PID namespace match the durable record. On Linux,
  record the identity of `/proc/self/ns/pid` along with boot ID before launch;
  never interpret a persisted numeric pgid in a different namespace.
- The non-signalling group query returns exactly `ESRCH`. Store an evidence
  record containing anchor ID/generation, boot/namespace, pgid, observation
  time and `group_probe_esrch`. `Ok`, `EPERM` and every other error are
  `Uncertain`, with a bounded reason; they cannot authorize a signal.

This is an atomic kernel absence observation, not a racy process-list census.
An original member still in the group keeps it present. Reuse that is already
visible gives a present/permission result and conservatively remains uncertain;
a group created after an absence observation does not resurrect the former
group. Escaped descendants remain outside the stated containment boundary:
they are not part of cleanup, they are the agent's responsibility, and they
are reported only as leftovers where a destination exists (C2 §2 Interrupt,
§4.2).
Permission/namespace failures cannot be turned into absence. Retry only this
read-only probe, at most every 20 ms until the existing cleanup deadline;
never retry a mutation. Quiescent is committed only after the absence proof.

A lost final anchor reply initially means uncertain outcome. A later fresh
`ESRCH` can settle **cleanup** to quiescent, including after autonomous EOF
self-KILL, but cannot establish protocol acknowledgement, forced-vs-natural
exit, vendor terminal state, or Host reaping. The turn remains `unknown` after
restart according to C1; positive cleanup does not permit resubmission.
If the probe cannot prove absence by the deadline, record uncertain; this
negative/degraded result cannot pass the ordinary positive F22/P-I2 gate.

F22 and platform P-I2 require both ordinary paths to demonstrate positive
proof: (a) restart after autonomous EOF cleanup has killed the anchor/group;
(b) restart while a test-barrier-held anchor survives, verify its identity,
request cleanup through it, then observe `ESRCH`. Record the predicate inputs
and observed result plus an independent harness check that the known vendor
and ordinary grandchild are gone. Also test leader-only exit, lost reply,
same-number unrelated group, namespace mismatch and denied probe: none may
produce false quiescence or an external numeric signal. No finish criterion
is weakened to accept uncertainty in place of these positive cases.

Core supplies one absolute deadline for cancellation. Fake defaults: protocol
interrupt grace 10 s (C1 `force_after_ms`); for a deadline-triggered stop that
has no wall budget left, immediately start forced OS cleanup. Host requests
anchor TERM, allows at most 2 s, requests KILL (also independently scheduled
by the anchor), and allows at most 1 s for exit/group verification.
These 3 s are a cleanup allowance after work's wall deadline, never an
extension of permissible vendor work. Explicit close's absolute deadline
includes its cleanup allowance: TERM no later than deadline minus 3 s and
KILL no later than deadline minus 1 s; already expired deadlines skip TERM
grace and attempt KILL immediately. Report uncertain by the caller's deadline
if absence is unproven; retain supervision/reaping ownership afterward while
the daemon lives. A timed-out wait releases no admission capacity and grants
no fresh wall budget. Final daemon exit follows §6.2's explicit incomplete
policy and makes no continuing-reaper guarantee after the process boundary.
A reaper that finishes after a failed child wait is a failed task, not
successful reaping.

After ordinary child/group exit, §5.2's `GroupAbsent` supports quiescent within the
documented group boundary. An exit of only the leader does not. Ack is
protocol evidence; forced is Host evidence. No successful cancellation reply
may assert that SIGKILL itself proved exit. S1 fake returns quiescent/uncertain;
P7's shared-server pending-cleanup policy remains for its vendor slice.

## 6. Store contract, schema and durability

```rust
pub struct Store { /* writer threads and their joins */ }
pub struct StoreClient { /* bounded sender, health receiver */ }
pub struct RuntimeResources { evidence: EvidenceRoot, journal: ProcessJournal }
#[derive(Clone)]
pub struct ProcessJournal { /* private existing bounded SQLite sender */ }
pub enum StoreHealth { Healthy, Failed(StoreFailure), Closing, Closed }
pub enum CommitOutcome<T> { Committed(T), NotCommitted(StoreFailure), Uncertain(StoreFailure) }
impl Store {
    pub fn open(state: &Path) -> Result<Self, StoreError>;
    pub fn client(&self) -> StoreClient;
    pub fn runtime_resources(&self) -> RuntimeResources;
}
impl RuntimeResources {
    /// Architectural no-call rule: consumed only by Wire bootstrap.
    pub fn into_wire_parts(self) -> (EvidenceRoot, ProcessJournal);
}
impl StoreClient {
    pub fn commit(&self, batch: CommitBatch)
        -> impl Future<Output = CommitOutcome<CommitReceipt>> + Send;
    pub fn read(&self, query: ReadQuery)
        -> impl Future<Output = Result<ReadPage, StoreError>> + Send;
    pub fn backup(&self, destination: BackupDestination)
        -> impl Future<Output = Result<BackupManifest, StoreError>> + Send;
}
```

`RuntimeResources` has private fields and no borrowed getter, `Deref`,
`Clone` or payload-bearing `Debug`; its construction is infallible and does
no I/O. Store's standalone public evidence-root constructor becomes a
private helper; isolated Store tests may consume the bundle. Move the
existing `commit_anchor_intent`, `commit_anchor_identified`,
`commit_arm_intent`, `commit_vendor_facts`, `commit_group_absence` and
`list_anchor_records` operations, retaining their typed parameters/results
and transaction semantics, from `StoreClient` to `ProcessJournal`. Journal
uses the same writer/sender and has no spawn, turn, handle, result, event or
log-query method. Host/ProcessControl hold only that restricted journal.
Core's StoreClient has no evidence-root or journal accessor. Inspect production
call sites: only Store owns `Store::open`, Wire calls `into_wire_parts` and
creates evidence folders, and Host calls the journal operations. This no-call rule
is a source review obligation, not compiler-enforced caller visibility.

Core bootstrap opens Store once, keeps the owner alive, and passes
`store_owner.runtime_resources()` unopened through Adapter/Route to Wire.
If forwarding construction fails, no child has started; lower handles drop
and Core shuts down the retained Store owner. Normal shutdown stops
admission/dispatch, joins active drivers/transports through C2–C5 while Store
is alive, drains observations and commits final records, then releases
lower handles and flushes/joins Store before releasing its lock. Dropping a
sender proves neither commit nor child exit. Current Store `Drop` joins both
workers synchronously: on a failure path it must run on an owned blocking
path, not a Tokio worker. That placement does not bound kernel I/O or the
join, nor does it satisfy the later persistent-failure shutdown gate. No
second shutdown owner or fake bounded shutdown API is introduced here; the
daemon's single bounded final shutdown is §6.2.

`CommitBatch` is a closed enum for create-session/turn, append-turn, submission,
acceptance/events, resolution, keyed control intent/result, session close,
recovery, `ArmIntent` and process/connection records. Each includes expected record version
and expected next seq where applicable. Store performs compare-and-set plus
constraint checks atomically; conflicting batches do not partially succeed.
Core owns the proposed transition; Store never guesses it from vendor text.
The session-close batch (`commit_closed`) carries its `ClosedRecord`'s
leftover report (C2 §4.2, C1 §5) into the `session.closed` event, the
stored `close_result` and the close operation's result, in the existing
JSON columns and atomically with the close, so a keyed replay returns the
same report; the `anchors` table is unchanged.
`ReadQuery` is a closed enum of C1 retrieval needs, not arbitrary SQL.

Open refuses unknown `user_version`, an unreadable/corrupt Store, failed WAL
activation or required directory validation. Only after that check may it
migrate. Use WAL, `synchronous=FULL`, `foreign_keys=ON`, busy timeout 250 ms,
SQLite cache target 8 MiB and `mmap_size=0`. Every migration runs in one
transaction with its `user_version` bump. No automatic repair/downgrade.
Newer-version refusal must leave database bytes untouched: first inspect an
existing database through a read-only connection, close it, then open the
sole writable connection only for a supported version. Avoid journal-mode
changes before that inspection. Before the first release no Store format is
supported across schema versions: a Store whose `user_version` is older than
the build's is an unreleased development format, is never migrated, and is
refused at open with a named error telling the user to recreate the dev Store
(remove `store.sqlite3` from the State directory); its bytes are left
untouched. `user_version = 0` is initialized only in a database file that
open itself creates (exclusively); an existing file at version 0, empty or
not, gets the same refusal before any writable open. Migrations as described
above start with the first released schema. Schema v6 (v1 was the unreleased
single-turn format; v2 lacked the unproven-anchor index; v3 lacked frozen
per-turn values; v4 lacked the close admission state and cancel cause; v5
lacked step rows, event columns, list order and evidence folders) is exactly:

| Table | Implemented columns and constraints |
|---|---|
| `sessions` | PK `id`; `handle_hash` BLOB, 32 bytes checked; `receipt` (the spawn receipt: route plan, capabilities, turn 1's `effective`); `params` (session-scope values: harness, model, cwd, `allow_untested`); `state` `active`, `idle` or `closed`; `next_seq` ≥ 2; `admission` `open` or `closing`; `close_result`; `created_ms`, `updated_ms` (the `at` of the transaction's highest-`seq` event: `last_active_at`); `harness`; `label`; `ord INTEGER NOT NULL UNIQUE` (its index serves `list`); nullable `vendor_session_id` and `transcript_hint` |
| `session_ord` | one row, `only INTEGER PRIMARY KEY CHECK(only = 1)`, `next INTEGER NOT NULL`, created as `(1, 0)`; a spawn's receipt transaction increments `next` and stores it as `sessions.ord`; never decremented or reused |
| `turns` | PK (`session_id`, `number`), FK session; `prompt` or `prompt_blob`, exactly one non-null; `effective` (the turn's frozen per-turn values, the receipt's `effective`, written once at receipt commit); `state` `queued`, `running`, `completed`, `failed`, `cancelled` or `unknown`; `queued_at`, `queued_seq`; `submitted_at`; `accepted_at`, `correlation` (vendor acceptance evidence); `envelope` (terminal); `cancel_cause` `cancel` or `close`; `ended_seq`, non-null exactly when the state is terminal; `evidence_dir` (relative to the state directory, written with `turn.submitted`). Partial unique index `turns_one_running` on `session_id` where `state='running'` |
| `steps` | PK (`session_id`, `turn`, `step`), `WITHOUT ROWID`; FK (`session_id`, `turn`) to turns; `step` ≥ 1; `started_ms`, `ended_ms`; nullable `tokens` ≥ 0; one row per completed model step |
| `spawn_keys` | PK `key`; FK `session_id`; `identity_len`, `identity_sha256` (32 bytes checked: the exact retry identity's length and SHA-256, never its bytes); `receipt`; kept for the session's lifetime |
| `operations` | PK (`session_id`, `op_key`); `verb` `resume` or `close`; `identity_len`, `identity_sha256`; `turn` with FK (`session_id`, `turn`); `result`; a `resume` row is committed in the same transaction as the queued turn |
| `events` | PK (`session_id`, `seq`), FK session; `turn` (deferred composite FK to `turns`); `type`; `event` (canonical JSON; late and time live inside it); index (`session_id`, `turn`, `seq`); `seq` allocated by Core and checked transactionally |
| `anchors` | PK `anchor_id`; `generation`, `marker`, `socket_path`; owner (`owner_session`, `owner_turn`) FK turn; `uid`, `boot_id`, `pid_namespace`; `phase` `intent`, `identified` or `arm_intent`; `record_version`; nullable identity `pid`, `pgid`, `start_ticks`; `vendor_pid`; `absence_time`. Partial index `anchors_unproven` on `anchor_id` where `absence_time IS NULL` |

Target columns and tables not implemented yet, with their owners:

| Target | Owner |
|---|---|
| `sessions`: `closing` admission state, record version and timestamps, frozen instructions/cwd/`allow_untested` | `via-jm4.7.7` (cancel/close, Task 3) |
| `sessions`: vendor session ID and transcript hint | columns `via-jm4.7.8`, values the first vendor adapter slice |
| `turns`: separate phase, cancel/cleanup columns (today inside `envelope`), revision of an `unknown` result by late evidence | `via-jm4.7.7` |
| `turns`: event-bound columns (today the envelope's `events` range) | `via-jm4.7.8` |
| `operations`: phase intent/done and other keyed verbs (`steer`, `close`) | `via-jm4.7.7` |
| `events`: separate `turn` and `type` columns (late and time stay in the event JSON); no raw reference columns | `via-jm4.7.8` |
| `steps`: `(session_id, turn, step, started_ms, ended_ms, tokens)`, primary key `(session_id, turn, step)`, one row per completed model step, the last in the terminal transaction; a session's rows go by one keyed delete | `via-jm4.7.8` |
| `turns.evidence_dir` (runtime §4) | `via-jm4.7.8` |
| `session_ord`: a one-row counter that allocates `sessions.ord` and never goes back | `via-jm4.7.8` |
| `processes` as a general table: vendor rows, exit and cleanup evidence beyond `anchors.vendor_pid` and `absence_time` | `via-jm4.7.7` |
| `metadata` table: retention low-water marks (the schema version is `user_version`) | none in S1, which prunes nothing |

There is no separate queue table: ordered queued turns without submission
intent are the queue. Only one in-flight turn per session, enforced by a
partial unique index; at most eight queued turns checked inside admission's
transaction. A partial index on anchor process IDs without absence evidence
(`anchor_id` where `absence_time IS NULL`) lets recovery count unproven
anchors past a cursor, saturating at the connection bound, without scanning
history. Terminal state requires envelope; nonterminal state forbids it.
Unknown is terminal but may be revised using C1's explicit revision batch.
Append events, resulting state, envelope and next seq commit together.
`turn.ended` is the final non-late event for that turn. Session events share
the same dense sequence.

Handle strings never enter batches, logs or the stored identity. Core hashes
them before persistence. Exact retry identity is the original validated C1
params object byte slice with the top-level handle replaced by its fixed hash;
the bounded parser tracks its byte range. Preserve all other bytes, including
whitespace/key order, to honor C1's byte-identical rule. Store keeps an
identity as its length and SHA-256, never its bytes; a retry matches when
both are equal. A prompt file is read once, before the lookup and outside
the admission lock, into a temporary blob whose length and SHA-256 enter
the identity; the blob is adopted by new work and discarded otherwise.
JSON duplicate keys
are refused. Replay lookup precedes current queue/admission/capability checks
after authentication; a matching historical key returns its original receipt
even if today's queue is full. A different payload/verb is a conflict.
Strong randomness and SHA-256 need approved implementations: the implementation
owner must request the smallest workspace crypto dependencies (OS randomness
and SHA-256), with deny results, rather than invent a hash/RNG or use
`DefaultHasher`. No dependency addition is authorized by this packet alone.

Write ordering is explicit:

1. Core commits session, handle hash, spawn key, queued turn and queued/session
   events atomically; only then acknowledge the receipt. Lost reply is replayable.
2. Acquire dispatch capacity, commit `submitted_at` and `turn.submitted`.
   Only a positive commit receipt permits Adapter open/start. Unknown commit
   outcome causes Store-failed mode, never speculative send.
3. Host commits anchor intent/generation, starts anchor, commits its
   verified identity, configures, commits `ArmIntent`, then sends ARM once.
   Anchor spawns vendor in its inherited group with the turn's stderr file
   and detaches fd 0/1/2 before its acknowledgement; Host records vendor
   facts. Wire starts its stdout reader and writes the prompt. Vendor
   acceptance is independent evidence.
4. Adapter emits an observation; Core commits acceptance, durable events
   or a step row, or folds it into the progress snapshot. The two
   acceptance paths deduplicate by correlation token.
5. Core commits terminal event/envelope, then wakes waiters. Cleanup gate
   separately controls next dispatch.
6. Shutdown performs final Store flush/checkpoint and joins before
   releasing daemon lock.

No SQLite transaction waits on vendor I/O.

Checkpoint after 8 MiB of WAL growth or 1000 commits, both configurable.
At a 32 MiB WAL (configurable) refuse new work (a receipt, a queued
turn's dispatch) by name as known not committed and retry a truncating
checkpoint at most once a second; every write of an admitted turn and
every lifecycle write still commits, so the limit is soft while admitted
turns run. This is not a Store health failure. Growth past the
limit is bounded by the admitted turns' writes and is measured in
end-to-end testing. Keep read transactions short (one bounded page); no
read holds a transaction open while waiting on a socket. No automatic
retention/pruning in S1. There are no disk size budgets. New work (a
receipt, a queued turn's dispatch) is refused by name while free space on
the state directory's filesystem is below a configurable floor, 5 GiB by
default; running turns, lifecycle and terminal writes proceed.
`daemon/status` warns when VIA's data exceeds a configurable size, 2 GiB
by default. An actual `SQLITE_FULL` or `ENOSPC` rolls back as known not
committed; only a failed rollback is uncertain (§7). Key/receipt lifetime
is never shortened by disk pressure.

### 6.1 Daemon state/runtime paths and bootstrap

Read two production path settings once at process startup. The CLI client
and daemon resolve the same settings; no fake-only path flags or C1 fields
are added. `VIA_STATE_DIR`, when present, is the absolute exact persistent
state root; otherwise use `$HOME/.via/state`. `VIA_RUNTIME_DIR`, when present,
is the absolute exact runtime root; otherwise use `$XDG_RUNTIME_DIR/via` if
`XDG_RUNTIME_DIR` is present, else `$HOME/.via/run`. Overrides are leaf roots:
append no extra `via`. Empty, relative or `..`-containing overrides are
invalid. An invalid configured XDG runtime path is an error, not a silent
HOME fallback. Missing/invalid HOME is an error only when the selected
default uses HOME. Do not expand tilde, environment references or shell
syntax inside override values.

Auto-start passes the resolved state/runtime paths explicitly in the new
daemon's environment with its other approved bootstrap settings, without
copying the client environment or passing these values to vendors.
Auto-start also forwards each name in `via-adapters`' fixed `BOOTSTRAP_ENV`
that is set in the client (`HOME`, `PATH`, `LANG`, `USER`, `LOGNAME`,
`XDG_RUNTIME_DIR` and the three `VIA_FAKE_*` names). The values are read
once by `AdapterConfig`, never logged or stored, and reach a vendor only
through that adapter's allow-list. No credential variable is listed. When
connecting to an existing daemon, the CLI completes hello, reads
`daemon/status` and compares its expected Store path with `store_path` by
filesystem identity of parent directories plus the fixed filename, not
textual path alone. On mismatch, exit 4 with a local configuration error;
do not stop that daemon or silently use its Store. A version-mismatched
client whose Store matches may request the idle-only stop of C1 §1 (a plain
`daemon/stop` that only daemon main's idle predicate accepts). On a Store
mismatch the daemon is never stopped (amendment A13 in the Task 3 design). Explicit protocol clients
choose a socket and can inspect status themselves. No new handshake field is
needed. Diagnostics may contain paths but no handles or vendor payloads.

```text
<state>/
  store.lock                 persistent Store-owner lock inode
  daemon.json                optional daemon config: disk floor and warning, WAL (§8)
  via.log                    daemon warnings and errors; via.log.1 after rotation at start past 10 MiB
  store.sqlite3              SQLite database (user_version schema)
  store.sqlite3-wal          SQLite-owned sidecar when present
  store.sqlite3-shm          SQLite-owned sidecar when present
  evidence/<session-id>/<turn>/  stderr.log, undecoded.bin, final_text.txt
  blobs/<blob-id>.blob       bounded immutable request/effective data
<runtime>/
  daemon.lock                persistent daemon/socket-owner lock inode
  via.sock                   C1 Unix socket
  anchors/<anchor-id>.sock   private Host anchor control sockets
```

Filename IDs are validated internal IDs, never caller paths. §4/§6 still
define the evidence folder. `daemon/status.store_path` returns
absolute `<state>/store.sqlite3`; `socket_path` returns
`<runtime>/via.sock`. Evidence fixtures, sync barriers, manifests and Store
backup destinations reside outside Store-owned evidence/blob namespaces; test
collectors receive the resolved root/path directly.

The daemon creates missing VIA-managed roots/subdirectories mode 0700;
default `$HOME/.via` is VIA-managed too. Refuse existing managed directories
with wrong owner/mode/type or symlinks rather than chmod/chown. Do not modify
arbitrary ancestors of an explicit root. Use safe platform path operations,
validate final roots without following symlinks, retain their directory
identities, and resolve managed children without following symlinks. Reject
unsafe existing lock/database/log targets before mutation. Native race
resistance remains a platform gate. Create regular state files and both
socket classes mode 0600 from the start. Initialize daemon umask 0077 before
threads or file creation, including SQLite sidecars. Store alone opens
SQLite and blob files, and validates or creates the `evidence/` root; Wire
creates each turn's folder under it; Host opens the turn's `stderr.log` for
the child; `final_text.txt` is written through `StoreClient`. Host owns
anchor sockets; daemon main owns the singleton/socket lock.

Acquire nonblocking `daemon.lock` first, then nonblocking `store.lock`; hold
both for daemon lifetime and never unlink either inode. Only after both
locks and directory checks succeed may startup replace stale `via.sock` or
mutate/open Store. The second lock prevents distinct runtime roots from
opening one state root as competing writers. Lock conflict refuses startup
without deletion/takeover. A daemon whose `daemon.lock` attempt meets
contention exits 75; a `store.lock` conflict exits 4. The CLI's
auto-start retries a 75 exit within a 15 s startup budget and shows the
daemon's startup stderr for other failures (amendment A3 in the Task 3
design). Shutdown retains §5/§6 flush/join then lock
release order. Do not bulk-delete anchor sockets at startup.

Default paths still form one per-user daemon. Explicit state/runtime pairs
provide test isolation or relocate that single ordinary daemon; clients
must use matching settings. The foreground server entrypoint is `via daemon`
with no child verb or double-fork. Auto-start uses the same entrypoint;
`via daemon status` and `via daemon stop` remain client verbs.

### 6.2 Daemon stop, final shutdown and exit

C1 `daemon/stop` replies `{"stopping":true}` once the request is accepted and
Core's admission gate is closed (new `spawn` → `daemon_stopping`). The reply
is acceptance only: it never proves that processes stopped, Store flushed or
the daemon exited, and the CLI may report only "stopping requested" from
it. A lost reply or caller disconnect does not cancel an accepted stop.
Without `drain` or `force`, a stop with active turns is refused
`admission_refused`; `drain` with `force` is `invalid_params`. A later
`force` escalates an accepted drain; any other repeat keeps the accepted mode.

- **Idle** (no active turn) and **force** enter final shutdown immediately
  after acceptance. Force closes every running turn with mode `force`: Core
  signals the turn's route, which asks the verified anchor to stop its
  private group (C2 Close(Force)) and drains its pipes under its cleanup
  bound. Messages already read are still delivered. A receipted turn not yet
  launched starts nothing; with a complete Host journal and no anchor intent
  for it, its cancel is `requested` with cleanup `quiescent` (C1 §7.4). Core
  commits the turn in final shutdown (C1 §7.6
  force row) once Host has reconciled its anchor; `forced` requires the
  anchor's report that its cleanup began while the vendor was live. A turn
  whose vendor may have launched (ARM sent) without that report or a
  terminal ends `unknown`. That report and proved group absence each count
  whether Route's own close or final-shutdown recovery obtained them; a
  failed recovery never discards what the close proved. A force Wire
  observed before an acquisition failure (including its deadline) owns that
  failure; an acquisition deadline observed first stays `deadline_wall`.
  Host shutdown leaves 1 s of the final deadline for these commits. After ARM, Host keeps the vendor pipes so Wire drains
  them on every acquisition failure or abandonment.
- **Drain** keeps serving reads while accepted turns finish under their own
  existing work deadlines; the drain phase gets no invented 10 s deadline.
  When accepted and active work has settled, final shutdown begins.
- Daemon shutdown produces no leftover report (C2 §4.2 limitation).

Final shutdown has **one absolute 10 s deadline** covering client closes,
Host control closes, anchor reconciliation, task joins, final durable
records and the Store join; no phase receives a fresh budget. F12
measures the same total from the latching Store failure (`failed_at`) and does not restart it
when final shutdown begins. Order: stop listening and admission; settle or
classify turns; request owned-group cleanup; collect process/task evidence;
commit final records while the Store owner is alive; then join Store off the
Tokio workers.

**Clean** shutdown requires positive group absence for every committed
anchor, no pending or failed owned join, every final record committed and
the Store writer joined. The daemon then emits its bounded
summary, releases resources and locks and exits **0**. **Incomplete**: at the
deadline, or after a failure that precludes clean completion, the daemon
snapshots the remaining uncertainty, aborts unfinished tasks, reports those
that did not join and exits **4**. Only daemon main selects this path; a
library timeout or dropped handle never exits the process or detaches work.
A Store join that does not finish is abandoned to process exit, whose
termination releases the locks; this is crash-like termination with
conservative recovery (§7), never a successful flush. Abort, handle drop,
lost control, pending SIGKILL and OS adoption prove neither reaping nor
quiescence, and VIA promises no reaping after its own process exits. An
uninterruptible kernel operation can still defeat the process-exit bound;
that is an unmet bound or infrastructure failure, never a pass.

The best-effort final summary is one bounded JSON line in `via.log` (§6.1);
the daemon writes stderr only while it starts, so an auto-starting CLI can
report a failed start. The line is `{"daemon_shutdown":{…}}`, separating
stop mode, elapsed time,
pending and failed joins, committed anchors, owners with uncertain cleanup,
the named Host failure, force-stopped turns whose terminal did not commit,
Store join status and the `clean`/`incomplete` disposition. `GroupAbsent`
is not reaped, and a joined status task is not group absence. It adds no
`daemon/status` field or RPC; `via.log` is diagnostic, not a contract, so
the line is no durable report. It may be lost on Store failure or abrupt
death, and the outer harness captures exit status and diagnostics.
A result that cannot persist keeps F12's named `store_error` and
`terminal_persisted:false`; no envelope is invented or replaced.

## 7. F12: persistent Store failure and crash reconciliation

A Store write has one of three outcomes:

- **committed**;
- **not committed**, when SQLite rolled the transaction back, or the
  request was never enqueued because the writer's queue was full;
- **uncertain**, when the error came from the commit step, the writer
  thread is gone (its queue is disconnected or a reply was dropped), or the
  2 s operation watchdog expired. An uncertain write latches.

A write that is not committed is scoped to the request or turn that made
it. A receipt fails with `store_error` and `commit_outcome:
not_committed`. A turn's first failed write stops that turn: no further
agent I/O is started for it, and the turn ends `failed(store)` with its
cleanup evidence through one resolution write. A natural terminal that
did not commit is retried once. Other requests, turns and sessions are
unaffected.

Core latches daemon health to `StoreFailed` when any write's outcome is
uncertain, when a turn's resolution write or terminal retry fails in any
way, or when SQLite reports corruption. The latch broadcasts through a
reserved watch channel and stops admission and dispatch immediately. A
watcher is independent of Store's work queue.

A latched Store failure cleans up every active connection. No Store task
silently swallows failure.

| Caller situation | Write not committed (scoped) | Latched |
|---|---|---|
| Unacknowledged spawn/resume | `store_error`, `commit_outcome: not_committed`; a keyed retry may succeed | `store_error`; `commit_outcome: unknown` and `retry: same_key_only` when uncertain; a timeout never proves absence |
| Receipted turn whose own write failed | the turn ends `failed(store)` with cleanup evidence, except that a natural terminal whose one retry commits keeps its result, and a dispatcher-owned queued cancellation whose retry commits stays `cancelled`; `wait` and `result` return that envelope | `store_error` with session/turn, `durable_state` from the last known commit, `terminal_persisted:false`; no invented envelope |
| Durable terminal result readable after failure | return it | return that committed result, not a fabricated new failure |
| Other mutations and dispatch | unaffected | refuse with `store_error`; `cancel` and `close` return `store_error`, and the latch's force stop performs cleanup |
| `daemon/status` | `health: healthy`; `store_failure` reports the latest failure and its scope | `health: store_failed`, which is sticky; `store_failure` reports the latest recorded failure and its scope; no prompts, payloads or handle |

This scoping is amendment A14 in the Task 3 design; its §7.2 lists the
scoped cases.

On the latched path: Core makes one best-effort failure-resolution batch
for each affected active turn: `failed(store)`, real cleanup evidence, plus
queued cancellations. It
uses a reserved Store slot if the writer is usable. If storage remains failed,
record the attempt only in bounded memory/diagnostics; do not queue infinite
retries, claim success, or overwrite an earlier committed vendor result.
Outcome uncertainty includes the possibility that the original commit did
persist despite an error; no contradictory second batch is issued until
that transaction is resolved by the writer. No transaction outcome resolution
within 2 s means skip the failure write, keep health failed, and clean up.

On the latched path: Host independently starts stopping private groups
using §5 on failure notification, without waiting for Store. Its bound is 3 s from the instant
the failure is raised, not from when a Host task first runs, and no later
step grants a fresh allowance. Once the failure is raised, Host sends no
new ARM from a launch that has not passed its ARM gate. A launch already
past the gate is sent `Stop` by its owner once Host receives its `Spawned`
reply; if that reply is lost, the failed acquisition's EOF cleanup applies.
Host attempts one `Stop` for every armed group, even when the bound has
already passed. The bound applies to the wait for the reply; the control
lock and the write themselves are not bounded.
In that early-stop exchange, a reply read at or after the bound does not
count as force evidence. A group whose `Stop` is not answered in time, or
whose cleanup is not proved, has uncertain cleanup until reconciliation
proves absence. The runtime does not promise that a group is gone within
3 s: the anchor may be slow, and a write to an anchor socket the kernel
will not accept bytes on cannot be completed (amendment A23 in the Task 3
design). Drain reads until EOF/deadline.

On the latched path: the daemon remains available for diagnostic/read
requests for at most 5 s after the latching failure (`failed_at`), attempts
Store flush and task joins within a total 10 s shutdown bound measured from
that failure (§6.2), then exits 4. No successful graceful-stop result
is returned for failed flush/join. Synchronous disk I/O can hang in the kernel:
it cannot be cancelled by a Rust timeout. Retain/report the unjoined thread;
the outer process supervisor enforces the process-exit bound in tests. The
runtime cannot promise bounded in-process joining under an uninterruptible
kernel operation; this is distinct from F12's repeatable returned I/O error.

Restart first performs Store validation and recovery writes, then enables
admission. For each last-durable nonterminal turn: submission intent ->
`unknown`, no automatic resend; cancel queued successors. A queued turn with
no submission intent remains queued only when no predecessor is unknown.
Recovered host exit alone cannot prove no vendor action happened. If Store
is still unwritable, startup fails; it does not dispatch from an uncommitted
recovery view. There is no fabricated persistent Store-failure marker when
the filesystem could not persist one. Durable intent plus conservative
recovery provides safety even if the last in-memory failure reason is lost.
Daemon-crash recovery produces no leftover report: recovered turns carry
`leftovers: null` (C2 §4.2 limitation).

## 8. Bounds and scheduling

Defaults below are S1 acceptance constants, not throughput claims. Tests may
reduce durations/capacities through explicit test config while separately
testing default ceilings. Only the `daemon.json` keys (disk free-space floor,
data-size warning, WAL limit and checkpoint triggers; see the end of this
section) are configurable; C1, C2 and every other limit here are fixed
(T4-A37). All
payload limits count encoded bytes plus separately bounded decoded structure.

| Resource | Default hard bound | Full/expired behavior |
|---|---:|---|
| Active private connections | Four live connections daemon-wide (per-turn process or persistent server, each with its anchor); a slot is reserved only for a new connection (C2 §3, AD16) | Queue eligible work; do not create a child until a slot is reserved |
| OpenCode owned HTTP servers / loopback listeners / SSE streams | 4 of each daemon-wide, one VIA session per server and private namespace | Fifth owner waits under Core admission or remaining deadline; no active/uncertain owner is evicted; caps consume common process admission, not extra pools. S1 has no memory pool (T4-A43); the OpenCode task (`via-4sw.3.2`) re-derives these bounds. |
| OpenCode vendor child-session metadata | 32 records per live server; one active top-level turn per owner | Refuse excess child metadata without routing it to another owner; idle namespaces retain durable identity but no listener or server memory |
| Queued turns | 8/session, 128 daemon-wide | `queue_full` / `admission_refused` before commit |
| Unresolved turns (receipted, no terminal known durable: in flight or failed to persist) | 256 daemon-wide | When full, `spawn` first forgets failed turns whose terminal a Store read now finds durable; still full of in-flight turns is `admission_refused` ("too many unresolved turns"), while a retained failed turn keeps refusal and its reads `store_error` |
| Client sockets / in-flight requests | 32 / 1 per socket | Extra connection refused; parser stops accepting the next request until current response admission |
| C1 line | 1 MiB including LF | `request_too_large`, then close |
| Global C1 input buffers | 32 MiB by construction (32 sockets × 1 MiB) | 5 s partial-request deadline prevents monopolization |
| JSON structure | depth 64, 65,536 nodes per document | Bound during streaming parse, before constructing a `Value`; named invalid params/protocol error |
| Vendor stdout message | 1 MiB including LF | Fail connection; the first 64 KiB saved as evidence |
| Pipe read buffer | 64 KiB per pipe | Reuse; never grows |
| Route message staging | 1,024 messages and 4 MiB/connection | Fail connection if saturated; health/control bypass |
| Codex shared Route ingress | 16 messages and 1 MiB/thread within the existing connection staging; fixed per-server buffers (the Codex task) | First full thread lane quarantines that generation immediately, separate from C2's 10 s stall. Reserved-path or global/raw failure escalates to connection overflow (C2 §4) |
| OpenCode HTTP/SSE transport metadata | Existing bounded Wire message splitting | Strip Basic `Authorization` before transport logging/capture; retain credential-redacted metadata and bounded body and HTTP message-boundary evidence; route by owned server generation and vendor session/message IDs |
| C2 observations | 1024 items and 4 MiB/session | Wait only normalizer; at 10 s without drain, the adapter closes the session's route hop; a private route fails the connection `overflow`, a shared route quarantines the thread generation (A1, C2 §4) |
| C2 observation payload | 256 KiB encoded; final text sent in pieces; IDs, names, stop reasons and codes 1 KiB | Fail protocol, the message saved to the evidence folder; unknown messages keep no payload |
| Data commands / control commands | 1 / 8 per driver, 64 KiB controls total | Data waits only until absolute deadline; duplicate interrupt/close coalesces; other control admission refused explicitly |
| Health channel | watch latch + at most one exit report/connection | Latest health replaces state; `health: store_failed` is sticky after the latch; `store_failure` reports the latest recorded failure and its scope; no event payloads |
| Store requests | 64 + 8 reserved lifecycle, 8 MiB total | Request-side overload refusal; Host cleanup never awaits this queue |
| Store transaction | at most 128 events and 1 MiB payload, not counting the one terminal envelope a transaction may carry (itself at most 1 MiB, C1 §5) or the turn's step rows it carries | Split event batches without splitting a lifecycle atomic batch; refuse a larger request before it is queued |
| Store operation watchdog | 2 s, busy timeout 250 ms | Latch uncertain failure; no resend; keep thread ownership |
| Socket response serialization | 1 MiB per response (a page, `status` or an envelope) plus 512 B | Stream bounded encoding; page reads stop on bytes as well as count; each reply written within 10 s of being ready, else the socket closes |
| Envelope | 1 MiB by construction (C1 §5): final text over 256 KiB goes to a file of at most 64 MiB; the denied and declined lists keep 1,000 entries each | Never fails the turn |
| Work deadlines | wall 1 h, idle 10 min | C1 deadline disposition; idle resets on normalized meaningful progress, not stderr/noise |
| Cleanup / daemon idle | §5 (3 s OS cleanup); daemon 60 s idle | No idle exit with a live client, running/queued work or pending cleanup |

Large C1 prompts are persisted from the bounded request buffer. Store command
payloads over 1 MiB use bounded chunks in a temporary Store-owned blob file
synced before the atomic row references it; never keep all queued prompts in
memory or split atomic receipt creation across commits. Blob paths are
private relative IDs, checksum/length checked at recovery; unreferenced blobs
are harmless. The same mechanism stores large immutable effective params, for inline prompts over 256 KiB and for prompt
files, while keeping Store messages small. Load only the dispatched prompt,
one per running turn. Outbound fake start may encode beyond 1 MiB; its input
message ceiling is the 16 MiB prompt plus bounded JSON wrapper expansion, streamed
without a whole second copy. Inbound vendor message cap remains 1 MiB.

Every buffer has a fixed maximum and every kind of holder a fixed count;
nothing is preallocated from an unchecked peer size or item count.
Persisted history is paged, not cached per session.
Idle session data is evicted and reloaded by bounded pages; the number of
historical/idle sessions on disk does not imply one resident actor per session.
Read requests and commits use separate bounded lanes with fair round-robin
service; a request flood cannot starve commits. At most 128 ready data items
are processed before checking deadlines/control/health again. F24 must measure
control response scheduling within 100 ms absent OS scheduling starvation.

Each of at most four anchors is limited to one 64 KiB launch spec and 64 KiB
control/diagnostic staging; its measured RSS ceiling is 32 MiB. Anchor RSS is
reported separately from the daemon and vendor in F24. Anchors load no SQLite or runtime
session cache and never receive queued prompts.
There is no memory pool, byte counter or memory setting. The daemon's
worst case is the sum over holders of each holder's fixed buffers times
its fixed count (C1 sockets, connections, running turns, the Store), about
332 MiB estimated. F24 drives every holder to its maximum at once and
records RSS at 10 ms intervals; the daemon's peak RSS less its idle
baseline must stay within that sum plus a 25% margin for allocator
overhead and CI variance, and growth must stay below 32 MiB after the
first 64 MiB of a 256 MiB flood. RSS is an empirical gate, not a
mathematical bound. Failure of either assertion requires correction or
explicit design review, not silently enlarging the limit.
The disk free-space floor, the data-size warning, the WAL limit and its
checkpoint triggers are keys of `daemon.json` in the state directory, with
provisional defaults. The daemon reads it once at start; a change takes
effect at the next start, and an invalid file refuses to start with a named
error. C1, C2, memory and the other runtime §8 limits are not configurable.
The Codex shared server's lanes and tool metadata are fixed buffers counted
per server by the Codex task, which measures 32 loaded leases and four
active turns against the RSS gate.
The S1 fake RSS result alone does not qualify this shared-server extension.
For the OpenCode extension, Adapter owns the frozen server key and vendor
semantics; Routes owns typed HTTP/SSE correlation; Wire owns sockets, message splitting,
bounded staging; Host exclusively starts and
supervises the authenticated loopback server. Core's durable-state and Store
ownership do not change, and Adapter receives no Store access. Four separate
vendor processes and their external memory require their own measured gate;
the S1 private-process fake cannot qualify these resources.

## 9. Paging, polling and slow peers

Store is the sole event source. A page is one bounded read transaction
that returns the page, the scan cursor and the committed head. `next_after`
records the last scanned seq, not just the last matched event; `more` uses
the captured head. Filtering can yield an empty page that still advances.
Page byte limit is 1 MiB. Keep read transactions short (one bounded page).

There is no follow stream and no subscription. Callers poll `status` for
the in-memory progress snapshot and step history, and use `wait` for a
turn's end; `wait` checks the Store at once, then once per second.
Cancelling a wait request only releases that waiter.

A reply is written within 10 s of being ready to write, otherwise the
socket closes (C1 §1), so a peer that never reads holds its reply buffer
for at most that long. `logs` returns only the addressed turn's evidence
locations (C1 §3.12).

## 10. C1/C2 amendment audit

The reviewed runtime clarifications below are incorporated into C1, C2 and
the coding standard; this table retains their source-to-contract audit.
The platform packet retains its own native proof and pending P-OWNER-1
macOS linkage gate. No design text here claims those live gates have passed.

| Source | Required clarification |
|---|---|
| C1 summary, §3.8–3.9, §8.1 | Qualify “every later problem resolves the turn, never a request error”: after a receipt, a write known not committed is scoped to its turn and resolved by §7's per-site rules: usually `failed(store)` through one resolution write, but a natural terminal whose one retry commits keeps its result, and a dispatcher-owned queued cancellation whose retry commits stays `cancelled`; when the Store failure is latched (an uncertain write, a failed turn resolution write or terminal retry, or SQLite corruption), failure to persist a terminal result returns named `store_error` with `terminal_persisted:false`; this is not a terminal envelope. Define F12 error metadata in §7 above. Add `health` and `store_failure` to `daemon/status`. |
| C1 §3.11 | Make `event_end` delivery best-effort subject to the 2 s writer deadline; add `store_error` reason; define byte/count outbox limits, scan vs delivery cursors and unsubscribe ordering. Replace literal registration “in the same Store read transaction” with the equivalent serialized cursor registration plus durable rescan protocol in §9 (no durable subscription rows). Superseded by T4-A25: follow removed. |
| C1 §1, §3.11–3.12 | State bounded JSON depth/node limits; pages are limited by bytes as well as requested item count; named overload on oversized aggregate result. Envelope accumulation overflow is explicit; no silently truncated successful result. |
| C1 §7.5 | Replace direct vendor marker discovery/group kill with verified live anchor identity/challenge authorizing only anchor-issued own-group cleanup. Vendor identity is distinct evidence; no environment marker scan or daemon-side numeric TERM/KILL. Absent/unverified anchor means no signalling; cleanup is uncertain unless §5.2 independently proves group absence by a same-boot/namespace, non-signalling `ESRCH` probe. Submission recovery stays unknown/no resend, regardless of proven cleanup. |
| C2 §2 SessionCx/SessionDriver | Opaque resource wiring instead of Adapter-accessible raw handle; separate control/health lanes; acceptance correlation token; health failures and Host cleanup travel upward/downward through C2/C3/C4 rather than Core calling Host directly. |
| C2 §2 Recover wording | “Core asks Host” means Adapter/Route/Wire forwards cleanup; `Dead` is process evidence only, never proof of non-submission or no action. |
| C2 A1 and §4 | Keep 1024 items/10 s unchanged; add byte budgets, splitting rules, independent sticky health failure delivery and acceptance deduplication. |
| C1 §3.14, runtime §§2, 5, 6.2, 7 | Stop receipt is acceptance only; drain keeps work deadlines, force and idle enter final shutdown at once; one absolute 10 s final deadline; clean exit 0 only with positive cleanup, joins and durability, otherwise truthful incomplete exit 4 chosen by daemon main. Live-daemon deadlines and cancelled shutdown futures never abandon owners; Host's shutdown report keeps pending/failed joins and the named failure on every path. |
| Coding standard §§5–6 | Permit signal setup in the same binary's internal Host-anchor entrypoint in addition to daemon main. Document anchor group creation and vendor inherited membership; distinguish durable anchor identity from vendor child facts. Force group KILL kills the anchor/reaper, so remaining child reaping is by the OS, never falsely reported as Host-reaped. Marker remains explicit vendor environment data but is never recovered by reading vendor environments. |
| Platform packet §5/§5.1 and P-I2–P-I4 | Match anchor-based authority, all-three-fd detachment, persisted generation/ArmIntent and §5.2 positive absence predicate. Keep native positive cleanup and negative identity-refusal requirements, with no uncertainty-only substitute. |

The anchored process lifetime (§5), evidence folder (§4), numerical limits
and F12 policy are material decisions for this packet's independent review.
The adapter design's runtime amendments (AR1, AR3–AR6 and AD20's report
delivery) are applied in place in §§2–8 and §11.1. AR2 and its rows in this
table wait for the owner's leftover-detection decision (conflict 4).
No P7/P11/P13 or A2–A8 vendor decision is made here. No change to the approved
dependency graph, one-route invariant or credential boundary is requested.

## 11. Failure-first seams and exact validation

Tests drive the real `via`/daemon/SQLite path. The planned test-only
`via-fake-agent` binary and layer-check allowance are an approved S1-plan
addition, not a production dependency or installed executable. The fake uses
explicit named synchronization messages/files owned by the test directory;
no fixed sleeps determine correctness. Time tolerances measure OS behavior.

### 11.1 Fake fixture launch configuration

Default-gate fake configuration works without `test-failpoints`. The three
paths below select an external stand-in process and scenario; they do not
activate VIA's pause/crash/`fail_io` controller. `via-adapters`'
`AdapterConfig` reads them once at daemon startup into only the fake
adapter's explicit launch configuration. The names and rules are unchanged.

| Variable | Consumer and meaning |
|---|---|
| `VIA_FAKE_AGENT_BINARY` | Fake adapter executable override; absolute executable path passed to Host; required to make fake available |
| `VIA_FAKE_SCENARIO` | Absolute JSON fixture path, passed explicitly only to the fake child's allow-listed environment |
| `VIA_FAKE_SYNC_DIR` | Absolute existing directory for named fixture barriers/artifacts, passed explicitly only to the fake child |

All three must be supplied together for the configured fake route. Absent
configuration makes fake unavailable; partial or malformed configuration is
a named fake harness-unavailable error, never a default to another binary or
scenario. The daemon does not parse fixture actions, execute shell text or
forward its complete environment. The anchor receives only the reviewed
launch spec, and its private token never enters the vendor environment. The
fake binary reads scenario/sync variables itself. No fake-scenario public CLI
flag, C1 field or global CLI environment passthrough is added.

The outer test harness starts an isolated daemon with this configuration
explicitly before client commands and terminates it afterward. Tests do not
depend on an auto-starting CLI inheriting the caller's environment into a
daemon. Use §6.1's state/runtime-directory settings for isolation; this
rule does not add fake-specific path flags.
The private S1 fixture invocation uses existing argument spelling
`--harness fake --model fake`. This is test-only routing, not a promised
first-release public harness or a new flag. No release-gated failpoint
activation is implied by selecting that fake fixture route.

Fixture shape is `{"expected_request":{...},"steps":[...]}`, or
`{"scripts":[...]}` of such scripts for multi-turn and multi-session
deployments: each launch runs the first script whose `expected_request` its
start request contains, and fails when none does. The selected
`expected_request` fields may be a subset, but the fake must also validate
the full typed start schema, positive turn, ID 1 and no second start.
Positive scenarios emit all fields required by §3.1. Explicit negative
`emit`/raw scenarios may violate that schema; the runner must not sanitize
their faults. Fixtures use known turn numbers to write `fake-turn-N`; no
template engine is required. Named barriers coordinate with the harness,
and waiting has an outer scenario deadline. Fixture commands affect only the
external fake process.

VIA's named failpoint controller and activation-variable parsing remain wholly
inside `#[cfg(feature = "test-failpoints")]`. The three fixture path keys do
not grant access to that controller. The fake executable is test-only and
never packaged; the binary override does not ship it. The release exclusion
check probes actual failpoint inputs, not a blanket ban on fixture path
strings. Removing the fake adapter from a release is a separate decision.
Route tests parse the same fixtures the fake emits and cover missing fields,
wrong IDs, duplicate acceptance and control before acceptance. Harness tests
cover default-feature fixture launch, full typed validation with subset
expectations, two isolated session IDs with identical per-turn IDs and no
crosstalk, and no fixture path leakage to non-fake vendors. Actual VIA
failpoint activation remains ineffective in a no-feature release.

For state-path conformance, each scenario uses a short absolute private root
with separate `state`, `runtime`, `fixtures`, `sync` and `evidence` children.
Set `VIA_STATE_DIR`/`VIA_RUNTIME_DIR` identically for its daemon and clients;
default-path tests use an isolated HOME/XDG environment and never touch user
files. Verify selected paths/status, default and override precedence,
invalid/empty/relative settings, unsafe symlinks/owner/modes, same-runtime
startup race, same-state/different-runtime writer refusal, client Store
mismatch without daemon takeover, and disjoint sockets/locks/SQLite and evidence data
for two scenarios. Use Store backup API for consistent evidence, never a
copy of live SQLite/WAL files. F1–F3 and F8–F12 remain unchanged.

### 11.2 Fake finalization and outer cleanup evidence

Positive fake scenarios assert both the C1 envelope and fake exit zero after
§3.1's stdin EOF validation. A Core-completed envelope cannot turn a failed
fake protocol check into a pass. The normal reply regression writes two
full starts before releasing its existing pre-accept gate: after terminal
and input close, fake must exit nonzero for the duplicate. The same fixture
with one start exits zero; a permitted interrupt remains valid. Also cover
a duplicate read ahead, one after an in-lifetime gate, a partial second
message, and a controller that leaves input open after terminal (bounded
nonzero finalization failure). Direct fake tests close input after reading
terminal, just as Route will. A future Route integration test must prove
half-close begins before process-exit/result waiting while output still
drains. Do not add a special `expect_request` step to the normal fixture
or alter C1 lifecycle precedence to hide a Route resend.

The outer scenario supervisor retains its isolated state/runtime roots
after daemon exit. Before a deliberate crash, capture committed anchor
rows from a bounded read-only atomic SQLite snapshot/Store backup; after an
unexpected failure, refresh from the readable Store. Never splice fields
from different row versions. Retain full anchor identity, immutable
generation, launch phase and private control path/marker in supervisor
memory; vendor identity or fixture PID files give no cleanup authority.

Teardown first attempts ordinary C1 force-stop through a reachable daemon
for at most 2 s. If needed, terminate the directly owned daemon by its
retained child handle and allow at most 1 s to reap; never signal a guessed
daemon PID. If that path fails or the daemon has died, process every captured
committed anchor. Connect to its private control socket and verify peer,
full identity and fresh challenge per §5.1. Only a verified live anchor may
receive `Status`/`Stop` for its persisted generation; teardown never
Configure/ARMs or replays input. If verification fails or the anchor died,
send no destructive command. In either case, claim absence only through
§5.2's exact predicate: valid recorded anchor generation/group, matching
boot and PID namespace, and a non-signalling group query returning `ESRCH`.
No valid identity, a guessed PID/PGID, present/reused group, permission
error or namespace mismatch can prove absence. A lost Stop reply does not
establish forced/acknowledged outcome.

The harness implements this seam itself from the snapshot; it never reopens
Core or the Store owner and uses no daemon debug RPC or CLI verb. The entire
outer teardown, including normal-stop fallback and observation,
has a 10 s deadline. Anchor Stop uses the earliest of its existing deadline,
now + 3 s or the outer deadline. Observe at most every 20 ms and supervise
at most four active groups. This test budget does not extend Host's native
3 s cleanup allowance. Pre-ARM anchors without durable full identity could
not have spawned a vendor; their existing 5 s bootstrap/EOF exit applies.
Failure to read a usable snapshot, validate identity or prove absence by
the deadline is explicit incomplete cleanup with evidence, never a silent
pass or permission for numeric signalling.

Write `cleanup.json` in each scenario's private evidence directory on every
teardown with per-anchor generation/identity reference, verification result,
requested cleanup, exact absence-probe result, timing, direct-child reap
status and remaining uncertainty. Omit private markers/control tokens from
the summary; the required private Store backup retains the record. Preserve
the originating pass/fail/timeout/infrastructure result and add cleanup or
evidence failure without replacing that cause. R1's native regression must
kill the daemon first while the vendor and ordinary grandchild occupy the
reviewed anchor group, then authenticate or observe autonomous cleanup,
prove absence and retain artifacts. Until Host anchor implementation exists,
this remains red runtime coverage; bounded supervisor evidence/error tests
do not substitute an unanchored fake for positive proof.

Feature name: `test-failpoints`, default off. `via-cli` forwards it through
the existing dependency edges only to owners that need it. Code and any
environment-variable parsing for failpoints are inside
`#[cfg(feature = "test-failpoints")]`; production contains no runtime switch
that can activate them. Isolated tests use private injected clocks and a
closed fault backend for SQLite and evidence-file operations. Production uses concrete
implementations; no general-purpose dynamic plugin mechanism.

Failpoint controller: private per-scenario directory plus token; commands
identify point, occurrence and action `pause`, `crash`, or `fail_io`. It
acknowledges entry so the harness can assert durable state before releasing
or killing the daemon. `fail_io` can remain active across the best-effort
failure write; the harness explicitly removes it for the recovered daemon.
No prompt, handle or vendor secret is included in acknowledgements.

| Point / seam | Required distinguishing assertion |
|---|---|
| `store.spawn.before_commit`, `.after_commit` | F8/F13: none or session+turn+key+hash together; lost reply replays one receipt |
| `core.intent.after_commit`, `wire.prompt.after_write`, `core.accept.before_commit` | F9/F10: restart unknown, zero second sends, queued successors cancelled |
| `host.anchor.before_arm_intent_commit`, `.after_arm_intent_commit`, `.after_arm_write_before_ack`, `host.vendor.after_spawn` | No ARM before positive durable ArmIntent receipt; stored generation survives restart; crash/lost acknowledgement never causes a second ARM/spawn; EOF/reconnect cleanup works without vendor-facts commit |
| `host.anchor.after_pipe_detach`, `.detach_fail_stdin`, `.detach_fail_stdout`, `.detach_fail_stderr` | Keep anchor alive after vendor exit: Wire sees both output EOFs and stdin `EPIPE`; each injected detachment failure returns no acquisition and performs bounded group cleanup |
| `host.anchor.final_reply_lost`, `host.group_absence_probe` | Autonomous EOF and verified reconnect cleanup each produce positive same-boot/namespace `ESRCH` proof plus independently observed vendor/grandchild absence; leader-only exit, reused group, probe denial or namespace mismatch remains uncertain with no external numeric signal; lost reply never invents forced/acknowledged outcome |
| `store.commit.fail_persistent` | F12: named error after receipt, terminal flag false if failure cannot commit, dispatch stopped, group cleanup, bounded memory; restart either fails unwritable or reconciles unknown without resend |
| `store.commit.reply_lost` | Durable mutation may exist; no duplicate send from timeout; keyed retry sees exact committed result |
| `core.observations.pause`, fake flood and stderr flood | F24: 1024/byte bounds and 10 s overflow; independent control service; RSS assertion |
| `core.progress.publish`, crash after a step commit | F25/F26 (superseded): status answers from memory during a flood; step rows survive a crash up to the last committed step |
| byte splitter proptest | F27: arbitrary splitting/UTF-8/EOF/size cap, exact messages or explicit failure, no panic/unbounded allocation |
| Linux identity/control seam and real process tests | F22: uid/start/boot/group/marker mismatch never commands cleanup; forged challenge refused; spawn/anchor-death race yields no unrelated signal; marker checks never read vendor environment; leader exit alone never quiescent |

Implement scenario test names `s1_f01_...` through `s1_f30_...`, plus
`s1_bounds_...`, `s1_store_...` for packet-specific assertions.
F1–F30's remaining requirements retain the S1 plan's acceptance; the matrix
above adds sharper seam assertions, not replacements. Test harness output
goes under gitignored `scratchpad/`; every scenario emits the coding-standard
§10 summary, manifest, consistent SQLite backup, event logs and report.
Never copy a live WAL database as its alleged consistent backup.

Exact implementation gates from repository root (these are required future
commands, not claims that tests/features exist today):

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace
cargo deny check
python3 scripts/check-layers.py
cargo clippy --locked --workspace --all-targets --features via-cli/test-failpoints -- -D warnings
cargo nextest run --locked --workspace --features via-cli/test-failpoints
cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08|09|10|12)_/)'
cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_(f2[4567]|bounds|store)_/)'
cargo build --locked --release -p via-cli --no-default-features
python3 scripts/check-release-features.py target/release/via
```

`scripts/check-release-features.py` is a **planned S1 test-harness deliverable**:
check Cargo's release feature graph excludes `test-failpoints` and the fake
binary; launch release VIA with every known activation input and assert it
ignores them; scan the artifact for unique failpoint control marker strings
as supporting evidence. A string scan alone is not proof of exclusion.
The implementation report must record commands, durations, nonempty test
counts, actual feature graph and fixture/artifact hashes. Remove the scaffold
empty-suite exception once tests exist. Do not count intentionally red
prerequisite tests as S1 runtime acceptance.

Design-only validation: Markdown relative links, cited existing paths (planned
paths explicitly labelled), diff/public-repository hygiene and unchanged layer
checker. Sol review must disposition every issue in §10 and the limitations
in §§5–8 before dependent code is dispatched.
