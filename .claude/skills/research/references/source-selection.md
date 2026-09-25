# Source Selection

Use source type based on the claim being made. Do not treat all sources equally.

## Source Weights

| Research Area | Highest-Weight Sources | Useful Supporting Sources | Weak Alone |
|---|---|---|---|
| SDKs, APIs, developer tools | Official docs, changelogs, source repos, release notes, issue trackers | Benchmarks, migration guides, practitioner posts | SEO tutorials, stale blog posts |
| AI/model tooling | Official model/provider docs, pricing pages, eval papers, SDK docs | Community reports, benchmark discussions, incident reports | Vendor marketing without docs |
| Investing/fundamentals | SEC filings, earnings transcripts, investor presentations, segment data, primary macro data | Sell-side summaries, expert interviews, industry data | Social posts, unsourced price targets |
| Product/market | Product docs, pricing pages, customer reviews, job postings, public case studies | Analyst reports, competitor blogs, app reviews | Generic market-size articles |
| Policy/regulation | Statutes, rules, regulator guidance, enforcement actions, court filings | Legal commentary, trade association notes, public comments | News summaries without primary links |
| Architecture decisions | Official docs, implementation source, design docs, incident writeups | Benchmarks, migration stories, practitioner reports | Framework hype posts |

## Evidence Rules

- Current-state claims need current sources.
- High-stakes claims need primary sources or explicit uncertainty.
- Community evidence is valuable for "what happens in practice" but weak for legal, financial, or technical guarantees.
- If sources conflict, explain why: definition mismatch, date mismatch, incentive mismatch, geography, customer segment, or implementation context.
- Cite sources inline in the research document, bound to the claims they support. Include retrieval dates for volatile pages.

## Red Flags

- No date, no author, no primary citation.
- Market numbers with no methodology.
- Benchmarks that omit workload, hardware, version, or test method.
- Vendor pages comparing themselves to competitors without reproducible evidence.
- Anecdotes presented as base rates.
- Old docs for fast-moving APIs.
