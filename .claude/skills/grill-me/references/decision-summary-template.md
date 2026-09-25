# Decision Summary Template

After the review is complete, produce a summary in this format. This becomes the canonical record of all decisions made during the design review.

---

## Format

```markdown
# Design Review: [System/Project Name]
**Date:** [date]
**Reviewer:** Claude (Grill-Me Design Review)
**Participant:** [user name if known]

## Context
[2-3 sentences: what is this system, what problem does it solve, what stage is it at]

## Decision Log

### [Domain 1: e.g., Data Model]

| # | Decision Point | Resolution | Rationale | Implications |
|---|---------------|------------|-----------|--------------|
| 1 | Event schema evolution | Versioned envelope with lazy migration | Forward-compatible, no downtime migration required | Reader code must handle unknown fields gracefully |
| 2 | Entity resolution strategy | Deterministic merge on composite key (source_id + entity_type) | Idempotent, debuggable, no probabilistic matching needed at current scale | Must define conflict resolution for duplicate composite keys |

### [Domain 2: e.g., Storage Layer]

| # | Decision Point | Resolution | Rationale | Implications |
|---|---------------|------------|-----------|--------------|
| 3 | Write authority | PostgreSQL | ACID guarantees for canonical writes, well-understood ops story | Graph DB (Neo4j) fed via async sync; must handle replication lag |

[...continue for all domains...]

## Open Items
[Anything explicitly deferred or flagged for future resolution]

| # | Item | Reason Deferred | Revisit Trigger |
|---|------|----------------|-----------------|
| 1 | Multi-region deployment | Not needed at current scale | When P99 latency exceeds 200ms from non-primary region |

## Dependencies Map
[Key decisions that constrain other decisions — captured during the review]

- Decision #1 (schema evolution) → constrains Decision #7 (query layer must be version-aware)
- Decision #3 (PostgreSQL as write authority) → constrains Decision #5 (sync pipeline must be ordered and idempotent)

## Risk Register
[Risks surfaced during the review that aren't resolved by any single decision]

| Risk | Severity | Mitigation |
|------|----------|------------|
| Graph DB sync falls behind under write spikes | Medium | Circuit breaker on sync pipeline; fallback to direct PG queries |
```

## Guidance

- Keep each resolution to one sentence. The rationale column is for *why*, not *how*.
- The implications column captures ripple effects — what other decisions or code this affects.
- Open items are not failures. Deliberately deferring is fine; accidentally forgetting is not. This section makes the distinction explicit.
- The dependencies map is the most valuable part for the user's future self. It answers "if I change decision X, what else breaks?"
