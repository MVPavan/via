# Optional visual style

Use when no existing project style answers a design choice. Components below are
options, not slots to fill or mandatory quotas.

## Aesthetic direction

**Tokens first.** If the project defines its own visual language — CSS custom properties, a theme or tokens file, an installed design skill — read it and use those values as the artifact's CSS variables. If it defines none, apply the direction below. Never invent tokens that merely resemble the project's: match the source or use the default. The invariants above always win — drop any token that would require a webfont request or pure black.

**Color:** Neutral page background — off-white in light mode, near-black in dark mode. One accent color used sparingly for emphasis (a single primary KPI, a copy button, the active tab in a tabset) — more content never buys more accented elements; if a palette offers eight colors, pick three and demote the rest. Reserve red/amber/green for severity tags only. Avoid loud color as a background; the page should look like a document, not an ad. Compute contrast against the token you actually ship: ≥4.5:1 for body and muted text, ≥3:1 (WCAG 1.4.11) for any graphical mark that carries meaning — if the accent can't clear it, carry the meaning on the boundary or weight and make hue redundant.

**Typography:** System or named-local font stack (see invariants). Body 16–18px with generous line-height (1.5–1.7); small caption text, large section titles, and one or two hero numbers per artifact. Monospace is for technical content only — paths, commands, values — never a blanket "dev" look. Tabular numerals for any column of figures.

**Spacing:** Consistent rhythm based on a small set of values (multiples of 4px or 8px). Sections separated by larger gaps than paragraphs; KPI tiles separated by smaller gaps. The same artifact should never mix arbitrary px values — pick a scale and stick to it.

**Layout:** Cap prose measure at 60–75ch. Sections are vertically stacked; multi-column only where it earns its weight (KPI rows, chart grids, controls/preview splits). Always single column under 720px.

**Dark mode:** Same layout, inverted palette — same opacities with the neutral RGB flipped; the accent and severity colors shift to slightly lighter/desaturated variants. Use a dark palette with readable contrast. Never name a tone in prose or a legend ("darkest") — a tone description inverts between themes and ships false in one of them; say "strongest".

## Component vocabulary

Every artifact draws from this small set. Reuse suitable existing components while following the aesthetic direction above. Lay content out as its own shape — a timeline drawn as a timeline, a comparison as columns, a diff as a diff — never markdown structure translated 1:1 into HTML.

**KPI tile row.** 3–5 cards at the top. Each shows a big number, a small muted label below, and optionally a small delta. Borders, not shadows; vary card widths (e.g. `1.1fr 1fr 0.9fr`) rather than shipping an equal grid. The fastest way to orient a reader — use it whenever there are concrete facts the reader needs before reading anything else.

**Numbered section header.** A large grey number ("01", "02") set alongside the section title, with a thin horizontal rule below. Signals "this is scannable, you can jump by section".

**Code block with copy button.** Monospace pre/code with a small "Copy" button in the top-right that writes to the clipboard and briefly flashes "Copied". A muted caption above naming the file path.

**Severity tag.** A small rounded pill in high/med/low severity colors. Tinted background, full color foreground — never raw severity color as a large background. For risks, incidents, statuses.

**Collapsible.** Native `<details>/<summary>` so JS-disabled readers still get the content. For deep-dives, raw data behind charts, anything off the main path.

**Inline SVG diagram.** Real SVG for flows, architectures, lifecycles — never ASCII, unicode boxes, or embedded screenshots. Before drawing any figure, read `references/svg-craft.md`: craft rules, connector geometry, accessibility contract, and the honest-data rules.
