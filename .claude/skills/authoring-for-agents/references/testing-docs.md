# Evaluating agent instructions

Choose checks for the failure the edit addresses. Use existing evidence and
permitted runtime tools; a prose review cannot prove a model will obey a rule.

| Change | Useful check |
|---|---|
| Editorial correction | Reread and inspect the diff |
| Metadata, pointer or alias | Parse metadata, resolve links, inspect actual discovery |
| Trigger boundary | Fresh-context positive case and a neighboring negative case |
| Workflow rewrite | Representative task with acceptance criteria and a bounded budget |
| Consequential guardrail | Baseline plus a realistic pressure case |

Test discovery with only the normal entrypoint available; handing the body to an
agent bypasses the routing under test. Observe selected actions and resulting
artifacts rather than asking whether the wording is clear. Keep evaluation tasks
isolated from production side effects.

For comparisons, hold task, context, tools and effort constant. Use an unchanged
control when attributing improvement to wording. Repeat only enough to distinguish
variance from a material effect; report sample size and uncertainty.

A failed case calls for the smallest effective change, not three new rules.
Recheck the failure and plausible regressions. Record actual outcomes separately
from predicted behavior. If runtime access or budget prevents a trial, report the
limit and finish the authorized structural work.
