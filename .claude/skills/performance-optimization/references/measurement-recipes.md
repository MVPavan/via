# Measurement recipes — Python-first

Load condition: about to take a baseline, or run a profiler, EXPLAIN, or
benchmark — and you need the invocation or the pitfalls that corrupt
baselines. The method stays in SKILL.md; this file is tool mechanics.

## Timing harness

- `time.perf_counter()`, never `time.time()` — only the former is monotonic
  with sub-ms resolution.
- Report central tendency and spread with enough runs for the decision.
  Separate warm and cold conditions; retain cold start when it is the metric.
- Both variants coexisting in one process (two functions, a flag): interleave
  runs (ABAB…) so thermal/turbo drift hits both. The variant is a
  working-tree change: re-run the baseline immediately before the result run,
  so drift is bounded by minutes rather than the whole edit session.
- Throwaway timing scripts live under `scratchpad/` and never commit.

## CPU profiling

- **Live process**: `py-spy dump --pid <PID>` — one stack answers "stuck
  where?"; a blocked asyncio loop shows as one frame that never moves.
  Flamegraph: `py-spy --idle record -o profile.svg --pid <PID> --duration 30`
  — `--idle` sits before the subcommand (documented form) and matters when
  I/O-bound; without it, waiting time is invisible and the profile lies.
- **Script or test**: `py-spy record --subprocesses -o profile.svg -- uv run
  pytest tests/test_x.py` (`--subprocesses` because `uv run` wraps the
  interpreter), or `uv run python -m cProfile -o out.prof script.py`
  (view: `uvx snakeviz out.prof`).
- cProfile's overhead distorts small hot functions — use it to *rank* call
  sites, then time the candidate with the harness above; never quote
  profiler columns as the baseline number.
- Read cumulative time to find the guilty subtree, tottime for the hot leaf.

## System and runtime view

For "every request is slow" and intermittent spikes — measure host and
runtime before profiling code:

- CPU vs I/O-wait vs memory pressure: `vmstat 1` (us/sy vs wa columns;
  si/so ≠ 0 means swapping), or `pidstat -u -d -p <PID> 1` for one process.
- Saturation lives in queues: connection-pool checkout waits (SQLAlchemy
  `pool.status()`, pool-timeout log lines), task-queue depth, server
  backlog. A saturated pool shows as caller latency with an idle database.
- GC pauses: time collections (`gc.set_debug(gc.DEBUG_STATS)` in a dev run)
  and correlate spike timestamps with them; a large static object graph →
  `gc.freeze()` after warmup.
- Lock or event-loop contention: repeated `py-spy dump` snapshots — the
  frame present in every snapshot is the contended one; asyncio dev runs log
  slow callbacks under `PYTHONASYNCIODEBUG=1`.
- Intermittent spikes correlate or they don't: line spike timestamps up
  against GC, deploys, cron, co-tenant load — before profiling code.

## Memory

- Distinguish first: steady-high after warmup is **footprint** (working
  set); growth without release across repeated cycles is a **leak** (leaked
  refs, unbounded cache, growing buffer). Different fixes.
- `uv run memray run -o out.bin script.py` (or `-m module` — memray takes
  the script directly, no `python` prefix), then
  `uv run memray flamegraph out.bin` — allocation attribution by call site.
- `tracemalloc`: snapshot before and after N repeated cycles,
  `compare_to(before, 'lineno')` — the diff names the growing line.

## Database (PostgreSQL-first)

- `EXPLAIN (ANALYZE, BUFFERS)` the exact query with the slow path's bind
  values, on production-shaped data volume. Run it twice — the first
  execution measures cold cache; report which run you kept.
- Read **actual vs estimated rows** first: an estimate off by 100×+ means
  stale statistics — run `ANALYZE <table>` before touching indexes; the
  plan may fix itself.
- Seq scan on a large table with a selective filter → index candidate.
  After adding, re-EXPLAIN to confirm the planner uses it — it often
  doesn't (low selectivity, type casts, functions on the column).
- N+1 shows in query **count**, not query duration: count queries per
  request (SQLAlchemy `echo=True` in dev, or the server's statement log)
  before timing anything.

## Micro-benchmarks

- `pytest-benchmark` (`uv run pytest --benchmark-only …`) for a named hot
  function — only after a profile named it; a micro-benchmark cannot tell
  you whether the function matters.
- Regression gating in CI: saved baselines live under `.benchmarks/` in a
  per-machine directory — persist it between runs (commit it, or cache it
  keyed to a fixed runner image) or every compare starts blind. Then
  `--benchmark-autosave` on the accepted baseline,
  `--benchmark-compare --benchmark-compare-fail=median:10%` on candidates
  (bare compare reads the latest saved run).
- Benchmark the seam callers use, with realistic inputs — tiny synthetic
  inputs optimize the wrong regime.

## External calls

- Time each dependency at its call boundary — one structlog field per call
  site — and compare against its timeout budget.
- Count calls per operation before profiling inside the client: the usual
  fix is fewer or concurrent calls (bounded gather), not faster ones.
