# routes-decision.md review r1: GPT-6 Astra high

Run: `codex exec -s read-only -m gpt-6-astra -c model_reasoning_effort=high`, 2026-09-25 (codex-cli 0.156.1, 67,594 tokens). Brief: shared with the Fable review. Verbatim output below.

**Verdict: ACCEPT WITH CHANGES.** Direct CLI/server routes and native ACP for breadth are defensible defaults, but the evidence does not establish several categorical SDK disadvantages or justify shared-server deployment without additional isolation and supervision gates.

Findings, ranked:

1. **[major] The SDK rejection confuses caller lifetime with supervisor lifetime.**  
   `docs/brainstorms/routes-decision.md:116` marks SDKs as failing R1 because runs depend on their library host; CLI and server processes also need a surviving owner. A VIA-supervised SDK host could outlive the caller—an architectural inference, not a measured result. The single-binary row also accepts Node-dependent bridges while rejecting SDK hosts (`:120`), despite sidecars being explicitly unresolved in `.repo-context/invariants.md:38–44`.

   **Strongest counterargument:** a supervised official SDK can transfer private-protocol maintenance to the vendor, potentially outweighing its runtime cost. The Claude SDK allows an executable override (`scratchpad/acpx-eval/prefix/node_modules/@anthropic-ai/claude-agent-sdk/sdk.d.ts:1822–1824`), and Cursor already supplies otherwise-missing capabilities (`docs/brainstorms/access-methods.md:517–519`). Thus “adding no capability” at `routes-decision.md:153` contradicts the document’s own Cursor exception.

   **Fix:** reject SDKs as a packaging/directness tradeoff, rather than an inherent lifecycle impossibility. Define whether R4 categorically forbids sidecars and acknowledge the capability and maintenance costs accepted by that choice.

2. **[major] Shared servers need a topology decision, not merely a lifetime choice.**  
   `routes-decision.md:157–160` acknowledges the invariant conflict but leaves ownership, transport and recovery unspecified. Separate workers cannot independently own the same stdio connection; an enduring owner must route commands/events or use a verified multi-client transport. The prior analysis explicitly separates route from lifetime and offers one app-server per activation (`access-methods.md:160–164`).

   **Fix:** include dedicated servers per run and bounded pools as alternatives. Before enabling sharing, specify server ownership, takeover fencing, authenticated caller handles, configuration-compatible pooling, and backpressure isolation. Clarify how starter-only cancellation survives caller death. Treat automatic resubmission as unsafe until completed side effects can be reconciled; “fail or resubmit” is not a neutral implementation detail.

3. **[major] The decision omits adverse isolation evidence.**  
   `scratchpad/headless-bench/analysis.md:79–86` reports off-target replies in **3 of 16 non-target OpenCode sessions** during abort conditions, explicitly without establishing causation. It also says kill tests did not establish cleanup during executing tools. `routes-decision.md:186–187` requests isolation checks only for Codex.

   **Fix:** carry both caveats into the decision and gate shared OpenCode on paired fault/control probes, cancellation during active tools, and descendant cleanup. The evidence establishes density benefits; it does not yet establish reliable shared-session isolation.

4. **[major] ACP’s measured overhead is over-attributed to bridging.**  
   `routes-decision.md:80–83,148–150` generalizes acpx measurements into “bridged ACP doubles cost.” But `scratchpad/headless-bench/acpx-analysis.md:34–35` attributes overhead to **both the acpx Node client and bridge**. VIA would replace the client. One process tree per session is acpx’s policy (`scratchpad/acpx-eval/capabilities.md:14–18`), not an ACP requirement.

   **Fix:** label these as measurements of the tested acpx topology. Distinguish direct VIA→bridge and shared native ACP as unmeasured alternatives. The permission and failure evidence still supports avoiding these bridge versions, independently of an unproven universal 2× penalty.

5. **[major] The permissions comparison uses different standards for different routes.**  
   R3 requires agent-native enforcement, but the matrix rejects native ACP because permission callbacks are optional (`routes-decision.md:118`). The spec’s “MAY” establishes optional client consultation, not absence of agent-native enforcement (`scratchpad/acp-docs/pages/protocol__v1__tool-calls.md:132`). The existing acceptance criteria already permit launch-time permission configuration (`access-methods.md:543–545`).

   There is also an unresolved scope conflict between “VIA adds no permission layer” and retaining OpenCode’s external-sandbox requirement (`routes-decision.md:23–24,167–168`).

   **Fix:** evaluate enforcement per adapter/configuration, distinguish approvals from sandbox bounds, and define how unanswered headless callbacks terminate. Explicitly decide whether externally supplied isolation is supported or OpenCode remains unavailable for bounded runs. “As OpenCode does today” at `:146` should name the existing parent-project adapter; VIA itself is not implemented.

6. **[minor] The CPU claim drops a material exception.**  
   The memory totals and approximately **4.8×/23×** reductions check out against `scratchpad/headless-bench/runs/full/summary.md:17,24,31,38`. However, `routes-decision.md:76–79` says vendor servers flatten startup bursts generally; Codex server peaked at **25.2 cores**, versus **21.9** for its CLI at 32 sessions.

   **Fix:** qualify the CPU benefit by vendor/concurrency. Label memory as baseline-adjusted, one-second-sampled anonymous peaks. State that ACP’s 32-session conditions had one repetition (`scratchpad/headless-bench/acpx-analysis.md:83–84`) and distinguish marginal session costs from total-per-session averages.

7. **[minor] The route table overstates uniformity.**  
   “One process, many sessions” (`routes-decision.md:48`) does not apply to Pi’s simultaneous runs (`access-methods.md:60–64`). Universal line framing (`routes-decision.md:38`) does not cover OpenCode’s HTTP surface (`access-methods.md:57`). The universal SDK wrapper description ignores reported in-process Pi/OMP implementations (`access-methods.md:44–47`).

   **Fix:** describe process topology and framing per transport/vendor. Replace the testing checkmarks with one common requirement: recorded contracts **plus live, pinned-version integration probes**. SDK mocks and recorded RPC fixtures both miss live drift.

8. **[minor] Decision scope and acceptance costs remain unclear.**  
   The header says only layer 1 changes, while server-first routing appears to supersede layer 0’s CLI defaults (`routes-decision.md:3–5,127`; `access-methods.md:445–446`). “Workflow crew concurrency,” “inspector,” and fallback conditions are undefined.

   **Fix:** state the release scope, target concurrency, and whether fallback occurs only before spawn; switching an existing run conflicts with `.repo-context/invariants.md:16–17`. Add native Linux/macOS validation before enabling adapters; WSL-only measurements do not cover the first-priority platforms (`invariants.md:33`). Record route-specific terms/billing status and shared HTTP-server exposure, already identified in `access-methods.md:349–351,565–589`.

**Unverified claims**

- `routes-decision.md:96–97`: the supplied capability report contains no system-prompt delivery probe supporting “Claude only; Codex/OpenCode silently ignored.”
- `:105–109`: bundled execution is verified, but the cited demo does not establish the 215 MB size, default minimal prompt, or host-death behavior. It explicitly selects the preset and closes the query (`scratchpad/acpx-eval/sdk-demo/demo.mjs:13,22`).
- `:56`: the supplied SDK request union contains **35** members (`sdk.d.ts:4280`); “~40” is loose, and definitions alone do not prove every request works directly.
- `:179–180`: the paused June credit-plan claim lacks supporting evidence in the reviewed material.
- The ACP “MAY,” draft per-turn usage, and v2 removal claims are supported by the supplied spec snapshots; I did not independently refresh their current status.

**What I would add or cut**

Add a compact per-adapter acceptance table covering ownership, isolation, permissions, supported versions/platforms, and terms status. Preserve a sanitized, committed benchmark summary so public readers can audit the rationale. Cut categorical route scorecards and replace “SDKs add no capability” with the explicit tradeoff being chosen.

Read-only review; no files edited or live agent probes run.