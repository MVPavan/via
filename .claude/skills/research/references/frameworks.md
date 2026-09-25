# Frameworks

Use frameworks to sharpen judgment, not to decorate a report. Pick 1-3.

## Decision Matrix

Use for tool, product, architecture, or vendor choices.

Minimum columns: option, best fit, sacrifices, integration cost, operating cost, lock-in, risk, confidence.

Rules:
- Weight criteria by the user's constraints.
- Explain why the winner wins.
- Include "do not choose if" for each serious option.

## Build vs Buy vs Compose

Use for architecture, tooling, platform, and workflow decisions.

Ask:
- What is core product behavior vs commodity plumbing?
- What must be customized?
- What failure modes become ours if we build?
- What lock-in or missing control do we accept if we buy?
- Can composition preserve control with less maintenance?

## Ecosystem Maturity

Use for SDKs, frameworks, databases, agent stacks, and infrastructure.

Check:
- Release cadence and breaking changes.
- Documentation quality.
- Issue velocity and maintainer responsiveness.
- Production references.
- Migration path and exit cost.
- Security posture and dependency footprint.

## Failure-Mode Analysis

Use for architecture, workflow engines, financial systems, compliance, and automation.

For each option list:
- What fails?
- How is it detected?
- Who is paged or notified?
- Can it retry safely?
- What state can corrupt?
- What human approval is required?

## JTBD

Use for product, feature, and market research.

Frame:
"When [situation], the user wants to [job], so they can [outcome]."

Then map functional, emotional, and social jobs. Include competing alternatives and "do nothing."

## Positioning Map

Use for competitive landscapes.

Pick two dimensions customers actually care about. Plot competitors, clusters, and empty spaces. Treat empty space as a hypothesis, not proof of opportunity.

## Segmentation

Use for markets, users, customers, and investing exposure.

Segment by behavior or economics, not demographics alone. For each segment identify size, urgency, willingness to pay, access path, competition, and reason to choose.

## Thesis Map

Use for investing or strategic bets.

Structure:
- Core thesis.
- Key drivers.
- Evidence.
- Catalysts.
- Risks.
- What would falsify the thesis.
- What is already priced in.

## Ripple-Effect Map

Use for trends and second-order effects.

Map:
1. Direct beneficiaries.
2. Suppliers and enablers.
3. Substitutes hurt by the trend.
4. Bottlenecks.
5. Regulation or policy reactions.
6. Time lag before impact appears in fundamentals.

## Regulatory Risk Matrix

Use for policy and compliance research.

Columns: rule, actor affected, jurisdiction, current requirement, enforcement evidence, likelihood, severity, mitigation, confidence.

Always separate written law from enforcement reality.

## Root Cause Analysis

Use for operational or product problems.

Start with the observed problem. Ask why until reaching a changeable cause. If multiple causes exist, group them by people, process, technology, policy, incentives, and external constraints.

## Source Confidence Ladder

Use in every final report — the four levels (High / Medium / Low / Speculative) are defined in `synthesis-engine.md` § Confidence Calibration.
