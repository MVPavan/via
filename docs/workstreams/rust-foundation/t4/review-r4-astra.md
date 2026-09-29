**UNSOUND for implementation.** The rewrite closes several round-3 findings, but allocation accounting, bounded failure summaries, and slice interfaces still need correction.

Reviewed clean `wt/t4-0` at `d75705d`, including S1 and S2 in full, all remaining briefs, the decision records, my Astra round-3 review, and relevant contract/dependency/source paths.

**1. Round-3 decisions**

APPLIED: **4, 5, 6, 7, 8, 9, 11, 12**. None wholly MISSING.

| Decision | Status | Remaining gap |
|---|---|---|
| 1 — No hidden reparse | PARTIAL | The RawValue-key exploit is closed, but the normative buffering prohibition is incomplete; see 3b. |
| 2 — Every allocation charged | PARTIAL | Borrowed-envelope scratch, expanded lists, and follower containers/transfers remain incorrect or unproved. Findings F1/F3. |
| 3 — Exclusive Latch bytes and transaction fit | PARTIAL | Exclusive reservation and arithmetic are correct; the fallback envelope is not necessarily bounded. F2. |
| 10 — Compilable boundaries | PARTIAL | Missing pass-through ownership and incompatible Store/Raw interfaces remain. F4. |

Decision 12’s self-check is documented in report §8.1; its completion does not establish its conclusions.

**2. My round-3 findings**

- **Finding 1:** the specific private-RawValue reparse is closed; the broader allocation proof remains open.
- **Finding 2:** partially closed—shared 512-byte minimum, public-page container charging, and borrowed-row checks are present; F1/F3 remain.
- **Finding 3:** closed as raised—Lifecycle cannot consume Latch bytes.
- **Findings 4–6:** closed at design level—terminal delivery preserves queued events, `list` has a fixed population, and closing takes precedence in counts.
- **Finding 7:** partially closed—separate `result_encoded` and S1 Public plumbing fix the original conflicts, but F4 remains.

**3. Amendments**

| Amendment | Ruling | Contract and simpler option |
|---|---|---|
| **A22: 704 KiB envelope** | **Accept the amendment; reject the current completeness proof.** | It resolves the conflict between `docs/specs/via-api-v1.md:466` and `docs/specs/runtime-contracts.md:1020`. F2 must be fixed. Blob-backed envelopes preserve the old limit but require more machinery; no simpler equally capable option is established. |
| **A23: `process.cleanup`** | **Accept.** | Consistent with `docs/specs/via-api-v1.md:274` and `docs/workstreams/rust-foundation/t3/design.md:469`. No simpler boolean-only representation distinguishes positive liveness from unproven absence. |
| **A12: `w0`, `l2.` cursor** | **Accept the amended semantics.** | Explicitly replaces `docs/specs/via-api-v1.md:304`. Fixed population and stopping before an unreturned match repair termination and skipping. Creation-order-only paging is simpler but sacrifices the requested initial ordering; a snapshot requires more machinery. |

**3b. Stronger decision 1**

**Accept avoiding peer `Value`; do not accept “every serde buffering path is closed.”**

`Box<RawValue>` preserves the literal span and avoids `Value`’s private-key reinterpretation. However, `docs/workstreams/rust-foundation/t4/design.md:1718` names only `flatten` and `untagged`. Internally tagged enums also allocate `Content`; adjacently tagged enums can buffer content received before their tag. The installed serde implementation confirms both. The report recognizes internally tagged enums, but the normative rule and S2 check omit them.

I found no such active peer enum in the inspected current Route/DTO definitions. This is an incomplete prevention rule, alongside the concrete allocation failures below.

**4. Remaining defects**

- **F1 — Blocker: the replacement memory proof is false.**  
  `docs/workstreams/rust-foundation/t4/design.md:292` assumes disjoint retained spans, but expanding `Box<RawValue>` into `Vec<String>` retains the original span **and** decoded strings. A long `require` element defeats `len(params) + 24 × elements + 1 KiB` after transient permits are released. Also, `docs/workstreams/rust-foundation/t4/design.md:1747` is not allocation-free: serde_json’s `deserialize_raw_value → ignore_value` uses scratch for nesting, and escaped field names use string scratch.  
  **Fix:** charge each pass and overlapping representation explicitly, or consume/drop raw fields with accounted ownership transfer. Prove capacity bounds from the pinned implementation. The former peer-node formula is now removed, not proved; Core/stored `Value` structures still require a structural allocation bound. Largest-input allocator samples alone do not supply that proof.

- **F2 — Blocker: the “bounded failure summary” can remain oversized.**  
  `docs/workstreams/rust-foundation/t4/design.md:1168` clears only `final_text` and `structured_output`. A valid sub-1-MiB terminal frame can contain a roughly 720-KiB `stop_reason`; `crates/via-core/src/engine/terminal.rs:342`. Clearing text still exceeds 704 KiB. A large `vendor_code` likewise threatens terminal extras.  
  **Fix:** construct a separately bounded overflow summary covering **all** vendor-derived members, retain full evidence in raw, and measure both summary and terminal extras before persistence. Test oversized stop reasons and vendor codes, not only final text.

- **F3 — Important: follower permit ownership contradicts itself.**  
  `docs/workstreams/rust-foundation/t4/design.md:1525` reserves and splits permits; `docs/workstreams/rust-foundation/t4/design.md:1600` acquires them again. `docs/workstreams/rust-foundation/t4/s6.md:59` transfers no permit. The rescan also omits a separate bound for its preallocated item vector when free bytes are small but free slots are numerous.  
  **Fix:** pass charged entries through the sink, transfer permits atomically during enqueue, and account for container capacity before the Store read.

- **F4 — Important: slice ownership does not support the required interfaces.**  
  `docs/workstreams/rust-foundation/t4/s2.md:51` owns Host/Adapter `live_armed`, but omits the necessary Route and Wire pass-throughs. `docs/workstreams/rust-foundation/t4/s3.md:48` forbids Store edits while requiring deletion of Store’s `RawWriter::append`. Finally, S1’s copying `Payload::stage(&[u8])` cannot implement §4.2’s zero-copy freeze of an already charged frame.  
  **Fix:** assign the complete liveness chain to S2; defer wrapper deletion to serial S5; define a consuming buffer-plus-permit freeze interface in S1.

- **F5 — Important: bulk replay comparison can block control admission.**  
  `docs/workstreams/rust-foundation/t4/design.md:696` holds global `admission` while reading up to 16 MiB through the shared raw worker. A stalled worker consequently blocks unrelated close admission; the blob-reader path specifies no overall deadline.  
  **Fix:** compare immutable identity bytes outside global admission, then reacquire and revalidate admission/key state. Add a stalled-replay test proving unrelated control remains serviceable.

**5. Slice readiness**

- **S3 ∥ S4 ownership:** disjoint on paper, **not operationally sufficient** because of F4.
- **Compilable boundaries:** **not established**; S2’s liveness chain and S1→S3 payload interface need explicit completion.
- **End-to-end endings:** all seven specify them; S7 ends with the integrated gate.
- **Self-sufficient briefs:** **not yet**—F1–F4 require decisions an implementer cannot resolve merely by following the briefs.

No files changed; no Cargo, Beads, tests, benchmarks, or runtime probes ran. Compilation, allocator/RSS bounds, timing, and exhaustive failure-path coverage remain unverified.