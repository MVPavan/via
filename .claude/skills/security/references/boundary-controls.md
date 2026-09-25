# Boundary controls by changed surface

Consult only the relevant control group. Match controls to the actual deployment
and check current official guidance when implementing unfamiliar mechanisms.

## Boundary controls (method, not catalogue)

- **AuthZ is not authN.** Every protected operation checks *ownership or
  role*, not just a valid login. "Authenticated" answers who; it never
  answers whether they may touch this resource.
- **SSRF** — any server-side fetch of a URL the user influenced (webhooks,
  import-from-URL, previews): allowlist scheme + host; resolve **all** DNS
  records and reject any private/reserved address (loopback, link-local
  `169.254.169.254` cloud metadata, RFC-1918, unique-local); forbid
  redirects. Know the residual TOCTOU gap: DNS can rebind between check and
  connect — for high-risk surfaces pin the resolved IP or front with a
  filtering proxy.
- **Uploads:** constrain type by magic bytes (never extension alone), cap
  size, store outside the web root under generated names.
- **Dependency audit triage** — an advisory is not a verdict; triage by
  severity × reachability × fix availability: critical/high + reachable in
  any runtime/build/test path → fix now; fix exists → take the patched
  version; no fix → workaround, replacement, or allowlist **with a review
  date**; moderate reachable → next release; dev-only/low → backlog. Every
  deferral is documented with its reason and date.
- **Supply chain:** first *find the installation boundary* — the workspace
  root that owns the lockfile, corroborated by the manager declaration and
  what CI actually runs; stop on disagreement or competing lockfiles. Then:
  block dependency install scripts before first execution and approve only
  the minimum, never blanket-approve; never auto-apply forced audit
  remediation; verify registry signatures/provenance where the manager
  supports it; review new dependencies, lockfile diffs, and script-policy
  changes together (ownership, provenance, release age, typosquats).
- **Secrets:** use the project's configured secret source; grep the staged diff
  for credential patterns before any commit; **a committed secret is
  compromised the moment it reaches a remote — rotate first, then purge;
  deleting the line is not remediation.**

## LLM / agent attack surface

Use these controls when the change affects model outputs, tool permissions or retrieval:

- **Model output is untrusted input (LLM05).** Never pass it *directly* into
  `eval`, SQL, a shell, a file path, or rendered markup. Parse defensively,
  validate against a schema, then apply the sink-specific control:
  parameterized queries for SQL, argv APIs (never shell interpolation) for
  commands, canonicalized allowlisted paths for files, sanitized/escaped
  markup for rendering.
- **Prompts can be hijacked (LLM01).** Any untrusted text in the context
  window — a fetched page, document, retrieved message or tool result — can carry
  instructions. The system prompt is **not** a security control: enforce
  permissions in code, and never let security depend on prompt
  confidentiality (LLM07).
- **Keep secrets and unauthorized tenant data out of prompts (LLM02/LLM07).** Anything in the window can be echoed back; a
  system prompt is ordinary context, not a vault. Permission instructions may
  describe policy; enforce that policy in code outside the model.
- **Constrain tool/agent permissions (LLM06).** Minimum tool scope;
  confirmation for destructive or irreversible actions; validate every tool
  argument like any untrusted input.
- **Bound consumption (LLM10).** Cap tokens, request rate, and
  loop/recursion depth so a crafted input cannot run up cost or hang the
  system.
- **Guard retrieval (LLM08).** In RAG, the vector store is a trust boundary:
  authorization-aware retrieval, tenant-scoped partitions or namespaces, and
  validation of documents before indexing so poisoned content cannot steer
  answers.
- **Model supply chain (LLM03/LLM04).** Models, adapters, datasets, and
  plugins get the same provenance and version review as code dependencies;
  authenticate and validate ingestion sources, and re-evaluate after data or
  model changes.
- **Don't act on unverified model claims (LLM09).** High-impact claims get
  checked against authoritative sources; consequential actions get human
  review.
