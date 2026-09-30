**UNSOUND**

1. **Important — Wire evidence writes escape task ownership.**  
   `crates/via-wire/src/connection.rs:198`, `crates/via-wire/src/runtime.rs:70`. **Verified code path; stalled-filesystem consequences inferred.** `keep_undecoded` drops its `spawn_blocking` handle after two seconds, although the write continues. Neither Wire’s straggler tracker nor Store’s blob tracker owns it. Evidence-directory creation has the same cancellation gap. Repeated timed-out writes can accumulate beyond the four connection holders; outstanding writes can be omitted from the shutdown summary. This violates R7 and coding-style §5. **Smallest fix:** admit these operations through a bounded blocking-task owner, retain ownership through timeout/cancellation, and include unfinished work in shutdown disposition.

2. **Important — Blob recovery allocates proportional to all historical turns.**  
   `crates/via-store/src/runtime/sql.rs:686`, `crates/via-store/src/runtime/sql.rs:710`. **Verified allocation path; exhaustion risk inferred.** `referenced_blobs` collects every stored blob reference into a `Vec`; sweeping repeats that collection and constructs a `HashSet`. Recovery runs both before serving requests. Many completed turns with small `prompt_file` inputs therefore produce unbounded startup memory growth. The recorded limitation permits linear verification I/O, not an unbounded resident collection outside §5.1’s accounting. **Smallest fix:** verify through a streaming query and sweep using bounded pages or database membership lookups.

3. **Important — Identical spawn retries depend on the original directory still existing.**  
   `crates/via-core/src/engine/receipt.rs:178`, `crates/via-core/src/engine/receipt.rs:217`. **Verified by reproduction.** Filesystem validation of `cwd` precedes the idempotency lookup. I completed a keyed spawn, removed its now-unused working directory, and repeated the identical parameters. VIA returned `-32602`, field `cwd`, instead of the original receipt. Runtime §6 promises historical receipt replay before current admission checks. **Smallest fix:** defer applying the filesystem-validation result until the key lookup establishes that this is new work; keep filesystem I/O outside the admission lock.

4. **Important — Invalid startup files can hang instead of being rejected.**  
   `crates/via-cli/src/server/config.rs:98`, `crates/via-cli/src/server/log.rs:63`. **Verified by reproduction.** Configuration opens the file before checking its type, without `O_NONBLOCK`. Logging accepts an existing nonregular entry and opens it for append. A `daemon.json` FIFO without a writer, or a `via.log` FIFO without a reader, blocks startup. Both isolated reproductions remained stuck after two seconds without opening the Store; the logging case holds the daemon locks. **Smallest fix:** use nonblocking, no-follow opens and validate the opened descriptor as a regular file before reading or writing. Configuration must return its specified startup error.

5. **Important — A slow size walk defeats the shared cache and competes with operational I/O.**  
   `crates/via-core/src/engine/status.rs:200`, `crates/via-store/src/runtime.rs:1697`. **Verified code path; resource-starvation scenario inferred.** After two seconds, `data_bytes` returns an error while its owned blocking walk continues. The cache remains absent or stale, and the next waiting `daemon/status` starts another walk. A walk consistently exceeding two seconds never publishes its eventual result. Longer stalls allow overlapping diagnostic walks to consume the same 16 slots used for prompt and other filesystem operations. This contradicts §5.3’s shared walk. **Smallest fix:** retain one in-flight measurement independently of request timeout and publish its completion into the cache; subsequent calls must not launch duplicates.

6. **Important — F24 does not establish the required simultaneous memory bound.**  
   `crates/via-cli/tests/s1_f24_memory.rs:98`, `crates/via-cli/tests/s1_f24_memory.rs:114`, `crates/via-cli/tests/s1_f24_memory.rs:271`. **Verified test construction.** The flood advances in two-message chunks after Core consumes previous observations. Its large text messages produce small observations; the socket readers query a session with only three events. Observation budgets and response pages consequently remain far below their maxima. The T4-6 report acknowledges these omissions, but §13.2 still requires every §5.1 holder at maximum simultaneously. An allocation defect appearing only with full observations or pages can pass this gate. **Smallest fix:** add a synchronized capacity phase that fills those holders, asserts their occupancy, and samples RSS while they coexist. This is missing required acceptance evidence, separate from deferred vendor measurements.

**Measurement suggestions**

The non-F24 latency assertion at `crates/via-cli/tests/s1_progress.rs:628` still allows only 100 ms of scheduling overhead above an injected 200 ms delay. It follows the current §13 requirement, so I would reconcile that requirement with A50: prove read count/order deterministically and measure latency separately.

**Could not verify**

I did not rerun the full acceptance suite or F24, inject filesystem stalls, or measure large-history recovery RSS. Those predicted consequences are marked inferred above.

The 11 targeted Store/Wire tests, layer check, formatting check, and scoped diff check passed. I made no tracked-file or Git-state changes. During review, HEAD advanced to `40e31ec` through a handoff-document-only commit; `crates/` remained identical to reviewed `7790912`.
