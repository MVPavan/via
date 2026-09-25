# Preset: explainer

"How X works", concept walkthroughs, onboarding docs, research summaries. The reader is new and chooses whether to invest each successive screen.

## Structure

Always lead with a TL;DR box — three bullets: what the thing is, why it matters, the one gotcha. A reader who only reads the TL;DR still walks away with the headline.

Then three to six narrative sections, each headed by a question the reader might ask: "How does the bucket refill?" beats "Bucket refill mechanics" — readers match their own question to the section. State the core insight — "the trick" — as one early sentence with the load-bearing word emphasized; never bury the punchline.

Always include a glossary, even if the audience is technical (spec below). Ground the abstract with a short "where you'll meet it" — real systems that use the thing. For explainers about this repo's code, end with a short FAQ and "where to look next" links into the codebase.

## TL;DR box

A tinted callout with the accent color as a left border. Three short bullets, each prefixed with **What**, **Why it matters**, **Gotcha**. Reads as a 10-second summary that stands alone.

## Collapsible deep-dives

For asides off the main path — interesting but not essential. Use native details/summary so the content is still there for JS-disabled readers. Style the marker subtly so it doesn't shout.

## Inline SVG diagrams

For any process, lifecycle, or relationship — draw it (rules in `svg-craft.md`).

## Tabbed code samples

When the same idea has multiple representations (curl / Python / TypeScript; config / code; production / test), use small in-page tabs. The active tab is underlined with the accent color. Tab content is monospace.

## Glossary

A two-column dl: term on the left in bold, definition on the right in muted color. 4-6 terms minimum. Hover-link relevant body terms to their glossary entries via anchor IDs. Place it in the margin at wide viewports — bottom glossaries are never read — falling back to the bottom under 720px.

## Comparison table

When teaching a choice ("X vs Y"), compare with numbers, not adjectives — "moves 1/N keys instead of (N-1)/N" beats "better". End with a "When to pick" row. That's the row the reader actually came for.

## Acceptable deviations

- Small interactive diagram (e.g. animated token bucket) when the concept is dynamic and a frozen frame doesn't carry it. Keep under ~30 lines of JS — bigger is an `editor` — and the static first frame must carry the point on its own.
- Drop the glossary for known-audience docs (ask first).
- Floating mini-TOC at wide viewports for explainers above ~5000 words.
