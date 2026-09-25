# Frontend performance

Load condition: the slow path renders in a browser. The SKILL.md method is
unchanged — baseline, profile, one change per measurement, keep-or-revert,
guard. This file supplies the browser-specific targets, measurements, and
fix patterns.

## Targets — Core Web Vitals

| Metric | Good | Needs improvement | Poor |
| --- | --- | --- | --- |
| **LCP** (Largest Contentful Paint) | ≤ 2.5s | ≤ 4.0s | > 4.0s |
| **INP** (Interaction to Next Paint) | ≤ 200ms | ≤ 500ms | > 500ms |
| **CLS** (Cumulative Layout Shift) | ≤ 0.1 | ≤ 0.25 | > 0.25 |

## Measurement

Two complementary sources — the baseline needs the first, a claimed
user-facing win needs the second:

- **Synthetic** (Lighthouse, DevTools Performance trace): controlled and
  reproducible — this is the Step 1 baseline command and the CI guard.
  `npx lighthouse <url> --output json --output-path report.json`
- **RUM** (`web-vitals` library, CrUX): real users on real devices — the
  only proof a fix improved what users feel.

INP workflow: field data first (CrUX/RUM) → record a DevTools Performance
trace while interacting; look for long tasks (>50ms) behind clicks and
keystrokes → re-test under 4–6× CPU throttling: INP problems often surface
only on mid-range hardware. Interaction-level attribution:

```js
import { onINP } from 'web-vitals/attribution';
onINP(({ value, attribution }) => {
  const { interactionTarget, inputDelay, processingDuration, presentationDelay } = attribution;
  // send to your RUM sink
});
```

Bundle analysis: `npx vite-bundle-visualizer`, or
`npx webpack-bundle-analyzer stats.json`.

## Symptom → first measurement

```text
What is slow in the browser?
├─ First page load slow
│   ├─ Large bundle       → bundle analysis; check splitting
│   ├─ TTFB > 800ms       → network waterfall: DNS long → dns-prefetch/preconnect;
│   │                       TCP/TLS long → HTTP/2, keep-alive, edge; server
│   │                       (Waiting) long → backend path: SKILL.md Step 2
│   └─ Render-blocking    → waterfall: CSS/JS blocking first paint
├─ Interaction sluggish
│   ├─ Freezes on click   → main-thread long tasks in the trace
│   ├─ Input lag          → re-renders, controlled-component overhead
│   └─ Animation jank     → layout thrashing, forced reflows
└─ After navigation
    ├─ Data loading       → API timing; serial request waterfalls
    └─ Client rendering   → component render profile; N+1 fetches
```

| Symptom | Likely cause | Investigate |
| --- | --- | --- |
| Slow LCP | large images, render-blocking resources, slow server | waterfall, image sizes |
| High CLS | dimensionless images, late-loading content, font swap | layout-shift attribution |
| Poor INP | long main-thread tasks, large DOM updates | long tasks in the trace |
| Slow initial load | bundle size, request count | bundle analysis, splitting |

## Fix patterns

### Images

- Modern formats (AVIF/WebP); responsive `srcset` + `sizes`; explicit
  `width`/`height` on every `<img>` and `<source>` (prevents CLS).
- Hero/LCP image: `fetchpriority="high"`, never lazy-loaded. Art direction
  via `<picture>` with per-breakpoint `<source>` crops when composition
  changes across screens.
- Below the fold: `loading="lazy" decoding="async"`.

### Main thread / INP

- Break tasks over 50ms; yield inside long loops — `scheduler.yield()`
  preferred, `scheduler.postTask()` with priorities for scheduling work,
  `isInputPending()` to yield only when needed.
- Defer non-critical work out of event handlers (analytics, logging);
  `requestIdleCallback` for deferrable work (prefetch, warmup).
- Heavy computation → Web Worker. Third-party scripts: `async`/`defer`,
  audited for size, heavy widgets (chat, embeds) behind a facade.

### React re-renders

- Stable references for props: hoist constant objects/arrays out of render.
- `React.memo` / `useMemo` / `useCallback` only where a profile shows
  benefit — sprinkled everywhere is as bad as nowhere.
- Long lists → virtualization (e.g. `react-window`).

### Bundles

- Route-level and heavy-feature code splitting:
  `lazy(() => import('./pages/Settings'))` behind `Suspense`.
- Tree shaking requires ESM dependencies marked `sideEffects: false` —
  profile before changing import styles; splitting and lazy loading are
  where the real gains are.

### CSS and fonts

- Critical CSS inlined or preloaded; non-critical CSS never render-blocking;
  no CSS-in-JS runtime cost in production (use extraction).
- Fonts: consider the system font stack first. Otherwise: 2–3 families and
  weights max, WOFF2 only, self-hosted; preload LCP-critical fonts;
  `font-display: swap` (or `optional`); subset via `unicode-range`; a
  variable font when multiple weights are needed (one file replaces many);
  adjust fallback metrics (`size-adjust`, `ascent-override`,
  `descent-override`) to cut font-swap CLS.

### Network and rendering

- Static assets: long `max-age` + content-hashed filenames + `immutable`.
  API responses: `Cache-Control` where staleness is acceptable. HTTP/2 or
  HTTP/3; `preconnect` known origins; `fetchpriority` on critical non-image
  resources too (key preloads, above-the-fold scripts); no redirect chains.
- Animations on `transform`/`opacity` only (GPU-composited); batch DOM
  reads, then writes — never interleave (layout thrashing);
  `content-visibility: auto` + `contain-intrinsic-size` for off-screen
  sections; keep bfcache eligibility (no `unload` handlers, no
  `Cache-Control: no-store` on HTML).

## Frontend budgets

```text
JS bundle (initial): < 200KB gzipped     CSS: < 50KB gzipped
Above-fold image:    < 200KB each        Fonts: < 100KB total
TTI on 4G:           < 3.5s              Lighthouse performance: ≥ 90
```

CI guards: `npx bundlesize` for bundle budgets, `npx lhci autorun` for
Lighthouse/CWV budgets.

## Frontend red flags

- Images without dimensions, lazy loading, or responsive sizes.
- Bundle growth landing without review.
- `React.memo`/`useMemo` added without a profile showing re-render cost.
- A CWV "win" verified only synthetically — RUM never confirmed it.
