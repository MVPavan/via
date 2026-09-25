# SVG craft — figure rules for any inline diagram or chart

Read before drawing any inline SVG figure, in any preset.

## Mechanics

- `viewBox`, never fixed `width`/`height`, so the figure scales with its column.
- `currentColor` for strokes and for `<text>` fill so ink follows the page in both themes; page tokens for shape fills and accent emphasis.
- Round coordinates (`x="120"`, not `x="119.7843"`) — a human may edit this file.
- Group with `<g>` and name each group; real `<text>`, never text rendered as paths.
- Paint order: background → zones → connectors → labels → nodes. Draw connectors before boxes so lines pass behind nodes.

## Connectors and layout

- Orthogonal (elbow) connectors with a small corner radius — never diagonal lines between boxes.
- Every arrow label sits on an opaque mask with a 6–10px gap; never let a label mask overlap a node painted after it — the node covers the label.
- No overlapping connector strokes — offset them, or bridge one over the other with a small hop arc.
- Fan multiple connectors across a box edge (≥12px apart); a connector never transits behind a box that isn't its endpoint.
- Dash is semantics (async, optional, a callout leader), never decoration. Annotation callouts live in margins, at most 2 per figure, and never label something the figure should label directly.
- Legend as a bottom strip covering every treatment used and nothing extra — never floating inside the figure.
- Axis and level labels stay horizontal and minimal — no `writing-mode` vertical text, no emoji as data markers (they don't localize, print, or scale).

## Complexity budget

- ≤9 nodes, ≤12 arrows, ≤2 accented elements per figure. Accent marks what is focal — if you want to accent four things, you haven't decided what's focal.
- Over budget → split into an overview figure plus a detail figure. Splitting beats shrinking; never squeeze.
- A single relationship a sentence can carry needs no figure — a figure must beat the paragraph it replaces.

## Accessibility

- `role="img"` on the `<svg>` with `aria-labelledby="fig1-title fig1-desc"`; `<title>` as its first child describing what the figure shows (not its geometry), plus a `<desc>`; decorative SVGs get `aria-hidden="true"`.
- Prefix IDs per figure (`fig1-title`, `fig1-arrow`): duplicate `<title>`/`<desc>` IDs make screen readers announce the wrong figure, and a duplicate marker or gradient ID breaks rendering — `url(#id)` resolves to the first match in the page.
- Shape and color both carry meaning, so the figure survives grayscale and colorblind reading.
- Happy path highlighted in the accent; failure and retry paths muted. Direction indicators on every edge — without arrowheads a flowchart is just a graph.
- One figure per `<figure>` with a `<figcaption>`; keep line weight, arrowheads, and type consistent across a set. Add a per-figure "Copy SVG" button when the user will reuse the art (absent with JS off — the markup is still selectable).
- Detail that won't fit goes in `<details>` blocks below the figure anchored to node labels — never a JS-only side panel.

## Honest data

The encoding is the claim. A figure that renders beautifully and states a falsehood is worse than no figure.

- Never clip, floor, log-scale, or drop a value to make it visible — an omitted part makes a "whole" a lie.
- Never move a mark to fix a label collision. Crowded labels are data; moving the mark converts a legibility problem into a false statement, invisibly. Fix the label, split the figure, or disclose.
- Never truncate a value axis to its observed extremes; state the domain you drew.
- Parts must reconcile with the stated whole, or the rounding is disclosed next to the figure.
- Unequal intervals are drawn unequal; proportional areas are drawn proportional — break an axis visibly rather than faking even spacing.
- Never impute a missing value and never silently drop the row — disclose it.
- A connector between two measured values is a gap, not a trajectory — don't narrate it as movement or read intermediate values off it.
- No forced trend lines on scattered data; no bubble-area encodings (area perception is unreliable).
- When a rule above must be broken, name the exemption, state the reason, and bound its scope — never silently relax.
