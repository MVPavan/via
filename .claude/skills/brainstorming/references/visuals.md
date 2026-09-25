# Visuals

Diagrams during a brainstorm. Default to Mermaid in the conversation; escalate
only when asked.

## Default: Mermaid, inline

When a question is genuinely about structure — what the pieces are and how they
relate, a flow, a sequence, a state machine — draw it as a Mermaid block in the
reply instead of describing it in prose. A drawing the user can point at gets a
correction faster than a paragraph they have to hold in their head.

Keep it small. A diagram with more than a dozen or so nodes is answering more
than one question; split it.

Prose is still right for requirements and scope questions, choices described in
words, and trade-off lists. A question *about* a visual topic is not
automatically a visual question: "what does personality mean here?" is
conceptual; "which of these two layouts?" is not.

## On request: a rendered HTML file

If the user asks for something Mermaid cannot carry — a real mockup, a layout
comparison, spacing and hierarchy — write **one self-contained HTML file** to
`scratchpad/brainstorm/<topic>-<n>.html` and give them the path.

No server, no build step, no external stylesheets, fonts, or CDN scripts. One
file they can open. Offer this only when a diagram genuinely will not do; do not
offer it upfront.

`scratchpad/` is gitignored. Everything in it is disposable working material.

## What survives into the document

A diagram the user approved, that explains the design, goes into the
spec as a Mermaid block under **Implementation Decisions** — not as a link to a
scratchpad file, which will not exist for the next reader.

If the approved artefact is an HTML mockup that cannot be reduced to Mermaid,
write into the document what it showed and what was decided from it. The
decision is what downstream needs; the mockup was only how you reached it.
