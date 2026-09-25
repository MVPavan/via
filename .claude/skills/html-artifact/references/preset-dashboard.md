# Preset: dashboard

Chart-first documents: scaling analyses, capacity views, metric explorations, A/B comparisons. Prose is captions, not body.

## When dashboard vs report

More than a paragraph between charts → use `report` with embedded charts. Dashboard is for cases where charts are the argument and prose only labels them.

## Structure

A dashboard front-loads the KPI tile row, then presents one large primary chart, then a grid of smaller secondary charts. Underneath, a collapsed details block exposes the raw data table for anyone who wants to verify a chart's claim.

## Charts: SVG inline first

For bars, lines, scatter, and most distributions, hand-rolled SVG is smaller, instant-loading, dependency-free, and styled by the page's design language. Use the accent color for the primary series and neutrals for secondary. Gridlines and axis lines use the border color. Labels use the muted text color. Read `svg-craft.md` before drawing any chart — it owns the accessibility and honest-data rules.

Charts have a small caption above naming the metric and time window. Below the chart, a 1-2 sentence note pointing out what the reader should notice — the outlier, the trend, the surprise. Do not restate axis labels in prose.

## Use a library only when

- Hover tooltips with detailed breakdowns are required
- Chart type is genuinely complex (heatmap, treemap, sankey, force graph)
- User already uses a specific library in their pipeline — match it

When a library is genuinely needed, inline it into the file (vendored source in a `<script>` block) — never a CDN link; the artifact must open offline from `file://`. Prefer hand-built inline SVG. Inline a no-script paragraph stating the headline number so the doc isn't useless without JS.

## Color discipline

- One accent color for the primary series
- Neutrals for secondary
- Severity colors only for true pass/fail metrics — never as ordinary series colors
- No rainbow palettes; if you need 5+ series, split into multiple charts

## Drill-down table

Raw numbers behind every chart, in a collapsed details block by default.

## Acceptable deviations

- Date-range selector or series toggle if the user asks for exploration. Pure JS state, no backend.
- "Last updated" stamp with a faint pulse for live-status dashboards. No auto-refresh.
- For A vs B comparisons, share y-axis ranges explicitly — auto-scaled axes mislead.
