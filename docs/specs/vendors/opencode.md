# OpenCode serve adapter contract

Status: **design candidate, 2026-09-26, via-4sw.2.1**. The pinned free-model
vendor probe now demonstrates result, continuity and bounded active-tool abort;
actual VIA qualification remains required in `via-4sw.3.4`. Sol review of the
refreshed evidence/parameter disposition remains required. The owner delegated
password handling on 2026-09-26; the coordinator selected the temporary
OC-SEC-1 exception in §2.1. Its controls still require proof. No implementation or
release acceptance follows from this document. The first release still
requires Claude Code, Codex **and OpenCode**, including successful real
spawn/result, conversation-preserving resume and supported controls.

Authority: [C1](../via-api-v1.md), [C2](../adapter-contract.md),
[runtime contracts](../runtime-contracts.md), [platform contract](../platform-packaging.md),
[goal](../../workstreams/rust-foundation/goal.md), and
[credential/platform invariants](../../../.repo-context/invariants.md).
This packet resolves proposed B4/B7/A4/A8/P11 mappings where evidence permits;
unverified behavior remains an explicit test gate, not an invented refusal.

## 1. Evidence boundary

Pinned binary: **OpenCode 1.18.32**, SHA-256
`513f500a1a5ea1dc7d865547ac87b32a8936334e8d5abd5b3ff585c45a170080`.
The local served OpenAPI 3.1 document has SHA-256
`46db986090aae41846cd6dbe16225a1d883f0bbcb4c48814008d3f6ce140aa5c`.
It, the raw SSE samples and machine-readable verdicts were inspected for this
design. The private evidence packet is under
`scratchpad/execution/rust-foundation-release/opencode-evidence/`; that directory
is an ephemeral local evidence location, not a public fixture dependency.
The served schema is stronger version evidence than moving website examples.

| Evidence | What is established | What is not established |
|---|---|---|
| Final no-model probe, 2026-09-26, WSL2 x86-64 | 14 passing cases: owned loopback listener, auth rejection, health/version, served schema, SSE connection/session event, create/get/status/empty messages/404, idle abort, private DB and server cleanup | Working model, conversation continuity, usage intervals, active abort/tool cleanup, all ambient config isolation, macOS |
| Default free model attempt | HTTP 200 carried assistant `APIError`, provider status 403 and `FreeTierError`; no answer | Successful result or support status of the serve route |
| Explicit second model attempt | Request timed out after 75 s; offline sanitized observation found user + incomplete assistant, no completed result; request was not resent | Whether the provider accepted/completed later, entitlement failure, continuity or cancellation |
| Free no-login conversation, `free-live-20260926T125743Z` | 12 pass, 2 unproven; pinned 1.18.32 `opencode/mimo-v2.6-flash-free`; async 204 then correlated completed answer; same-session second turn recovered a marker omitted from its prompt; model-only abort reached `MessageAbortedError` | Actual VIA adapter behavior, multi-step usage scope, broad cleanup/config/isolation |
| Free active-tool abort, `free-tool-20260926T130006Z` | 9 pass; observed running Bash `sleep 20`, abort acknowledgement, correlated abort terminal and the observed sleep child absent before server shutdown | Every tool/escaped descendant, general quiescence, VIA cancellation/cleanup |

The free-route report and machine summaries/cases are retained under
`scratchpad/execution/rust-foundation-release/opencode-evidence/free-tier-report.md`
and its named run directories. The successful probe used fresh private HOME/XDG
roots/DB and the vendor's no-login free route, without paid credentials or
provenance spoofing. Both owned servers/groups/listeners were absent after
cleanup; the active-tool check observed the sleep child absent before shutdown.
These are vendor protocol observations on the recorded WSL2 environment, not
VIA Core/Host/Wire/Store or Linux 5.15 release qualification.

Earlier 403/timeout remain failed infrastructure evidence; model, request shape
and auth context changed together, so the cause of the earlier refusal is not
identified. They no longer establish a currently missing working free route.
The live qualification still requires a free model; no paid-model/provider
substitute without new owner authority. `via-4sw.3.4` must run the actual VIA
route, complete the remaining cases and record free-access basis. No new
provider attempt is authorized or performed by this evidence refresh.

Primary sources below are pinned to `v1.18.32`; changing facts were checked
2026-09-26. Claims marked **schema/source** describe interfaces or code paths,
not completed live behavior. [Release](https://github.com/anomalyco/opencode/releases/tag/v1.18.32),
[HTTP session schema](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/server/routes/instance/httpapi/groups/session.ts).

## 2. Ownership, server identity and private state — B7/A8/P11

Use route `opencode-serve`; the revised containment candidate assigns one
VIA-owned OpenCode HTTP server and private DB namespace to one VIA session.
**Cross-VIA-session sharing is disabled**, including identical model/cwd/bound
requests. This replaces the initial A8 sharing proposal because a vendor tool
can inherit the server-wide Basic credential (§2.1). It reduces the directly
exposed API set. The inherited generated-password exposure is now explicitly
accepted as a temporary owner-authorized exception; selected controls remain
a qualification gate (§2.1).
Core owns turn admission, receipts,
queues, Store and final states. The Adapter owns OpenCode semantics and the
server key; Routes owns typed HTTP/SSE correlation; Wire owns sockets, HTTP
message splitting/raw capture and bounded staging; Host exclusively starts/supervises
the server through the reviewed anchor mechanism. SQLite in OpenCode is
vendor-owned storage, never a second writer to VIA's Store.

Server key, persisted with each session:

```text
(route_revision, resolved_binary_identity, exact_vendor_version,
 canonical_cwd, provider_profile_id, provider_profile_epoch,
 generated_config_digest, environment_policy_revision,
 full_effective_bound, owning_via_session_id, private_storage_namespace)
```

Allocate the namespace once per new VIA session, in the same durable Store
transaction as its owner/key record and **before vendor session creation**.
Generate an opaque random namespace ID; store a unique owner-session mapping
and validated relative directory locator. An idempotent spawn replay and
every resume/restart load this mapping; they never allocate a replacement.
Missing mapping or a mismatching owner fails recovery before vendor I/O.
The final server key is constructed at open_session after Core assigns the
owner ID; describe uses the key policy without allocating a namespace.
The namespace maps to private DB/config/state paths beneath the VIA-owned
storage root. The process generation, listener port and random password
belong to a server instance, not the logical key. Credentials and their hashes
never appear in the key. The selected profile is
`provider_profile_id = "opencode-free-anonymous-v1"`, `provider_profile_epoch = 1`:
no saved login, private HOME and every XDG root, vendor-managed public fallback,
and a verified free model. It does not identify an ambient login location.
Namespace identity distinguishes the per-owner private data directories;
environment_policy_revision includes this no-login recipe. Reopen/restart loads
the same profile/namespace, never discovers a caller login or migrates auth.
A profile epoch change requires a new server; no login-backed profile is
implemented by this design. Freeze the key while sessions exist. Bound, cwd/profile or
configuration-key changes do not migrate a live vendor session.

**P11 decision candidate:** include the entire effective bound, including
network and extra directories, even though v1 admits only `full/network:true`.
A changed bound on resume is `bound_unsupported` before vendor I/O.
Nonempty `extra_write_dirs` with `full` is deterministically rejected as
`invalid_params` before any namespace allocation, process or request: it has
no confinement meaning in this route. There is no cross-directory or
cross-VIA-session shared event bus. Keep the existing server-route control
semantics: cancel/close does not acquire a new right to force-kill a server
merely because this revision dedicates it to one VIA session. Server shutdown
is a separate zero-reference
idle or daemon-shutdown operation with its own Host evidence.

### Listener and authentication

Launch argv `opencode serve --pure --hostname 127.0.0.1 --port <explicit-port>`
with explicit cwd and reviewed environment. Select a nonzero available loopback
port, release the temporary reservation only to launch, and accept that a bind
race remains: on collision, fail this startup without attaching to or killing
the occupant. The caller may initiate a fresh startup; do not add an unbounded
port-retry loop. Never attach to an already listening server.

Pinned listener source makes `--port 0` try 4096 first; it is not a guarantee
of an ephemeral port. Read the launched process's bounded startup output,
require the expected numeric loopback address/port, verify Host process and
listener provenance, then use a fresh unpredictable 256-bit password for
authenticated health/version. Same password must not be reused for another
server generation. Linux listener/PID provenance was probed; equivalent macOS
provenance remains required platform work. No prompt or session mutation is
sent before ownership/auth/version checks pass.
[Listener implementation](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/server/server.ts).

Use `OPENCODE_SERVER_USERNAME=opencode` and a nonempty generated
`OPENCODE_SERVER_PASSWORD`; HTTP Basic authentication goes only in headers to
that exact loopback origin. Disable HTTP redirects and proxies for this local
client, including inherited proxy environment handling. Never put passwords
in URLs, Store, manifests, traces, HTTP raw captures or command argv. Redact
Authorization before HTTP metadata enters a raw log; retain vendor body bytes
and message-splitting evidence. VIA creates the password only in daemon memory and the
owned server launch environment. That is a VIA handling rule, **not a claim
that the vendor keeps it from descendants**; the source-backed exposure below
precludes that assertion under the temporary exception. Restart cannot recover
it by inspecting process env.
Unauthenticated and wrong-password requests must fail; a missing password is
a startup failure, not an unauthenticated fallback.
[Server auth](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/server/auth.ts).

### 2.1 Server credential exposure: temporary owner-authorized exception

**Source-verified chain, runtime presence untested:** the pinned Bash tool
constructs a child without an explicitly scrubbed environment, and its spawner
passes the configured environment or host inheritance. Server Basic Auth uses
`OPENCODE_SERVER_PASSWORD` from that host environment. A shell can plausibly
obtain the credential without deliberately inspecting another process. Basic
Auth authenticates the server, not one VIA session; a shared server would
therefore expose sibling sessions through ordinary credential inheritance.
Do not inspect an actual secret to confirm this. [Bash tool](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/core/src/tool/bash.ts),
[child spawner](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/core/src/cross-spawn-spawner.ts),
[authorization middleware](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/server/routes/instance/httpapi/middleware/authorization.ts).

**Approved boundary:** [C1 §2](../via-api-v1.md)
explicitly excludes defence against same-user processes reading caller memory
or files. [C2 A4/D9](../adapter-contract.md) supports only OpenCode `full` and
leaves external sandboxing undecided. The goal's no-cross-session-traffic
requirement still requires correct ownership, authorization and demultiplexing;
it does not establish adversarial isolation between processes with the same OS
user authority. This draft consequently does **not** require preventing all
same-user retrieval of another process's secrets. The previous broader
OC-SEC-1 wording imposed a stronger boundary than these approved contracts.

The one-owner server/DB mapping above is a concrete containment change:
there is no unrelated VIA session served under the directly inherited password.
All vendor child sessions belong to that owner and consume its limits. This
controls direct sibling API authority; it is not a filesystem/process sandbox
and does not remove the credential from child environments. An ordinary
command that reports its environment could still expose its instance password
in model-visible output. VIA must not itself put the secret in argv, persistent
state, HTTP metadata/raw headers, diagnostics or fixture output.

**OC-SEC-1 disposition, 2026-09-26:** the owner delegated the password
handling choice; the coordinator selected option B as a temporary material
exception to C1 §9's descendant-confinement promise. This is not a reinterpretation
of the earlier “only” wording. The initial High finding and subsequent Sol
review remain valid historical evidence: ordinary child inheritance was not
prevented, and a complete supported scrub was not established. The exception
permits inheritance of this VIA-generated server-instance password only, for
`full/network:true` with one VIA session per owned server. It grants no right
to read, copy, forward or log user/provider credentials, inspect actual secret
values, weaken Basic Auth or share a server across VIA owners. Anchor tokens
remain excluded from vendor environments. Same-user adversarial isolation is
not added to the full-bound contract.

Design acceptance no longer waits for a complete tool-environment scrub or
OS isolation. It requires scoped Sol review of this explicit exception and
its controls. Implementation/release acceptance still requires OC01/OC02/OC12:

- Every instance has a fresh unpredictable secret; unauthenticated,
  wrong-secret and other-owner-secret requests fail, while correct auth works.
  Provenance/bind collision checks prevent foreign attachment; redirects,
  proxies and unauthenticated fallback are disabled.
- Each owner has its own persistent namespace/DB/server mapping. A's API must
  not serve B's sessions; A's credential must fail on B. Child sessions remain
  under their owner; no cross-owner event/control routing is admitted.
- VIA injects the generated secret only into the owned launch environment;
  never puts it in argv, Store, keys, manifests, HTTP captures/metadata or
  diagnostics, and strips Authorization before transport logging. Tests use
  only synthetic secrets and report Boolean leak checks, never values.
- Restart rotates generation/password while retaining the correct owner's
  namespace/history. No process-environment secret recovery; no uncertain
  resend. Four-server budgets and ordinary control semantics remain unchanged.

This is tested authentication/ownership/handling, **not** proof that vendor
tools cannot obtain or emit their inherited instance secret. Arbitrary
model-controlled output is not made secret-free by this exception. VIA must
not initiate environment/credential dumps or copy provider authentication
material; fixtures and reports must use nonsecret synthetic data. Keep the
inherited-password limitation explicit in usage/release documentation. Do not
claim an environment scrub or sanitization of arbitrary vendor payloads.

**Hardening follow-up / revisit:** track a supported version-pinned child-env
scrub across every enabled spawn path. Revisit this temporary disposition at
the next vendor-pin/adapter-security review or before enabling any cross-owner
sharing, remote/public listener, narrower advertised isolation or credential
reuse. Those changes are not authorized by this exception. If wrong-owner auth,
foreign attachment or VIA's credential-handling checks fail, qualification
fails; the exception does not excuse them. Revisit immediately if evidence
shows VIA itself exposes user/provider credentials.

A later bounded inheritance fixture may use only nonsecret runtime necessities
and `VIA_TEST_INHERITANCE_SENTINEL=present`, returning Boolean presence rather
than an environment dump. Positive inheritance and negative explicit omission
controls demonstrate the mechanism without actual credentials. This is useful
hardening evidence, not a new complete-scrub release gate. No fixture or new
provider/server/model call was run for this disposition. Existing successful
actual VIA free-model result/continuity/control qualification, multi-step
usage and B7 gates remain. `max_steps` follows the supported refusal contract
in §3; paid substitution requires a new owner decision.
Linux proof remains required; macOS artifact production, linkage inspection
and native qualification are deferred together under the
[platform contract](../platform-packaging.md).

### Environment: selected private no-login free profile

Production v1 selects the same **private, no-login split** as the successful
free probe. Start from an empty environment and construct only these keys.
New owner namespaces are fresh; idle restart retains only that owner's private
namespace and history. The full generated permission/config recipe still needs
B7/VIA qualification; the probe did not prove complete config isolation:

| Keys | Value / ownership |
|---|---|
| `PATH` | Reviewed executable search path needed by approved vendor tools; explicit value, not arbitrary caller additions |
| `HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `TMPDIR` | Fresh VIA-owned private directories, mode 0700, scoped to the persistent owner namespace; never resolve or inherit the caller's vendor login data root |
| `OPENCODE_DB` | Absolute path to the namespace's private OpenCode DB; never the ordinary interactive session DB or `:memory:` |
| `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR` | VIA-generated private config/file paths; fixed digest in key |
| `OPENCODE_DISABLE_PROJECT_CONFIG`, `OPENCODE_DISABLE_AUTOUPDATE`, `OPENCODE_DISABLE_PRUNE` | `1`; source-supported policy switches, to be exercised in isolation tests |
| `OPENCODE_SERVER_USERNAME`, `OPENCODE_SERVER_PASSWORD` | Fixed username and generated instance secret as above |
| VIA vendor marker | Launch-only marker; distinct from Host anchor's private marker |

Do not forward provider API-key variables, `OPENCODE_AUTH_CONTENT`, console
tokens, arbitrary `OPENCODE_*`, proxy credentials, telemetry headers or
caller config-content strings. If a deployment needs extra nonsecret TLS or
proxy settings, review a new explicit profile/allowlist; never inherit broadly
to repair an auth failure. VIA never reads, copies, hashes or extracts any
user/provider credential or vendor auth file. Do not discover, mount, symlink
or forward saved-auth locations from the caller's HOME/XDG directories. A
missing/unavailable free route is an explicit vendor/infrastructure outcome,
not permission to use a saved login or paid model.

The pinned vendor's provider implementation supplies its own `public` fallback
when no credential exists; VIA neither injects a provider API key nor spoofs
client provenance. The demonstrated model is `opencode/mimo-v2.6-flash-free`.
Other model eligibility requires separately verified free access; there is no
silent model/provider substitution. `provider_profile_id` above is an anonymous
policy identity, not a user identity or a hash of credentials. A future optional
vendor-login profile would require explicit selection, a distinct key/epoch and
separate evidence; implementing it is outside this first no-login recipe.

Pinned sources explain why every root is private: authentication is found under
the vendor data root; `OPENCODE_DB` redirects only the session database; project
config disabling does not remove home `.opencode` discovery. `--pure` removes
external plugins, not every config source. B7 must test that hostile caller
HOME/XDG/auth/config and project settings cannot silently select ambient login
or override the generated never-ask policy under this recipe. Use only synthetic
sentinels/fake auth fixtures; never inspect a real login. Also test idle reopen
and restart with the same anonymous profile. Unexpected saved-auth state in a
private anonymous profile must fail qualification/startup rather than be used
as an implicit login; detect known auth-location presence via safe metadata,
not credential contents, and qualify the complete relevant location inventory.
This protects accidental profile selection, not adversarial same-user access.

The selected data root contains this owner's vendor logs/repositories as well
as any vendor data; it is now inside the private namespace. That is stronger
placement control than the initial managed-root proposal, not proof that all
full-bound tools or vendor config are OS-confined. Never claim a generated-config
digest covers every vendor-resolved setting. These rules deliberately replace
the earlier managed-login-root design so the production profile matches the
successful probe's auth/environment basis.
[Provider fallback](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/provider/provider.ts),
[Auth implementation](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/auth/index.ts),
[vendor paths](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/core/src/global.ts),
[config discovery](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/config/paths.ts),
[config flags](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/core/src/flag/flag.ts),
[database selection](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/core/src/database/database.ts).

Keep the private DB across idle server restarts so vendor session IDs/history
remain available. Only one owned server generation can open that namespace;
hold its VIA registry lock through verified previous-generation shutdown.
Never read the live vendor DB to assemble results; use HTTP. Consistent vendor
DB backups for tests use a vendor-supported/quiescent backup procedure, not a
copy of a live WAL main file. Cleanup deletes only an explicitly disposable
test namespace; normal close preserves history. Namespace ownership never
changes when a server goes idle: a replacement process uses the same owner ID,
DB and vendor session, with a new generation/port/password. Idle namespaces
do not reserve ports or server memory. Reserve at most four live servers,
four listeners and four SSE streams daemon-wide for OpenCode; a fifth session
waits under Core admission or uses its remaining deadline. Do not evict a
server with active/uncertain work to make space. Each live server has one VIA
owner and one active top-level turn; owned vendor child-session metadata is
bounded to 32 records and never admits another VIA owner. These caps consume
the common daemon memory/process permits; they are not extra unbudgeted pools.
Production measurements must include four separate vendor processes (their
memory is external to VIA's Rust buffers). No claim of equivalent sharing
memory cost is made. Port exhaustion/bind collision is startup failure, and
OC-SEC-1 disposition and selected-control proof remain required despite these caps.

## 3. Capabilities, bounds and version gate

This table is the **target declaration after its tests pass**, not today's
tested capability matrix. Pin tested range to exactly 1.18.32 initially; no
semver range inference. No-model protocol evidence alone does not make the
complete adapter tested. `describe` reads installed/catalog metadata without
starting a process, server or writing files; version discovery occurs during
explicit harness discovery/startup and is cached. No hidden `--version`
subprocess inside the no-process `describe` path. Unknown/changed binary
identity invalidates that cache; actual startup health must match the pin.
A2/P13 use `untested` plus explicit `allow_untested` for bound-bearing calls;
handshake/schema failure is `harness_unavailable`. `allow_untested` never
bypasses credential, never-ask or unsupported-bound validation.

| Surface | Planned support and mapping | Evidence gate |
|---|---|---|
| Spawn / resume | native create + same vendor session prompt | Vendor free-model result/two-turn continuity observed; actual VIA qualification remains |
| Steer | unsupported: no demonstrated active-turn injection verb; sending another prompt is not steer | No busy-prompt emulation |
| Cancel | native abort request/acknowledgement with separate cleanup certainty | One vendor sleep-child abort observed; VIA/sibling/escaped-descendant cases remain; idle `true` is insufficient |
| Close | native logical detach; active work follows existing server-route cancel/close policy | No destructive session DELETE and no unreviewed force-kill capability |
| Instructions | native `system` sent from frozen session instructions on every turn | Persisted user message + live semantics check |
| Output schema | native `format:{type:"json_schema",schema,retryCount:0}`; null means explicit `{type:"text"}` | Validate C1 schema first; live structured response/missing/error cases |
| Effort | native only for a model's verified exact `variant` mapping | Unknown canonical/vendor value refused; no assumption that every model has low/medium/high |
| Max steps | unsupported on this pinned route; explicit non-null request refused before dispatch | C1 §4.1 permits per-parameter unsupported status; refusal proof below |
| Recovery | unsupported live rejoin after daemon restart (P12) | Active turns remain unknown unless independent evidence supports C1 terminal mapping |
| Usage / cost | reported `vendor_interval` until measured; missing values unavailable | Never infer turn scope from empty-session zeros |

**A4 decision candidate:** only `full` with `network:true`. Refuse
`read_only`, `workspace_write` and every `network:false` request before server
creation/submission as `bound_unsupported`. `network:true` permits networking;
it does not assert that egress works. Tool permissions and `--pure` are not OS
sandboxes. No external sandbox or sandbox-enforced capability is added here.
The exact never-ask candidate below allows ordinary full-bound actions and
denies interactive question/plan-mode prompts; reject any remaining request
for approval or interactive input as §5 specifies. OC-SEC-1 control qualification still gates using
these tools; permission rules do not solve credential inheritance.

### Exact permission payload and precedence gate

Generate private config with this permission object globally and on the
explicit `via` primary agent (`default_agent:"via"`). Configuration is
immutable for the server generation:

```json
{"permission":{"*":"allow","question":"deny","plan_enter":"deny","plan_exit":"deny"},
 "default_agent":"via",
 "agent":{"via":{"mode":"primary","permission":{"*":"allow","question":"deny","plan_enter":"deny","plan_exit":"deny"}}}}
```

Create the vendor session with `agent:"via"` and this exact ordered
`permission` array; send `agent:"via"` on every prompt. Wildcard comes first,
specific denials last:

```json
[{"permission":"*","pattern":"*","action":"allow"},
 {"permission":"question","pattern":"*","action":"deny"},
 {"permission":"plan_enter","pattern":"*","action":"deny"},
 {"permission":"plan_exit","pattern":"*","action":"deny"}]
```

Pinned V1 permission evaluation chooses the last matching rule. Its tool
context merges agent rules first and session rules last; therefore this
session wildcard permits normal bash/read/edit/network actions even if an
earlier agent default would ask, while the trailing exact denials remain
effective. Do not send deprecated per-prompt `tools` overrides, which can
replace the session permission array. [Permission evaluation](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/permission/index.ts),
[tool ruleset construction](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/session/tools.ts).

Before first submission and on every idle reopen, require the vendor's session
readback to match the exact ordered ruleset and require the selected agent's
public metadata to identify `via` as primary with the intended permission
rules; retain only those nonsecret fields. Do not fetch/dump provider config
or auth data for this check. Source evidence for the V1 merge path does not
prove every V2/remote/organization/tool path obeys it. B7 must exercise hostile
global, project, home, agent and possible remote configuration with synthetic
source fixtures under the selected anonymous profile:
ordinary tool actions remain available, question/plan requests are denied,
unknown approval requests get bounded rejection, and session rules are never
silently replaced. If effective policy cannot be established, fail startup
(or stop dispatch on reopen) before a prompt, without weakening the policy.
Reactive request rejection is fallback defense, not policy/precedence proof.

**Max-steps disposition for Sol review:** served PromptPayload has no
per-turn max-steps field. Pinned prompt code uses agent `steps` plus a final-step
prompt; no faithful per-turn mapping is established. Declare
`params.max_steps:{support:"unsupported",reason:"No qualified per-turn step limit on opencode-serve 1.18.32"}`.
Non-null effective `max_steps` is refused by Core preflight as `invalid_params`
(JSON-RPC -32602), with a message naming the field and route, before namespace
allocation, vendor creation or submission. Do not invent `unsupported_verb` or
use `missing_capability` (the latter applies to `require` verbs). No new closed
`kind2` value is implied. Null/omitted max_steps remains usable; normal C1
inheritance/clearing applies, and an unsupported non-null value is never accepted
into a receipt. `allow_untested` cannot bypass the refusal. Wall/idle deadlines
remain independent; never use a timeout/tool count as a substitute step limit.

This is an ordinary truthful parameter refusal, **not an owner scope waiver**:
[C1 §4.1](../via-api-v1.md) expressly permits unsupported parameter capability
entries with a reason; C1 §4 defines max_steps as an optional nullable per-turn
parameter. [C2 §6.2](../adapter-contract.md) already records explicit max_steps
as unsupported for Codex. The [goal's product surface](../../workstreams/rust-foundation/goal.md)
requires full C1 methods, all three adapters, real spawn/resume/result and every
supported control; it does not promise native support for every parameter on
every route. The earlier automatic owner-disposition release blocker was too
strong. Required proof is honest describe/receipt capability plus pre-I/O refusal
on spawn/resume, null/omitted acceptance and no silent parameter drop, with Sol
review of this mapping. New owner scope approval is needed only if full support
is separately promised or required, not for this existing capability mechanism.
[Prompt implementation](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/session/prompt.ts).

V1 vendor-option allowlist is empty. Reject supplied vendor keys rather than
pass through arbitrary server configuration. In particular reserve listener,
auth, database/home/config, plugin/MCP, permission/tool flags, agent, cwd,
model, variant, system, format, session/message identity, `--auto`,
`--continue`, `--session`, `--dir` and all canonical-parameter equivalents.

## 4. Methods and exact turn correlation — B4

| C1/C2 operation | Vendor operation / local behavior |
|---|---|
| `hello`, `describe`, `models` | VIA metadata/catalog; model discovery on a previously owned authenticated server can use `GET /provider`, keeping only public model/capability fields |
| `open_session` / spawn | Subscribe SSE first; `POST /session?directory=<cwd>` with explicit model/agent and full-bound never-ask permission policy; persist returned `ses…` ID before prompt |
| `StartTurn` | One `POST /session/{id}/prompt_async` with frozen request below; HTTP 204 is vendor dispatch acceptance, never completion |
| Resume / idle reopen | Reuse recorded namespace + exact vendor ID; `GET /session/{id}` verifies identity/directory; then one new StartTurn. Missing/mismatching ID fails, never silently create/fork |
| Steer | Named unsupported-verb refusal, no vendor request |
| Interrupt / cancel | `POST /session/{id}/abort`; interpret active cancellation evidence as §6 |
| Close | Cancel/drain according to mode/deadline, release session driver/subscriptions/reference; preserve vendor session/DB |
| `status`, `wait`, `result`, `list`, `events`, `unsubscribe`, `logs` | Core's durable VIA state/event/raw APIs. Vendor reads are observations, not an alternate public state source |
| `daemon/status`, `daemon/stop` | Core registry and reviewed shutdown; Host stops only VIA-owned server groups during whole-server shutdown |

Use one explicit cwd selector consistently on every request and SSE connection;
do not mix `directory`, workspace defaults and process cwd. Percent-encode path
and query components, never interpolate an arbitrary URL. A server-instance
session registry maps `(server generation, vendor session ID)` to VIA session;
unknown IDs cannot route into another session's stream or result.

Before submission, Core persists the canonical turn/submission intent. The
driver allocates one unique caller `messageID` accepted by the pinned schema
and retains its mapping to the VIA session/turn while alive. Wire's outbound
raw evidence records the request; C2 acceptance subsequently carries the
message ID as vendor-turn correlation for Core to persist. No Adapter Store
access or new pre-submission callback is implied. Recovery without a committed
mapping remains P12 unknown rather than reconstructing/resending a prompt.
Test generated IDs
against vendor ordering/retention semantics, not just the `^msg` schema regex.
Body uses `parts:[{type:"text",text:prompt}]`, explicit model
`{providerID,modelID}`, the fixed approved agent, and explicit inherited
parameter values. Do not enable `noReply`, insert file/subtask/agent parts,
expand prompt text as a CLI command or alter session model on resume.

Allocate one AcceptanceToken before the POST. A 204 or a matching persisted
user message/event is acceptance evidence; deduplicate both with that token.
No response after a possibly sent request is `Unknown` unless later read-only
evidence resolves it; never automatically repeat a POST, even with the same
caller message ID. A read-only lookup may find acceptance, but duplicate-ID
idempotence is not assumed. HTTP 4xx schema/not-found errors count as rejection
only where the pinned handler establishes no submission; 5xx/transport loss
after dispatch is ambiguous. An HTTP 200 assistant error in synchronous
evidence is not success; the production route remains async.

Each assistant must match session ID and `parentID` of the submitted user
message. Multiple assistant messages/steps may belong to one turn; intermediate
`finish:"tool-calls"`, a step-finish part or idle status alone is not a VIA
terminal. Candidate terminal requires correlated completed assistant/error,
no further active continuation for that input, and reconciled session state.
Use `GET /session/{id}/message/{messageID}` or bounded message pagination to
reconcile parts and assistant parents. Keep Core's one-active-turn gate; never
send queued prompts while a previous turn or cleanup is unresolved. Missing
completion metadata, a status-map omission or an incomplete assistant produces
continued waiting/unknown at deadline, never invented final text.

Source handles prompt submission asynchronously and exposes bounded pagination
through `limit`/`before` and `X-Next-Cursor`. Set `limit=100`, never zero (zero
means unbounded in the pinned handler); cap each decoded response at 16 MiB
and reconciliation to the turn deadline and 1,000 pages. Cursor cycles, oversized
responses or exhausted bounds are explicit incomplete/protocol outcomes.
Live tests must establish the exact successful-terminal sequence before
declaring this mapping native. [HTTP handlers](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/server/routes/instance/httpapi/handlers/session.ts).

## 5. SSE, decline handling and bounded operation

Wire parses SSE incrementally: UTF-8 boundaries, CRLF/LF, comments, multiple
`data:` lines and omitted `event:` (default message) all work. Decode JSON
`{id,type,properties}`; subscribe before session mutation. `server.connected`
proves stream setup; `server.heartbeat` is transport liveness, not progress
that extends a work idle deadline. Use the legacy `message.*`/`session.*`
event family described by the served schema as canonical; future/parallel
`session.next.*` events are retained as bounded `vendor.other`, never a second
text/tool/usage emission. Permission/question request aliases must be decoded
and deduplicated by request ID even if represented in both event families.
Source directory filtering and the observed default SSE message are independently
required fixture cases. [Event handler](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/server/routes/instance/httpapi/handlers/event.ts).

Map text deltas and authoritative part snapshots without appending the same
text twice. Correlate tools by session/message/part/call IDs; state is
pending/running/completed/error. Keep partial text distinct from final text.
Unknown notification types become bounded `vendor.other`; malformed known
payloads fail the connection. An unrouteable mutation/request never guesses a
session. Deduplicate known vendor event IDs with a bounded 4,096-entry window;
do not promise exactly-once vendor delivery beyond it. Core durable event
ordering remains authoritative.

Never-ask control path: `permission.asked` →
`POST /permission/{requestID}/reply` with `{"reply":"reject"}`;
`question.asked` → `POST /question/{requestID}/reject`. The deprecated
session permission endpoint is not the primary mapping. All declines get
`vendor.request_declined`; observed action denials get `action.denied` with
accurate provenance, not an invented denial merely because a prompt was sent.
Reply within the minimum of remaining operation budget and 1 s via the
independent control lane. Duplicate request IDs coalesce. A 404 reply means
the request is no longer present, not proof of a successful rejection. An
unknown interactive request or failed decline triggers session abort and
explicit protocol/control failure; it must not hang or be auto-approved.
Permission traffic must stay serviceable when ordinary observations fill.

Reuse runtime/C2 bounds: 1,024 items and 4 MiB per session; 256 KiB normalized
observation maximum; 10 s event stall; independent bounded control/health.
Set SSE line and assembled data-event limits to 16 MiB, HTTP headers to
64 KiB, decoded response bodies to 16 MiB; reject oversized compressed output
based on decoded bytes too. At most four OpenCode server instances, one SSE
stream per server key, and one active prompt per session; server counts and
buffers consume daemon-wide admission/memory permits, not a separate unlimited
pool. Reserve control capacity independent of reads/model work. Measure and
review total resource budgets with all three adapters before integration.

The daemon's SSE dispatcher must not let one dedicated server/owner's saturated
queue stall another server/owner. Dispatch with
nonblocking per-session admission; when a session exhausts its lane, quarantine
that session immediately, retain its original turn correlation, and deliver
overflow/loss through reserved sticky health/control. Continue other sessions
and control messages. Subsequent quarantined data remains raw-only; its missing
normalized sequence is explicit and never resumed as complete. Do not allocate
a spill queue or block other server parsers for the 10 s session deadline.
The 32 records per server are owned vendor child-session/correlation metadata,
not 32 independently admitted VIA sessions. If global metadata/control/raw
capacity is exhausted, fail affected connections explicitly. OC09 must flood
owner A's dedicated server while owner B completes and abort responses
arrive before advancing fake time to the stall deadline.

SSE disconnect has no proven durable replay guarantee. Do not reconnect with
`Last-Event-ID` and pretend events/raw bytes were recovered. Mark affected
streams incomplete, stop new dispatch, preserve raw evidence and use bounded
status/message reads only to classify terminal/unknown outcomes. Fail the
server connection explicitly when exact log capture cannot keep up; its
owner receives honest loss evidence. One session's cancel never
kills the server as a backpressure workaround. No server-internal unbounded
queue is counted as VIA's bounded memory; slow-consumer tests measure the
actual vendor behavior and document that external-process limitation.

## 6. Abort, cleanup, continuity and recovery

An HTTP 200 `true` from abort is command acknowledgement only. With an active
turn, combine it with correlated interrupted/error terminal evidence before
claiming cancel completion. `MessageAbortedError` after VIA interrupt supports
interrupted outcome; unsolicited abort is reported as vendor evidence, not
invented VIA cancellation. Track all observed tool items: open items are
`pending`; missing/uncertain observations or tools surviving abort are
`uncertain`. Only complete, evidenced tool completion supports quiescence;
an abort Boolean, idle map or socket closure does not. Active-tool live
descendant tests must establish whether terminal tool records are sufficient
for this pinned route; until then never promote aborted-tool metadata to proof
that external side effects stopped.

Preserve the reviewed server-route behavior: the driver does not ask Host to
kill the server for cancel/force-close merely because the server is now
dedicated. At the deadline return unknown cancellation
or uncertain cleanup as C1/C2 require. Core applies P7's cleanup gate before
the next turn. Close detaches logical ownership and preserves vendor history;
`DELETE /session/{id}` is not a close implementation. Cancel of an undispatched
queued turn is Core-local and sends no abort for the currently running turn.

Idle re-open starts only a new VIA-owned same-key server over the retained DB,
then verifies the old vendor ID before continuing. Two-turn and restart tests
must demonstrate preserved context, not just equal session IDs. Private DB
existence alone proves neither continuity nor supported resume.

After daemon crash, in-flight work follows P12 `unknown`; no optional live
rejoin is introduced. Generated HTTP password is intentionally unavailable
after restart, so recovery uses Host anchor records for old-process cleanup,
not credentials extracted from its environment. Server death observed while
the original daemon is supervising informs C1 `server_lost`; restart recovery
still follows P12 for submitted/accepted turns and never fabricates a known
outcome from a subsequently dead server. Start a replacement only
after exclusive namespace ownership and prior-generation cleanup are verified.
Never send another prompt to reconstruct an uncertain answer. A surviving
unverified server is an orphan, not a server to attach to or signal numerically.

## 7. Usage, errors and observations

Served schema contains assistant token/cost fields and step-finish token/cost
fields; source does not substitute for measuring their accounting intervals.
Use one ledger of authoritative assistant snapshots keyed by message ID;
replace repeated snapshots, do not sum them with step-finish payloads. Retain
step data as vendor evidence for reconciliation. Until two turns plus a
multi-step turn prove intervals, canonical token/cost scope is
`vendor_interval`, provenance `reported`, warning `usage_interval_unverified`.
Never derive cost from token price tables or interpret missing values as zero.
Invalid/nonfinite/negative counts or costs produce protocol/accounting warning
and unavailable canonical values with raw evidence. Cache-read maps to cached
input only after inclusion semantics are measured; preserve cache-write
separately in vendor data. Session totals must not be relabelled turn totals.
Two successful single-step free turns now provide measured observations:
each assistant's step-finish exactly duplicates its assistant token/cost fields,
so adding both would double count. Component sums matched totals 11,197 and 11,261;
the second included cache-read 11,136 and both reported cost 0. This supports the
one-ledger rule on those samples only. Multi-step intervals, cache/billing
inclusion and broader cost scope remain unproven; preserve vendor_interval and
the warning until that proof. Empty-session zeros, failed requests and zero
free-model cost do not establish paid billing semantics.
[Message representation](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.32/packages/opencode/src/session/message-v2.ts).

Adapter class hints: explicit `ProviderAuthError` or another pinned, verified
unauthorized vendor subtype → auth. HTTP status alone does not establish that
class: ambiguous 401/403 and the observed HTTP-200 assistant `APIError` carrying
403 `FreeTierError` map to `vendor_error`, preserving the structured vendor
code/status. Do not infer login failure, quota or paid entitlement from a
status code. A 429 maps to rate_limit only with matching provider evidence;
`ContextOverflowError` → context_exceeded; `MessageOutputLengthError` →
vendor_error unless a verified finish maps a supported stop reason;
`StructuredOutputError` → vendor_error + missing/schema diagnostics;
unknown/API errors → vendor_error with structured source code. Control/HTTP
transport failures map to protocol/loss observations; Host alone confirms
server death. Core applies cancellation precedence and commits C1 classes.
Vendor `isRetryable` and retry-status events are observations, not authority
for VIA to resubmit. The earlier provider 403 remains an infrastructure failure
of that attempt; it no longer blocks the now-demonstrated free route by itself.

## 8. Failure-first acceptance and outstanding gates

All tests run real VIA/Core/Host/Wire/Store with a fake HTTP/SSE vendor first;
live sets use the pinned actual vendor and a working free OpenCode model.
Paid-model/provider substitution requires a new owner decision. Preserve coding-style §10 summaries,
raw references, consistent backups, versions/features and hash manifests.
No credentials or generated server passwords may enter fixtures/reports.

| Case | Required observation |
|---|---|
| OC01 ownership/auth/port | Occupied port and unrelated server remain untouched; no unauthenticated fallback, redirects or proxy credential leak; bad health/version refused; actual owned listener on Linux; corresponding macOS qualification deferred under owner delegation |
| OC02 private state/config | Allocate/persist one namespace per VIA owner before vendor creation; repeated spawn/resume/restart loads it; two otherwise identical sessions never share namespace/process/password. Missing/mismatched mapping fails before I/O. Ordinary vendor session DB unchanged; private HOME/all XDG roots including DATA and DB persist under anonymous profile. Hostile ambient saved-auth/config synthetic fixtures never select login; unexpected private auth-state presence refuses without reading contents. Restart retains no-login identity; generated policy and actual VIA route still require proof |
| OC03 full method path | Successful live spawn/result on a free model; background/wait, reads/events/logs and stdio expose same durable turn; close never deletes vendor history |
| OC04 continuity | Two-turn context-dependent answer on the same free model, equal vendor ID, idle server restart over private DB; missing/mismatching ID refuses fresh-session substitution |
| OC05 ambiguity | Crash before/after POST/204, lost HTTP reply, event acceptance before reply, duplicate IDs, incomplete assistant and HTTP-200 error; exactly one submission, no false completion |
| OC06 correlation/message splitting | Cross-session interleaving, multiple assistant steps, delta+snapshot duplicates, default SSE event, partial UTF-8/multiline SSE messages, unknown events/requests, late events and message parent mismatch; no leak/double terminal |
| OC07 never-ask | Exact ordered create/readback rules; agent-before-session last-match precedence tested under hostile config; normal full-bound tools remain usable, question/plan tools denied. Real/fake request rejects under saturated observations; 404/error truthful; unknown effective policy prevents prompt |
| OC08 control | Active tool abort with descendant liveness observations; tool refuses stop, lost abort response, late terminal; other VIA owner's dedicated server unaffected; queued-turn cancel sends no abort |
| OC09 overload/loss | Four-server memory/listener/SSE limits and fifth-owner admission; flood/oversize/slow consumer, full lane, SSE disconnect, compression expansion; other owners and control progress, loss explicit; idle namespaces consume no listener; nonempty bounded suites |
| OC10 recovery | Server/daemon death during accepted work, unknown state no resend, verified anchor cleanup, namespace lock and password rollover; no attachment to unrelated survivor |
| OC11 parameters/usage | Instructions/format clearing, invalid variants, pre-I/O unsupported max_steps refusal and null/omitted acceptance, multi-step usage scopes; nonempty full-bound extra_write_dirs deterministically invalid_params before allocation/I/O. ProviderAuthError→auth; FreeTierError/ambiguous403→vendor_error with exact code; no entitlement inference |
| OC12 credential boundary | Selected temporary exception is explicit; test correct/wrong/missing and other-owner Basic Auth, exact server/namespace ownership, password rotation, and no generated secret in VIA argv/Store/keys/diagnostics/transport captures. Synthetic credentials only; tests emit Boolean leak checks, not secrets. Never read/copy/log actual user/provider credentials. Inherited instance-password presence is an accepted limitation, not a failed scrub test or proof of same-user isolation |

OC-SEC-1's material exception has owner authority; scoped Sol review and the
selected-control proof remain required. Free-model vendor result/continuity and
one active-tool abort are now observed. Final acceptance still needs actual VIA
free-model conformance in `via-4sw.3.4`, multi-step usage/cache scope, broader
cleanup/restart/load, B7 config/never-ask and model-specific parameter checks.
Unsupported max_steps needs the truthful capability/preflight tests in §3;
no separate owner release waiver is required by the existing C1/C2 contract.
Linux platform execution and integrated three-adapter gates remain mandatory;
macOS artifact production, linkage inspection and native qualification are
coordinator-deferred under the owner's optionalization, not passed. No paid
model/provider may substitute for the free live gate without a new owner
decision. Passing OC01/empty-session cases does not substitute for OC03/04/08. No provider retry,
new login, provisioning or external sandbox experiment is authorized by this
owner-disposition document.


## 9. Exact shared amendments proposed for coordinator integration

These are proposed changes, not edits applied by this worker:

1. **C1 §9 listener wording:** replace “no network listener in v1” with
   “The VIA public C1 API has no network listener in v1; it uses the user-only
   Unix socket/stdio proxy. A VIA-owned vendor server may use a private
   authenticated loopback HTTP listener with verified process/listener
   provenance. VIA never attaches to an unrelated vendor listener.”
2. **C2 §6.2 OpenCode column:** replace `--port 0` with the explicit-port,
   authenticated ownership-checked startup in §2; document retained private
   `OPENCODE_DB`, one-VIA-owner server/namespace containment (sharing disabled)
   and session-close detach. Select anonymous `opencode-free-anonymous-v1`
   epoch1 with private HOME/all XDG roots including DATA, absolute private DB
   and no ambient saved-auth discovery; vendor public fallback only. Login
   profiles are separate future work, not a fallback. Add
   per-turn caller-message correlation, 204 acceptance-only, modern permission
   reject and question reject; retain steer unsupported and P12 unknown.
3. **A4/A8/P11:** record §2/§3 full-only/network refusal and the exact server
   key including VIA owner ID, one durable namespace per session, frozen bound,
   unconditional full-bound extra-directory refusal and no session migration,
   after Sol disposition. No owner may join another owner's server even if
   every other key field matches. C2
   OpenCode usage remains vendor_interval until measured; no live capability
   is marked tested merely from this schema packet.
4. **C2 reserved keys / params:** adopt the empty vendor allowlist and reserve
   auth/listener/storage/config/agent/model/session fields. Record the
   pinned max_steps unsupported capability and Core pre-I/O invalid_params
   refusal from §3; this follows existing C1 parameter capability semantics,
   needs tested behavior/Sol review, and does not need an owner scope waiver.
5. **Runtime extension:** add bounded owned HTTP/SSE server resources and
   credential-redacted transport metadata under existing Routes/Wire/Host
   ownership. This extends S1's private-process fake implementation; it does
   not change Core's durable state or permit adapters to access Store.
6. **C1 §9 temporary generated-password exception:** replace the old
   descendant-confinement wording with the following owner-authorized text,
   after scoped Sol review:
   “VIA never reads, copies, reuses or logs user/provider credentials. For the
   full-bound OpenCode route, VIA generates a fresh password per owned server,
   retains it in daemon memory and injects it only into that server's launch
   environment; VIA does not persist or emit it in argv, diagnostics or
   transport captures/metadata. As a temporary exception, vendor tool children
   may inherit that generated instance password. Each server/password/private
   namespace belongs to one VIA session; cross-VIA-session sharing is
   prohibited. Loopback Basic Auth and listener provenance remain required.
   Full-bound sessions do not isolate hostile same-user processes; this
   exception does not authorize access to user/provider credentials. Revisit
   at the next vendor-pin/security review and before changing sharing,
   listener exposure, credential reuse or the advertised trust boundary.”
   This is an accepted limitation, not a fixed inheritance mechanism. Retain
   OC01/OC02/OC12 control tests, four bounded server slots and durable namespace
   ownership. Pin §3's generated rules/readback/precedence and §7's structured
   auth-error mapping. Track complete supported child-env scrubbing as the
   hardening follow-up; macOS production/inspection/native checks follow the
   deferred platform row rather than blocking the current Linux goal.
7. **Goal/live-probe free-model requirement:** OpenCode successful live result,
   conversation continuity and supported control/usage qualification must use
   a working free model. No paid-model/provider substitution without a new
   owner decision; failed403/timeout remain infrastructure evidence. Apply to
   `via-4sw.1.2`, live qualification leaves and the goal's three-adapter/live
   acceptance wording; the free vendor probe is now positive evidence, while
   actual VIA follow-up `via-4sw.3.4` remains required. Do not narrow the
   three-adapter scope.
