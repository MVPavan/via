# Examples

Use these as quality calibration only.

## Tech Tooling

Weak:
"Temporal is good for workflows, Celery is good for tasks, and Redis queues are simple."

Strong:
"Temporal wins when workflow correctness depends on durable timers, signals, retries, and replay across worker crashes. That strength is also its cost: it adds a workflow service, deterministic workflow constraints, and a new operational model. For a single-user assistant with human approvals, Postgres-backed jobs may be enough until runs span days or require compensation semantics. The upgrade trigger is not 'we have workflows'; it is 'we cannot reliably resume or audit long-running workflows with our current job model.'"

## Investing

Weak:
"AI demand is increasing, so semiconductor companies may benefit."

Strong:
"The investable question is not whether AI demand grows; it is where incremental dollars land and when that shows up in fundamentals. GPU vendors capture direct capex first, but power, networking, memory, cooling, and data-center REITs may see delayed second-order effects. The thesis needs segment exposure, margin sensitivity, backlog quality, customer concentration, and whether the growth is already priced in."

## Product

Weak:
"Users want collaboration features, and competitors have them."

Strong:
"The collaboration request is actually two jobs: shared review before publishing and async accountability after publishing. Competitors bundle both into generic comments, which creates noise. A better product decision is to separate approval workflow from discussion: approvals need state, owners, and audit trail; discussion needs context and low friction."

## Developer Tools

Weak:
"Use the SDK because it has tools and handoffs."

Strong:
"The SDK should own the loop only if its abstractions match your failure model. If you need provider portability through LiteLLM, custom approval gates, and first-class artifact logging, keep the orchestration boundary yours and adapt SDK utilities selectively. The SDK is a component, not the architecture."

## Policy

Weak:
"The law requires compliance, and penalties may apply."

Strong:
"Separate three layers: written rule, enforcement pattern, and practical compliance norm. If enforcement actions target deceptive marketing rather than technical paperwork defects, the highest-risk control may be claims review, not form automation. That distinction changes the product requirement."

## Quick Tests

- Replacement test: if the paragraph could apply to any industry, make it specific.
- Decision test: if it does not help a decision, cut it.
- Source test: if challenged, can the report show where the claim came from?
- Scope test: if it introduces a new research direction, did the user approve it?
