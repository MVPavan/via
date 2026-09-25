# Spike: pure-Go SQLite under multi-process WAL (via-str)

Question (lang-council chair condition 2 / gate 7,
[chair.md](../lang-council/chair.md) §1 and §6): can a pure-Go SQLite driver,
built with `CGO_ENABLED=0` (no C compiler, so a single static binary), handle
several processes writing one WAL-mode database safely? WAL (write-ahead log)
is the SQLite mode where commits append to a `-wal` file and readers keep a
consistent snapshot while one writer works. Revision 2 answers an independent
review (GPT-6 Sol) that found the first version's tests weaker than its verdict.

## Verdict

Gate 7 says: "passes checks 2 and 3 with `CGO_ENABLED=0`". Check 2 is S1
below (50 kills inside a transaction). Check 3 is S6: 32 concurrent writers
with 10 MB lines, a DB locked for 5 s, 0 BUSY, hashes identical.

- **modernc.org/sqlite (C translated to Go): meets gate 7 on linux/amd64.**
  S1 and S6 passed 3/3, and so did the supporting S2–S4. Its results match
  the C control.
- **ncruces/go-sqlite3 (SQLite compiled to Wasm, then translated to Go): meets
  gate 7 on linux/amd64. On arm64 it is unproven.** It passed 3/3 natively.
  Under arm64 emulation it failed S2 (see arm64 below), and no real arm64
  machine was available.
- **Build gate (chair check 6) is only partly met.** Both pure-Go drivers
  built static binaries for linux/amd64, linux/arm64 and darwin/arm64. The
  amd64 binaries ran natively; the arm64 ones ran only under qemu emulation.
  **The darwin/arm64 binaries were built but never run.** The chair's gate
  says "artifacts run", so macOS still needs one run on a Mac.
- Not tested: power loss, real arm64 hardware, macOS at run time, Windows.

## Environment

| Item | Value |
|---|---|
| Go | go1.26.8 linux/amd64 (installed toolchain is go1.24.7; see Limitations) |
| OS / kernel | Ubuntu 24.04.4 LTS, Linux 6.18.44 (Firecracker VM) |
| CPU / RAM | 4 × Intel Xeon @ 2.10GHz, 15 GiB |
| Filesystem | ext4 on `/dev/vda` (virtio disk, not tmpfs), `rw,relatime` |
| modernc.org/sqlite | v1.59.0 (SQLite 3.53.4), modernc.org/libc v1.75.7 |
| github.com/ncruces/go-sqlite3 | v0.35.6 (SQLite 3.53.4), go-sqlite3-wasm/v6 v6.3.35304 |
| github.com/mattn/go-sqlite3 | v1.14.52 (SQLite 3.53.4), cgo with gcc 13.3.0 (control) |
| qemu | qemu-aarch64-static 8.2.2 via binfmt_misc (installed for this spike) |

**How ncruces does WAL on Linux.** There is no Wasm runtime: the SQLite Wasm
build is translated to Go source (wasm2go). The `-shm` WAL index is shared
between processes by `mmap(MAP_SHARED)` into the module's memory
(`internal/sqlite3_wrap/mmap_unix.go`, `vfs/shm_ofd.go`), and files are
locked with OFD locks (per-open-file locks, Linux 3.15+). The binary reports
`SupportsSharedMemory=true`; S1–S6 then exercised WAL across processes.

## Design

`spikes/sqlite-wal/` builds one binary per driver. Each binary is the
orchestrator and re-runs itself as worker processes. Every connection sets
`busy_timeout=10000` (wait up to 10 s for a lock), `journal_mode=WAL`
(checked), `synchronous=FULL` (fsync each commit) and `foreign_keys=ON`,
with one connection per process. Writers use `BEGIN IMMEDIATE` (take the
write lock at the start). Every check after a run happens in a **newly
spawned verifier process**. It runs `integrity_check`, requires every acked
receipt and no other receipts, checks child counts, and re-hashes raw logs.

- **S1 crash (check 2).** There are 50 gate kills. The writer commits 0–4
  transactions, runs `BEGIN IMMEDIATE`, and writes the receipt plus 10 of 20
  children. It prints `INTXN` and stops, and then gets `kill -9`. Its page
  cache is 8 pages, so uncommitted pages are already in the WAL. Pass: the
  kill is confirmed inside the transaction, the process died by SIGKILL,
  every acked receipt survives, the killed receipt is absent, nothing is
  partial, and integrity is ok. 50 more kills land at random times
  (supplementary; these can hit a COMMIT in progress).
- **S2 contention.** A holder takes the write lock. 32 writers (10 small
  transactions each) and 8 readers open, prepare and print READY. The lock is
  released only after every client is ready **and** 5 s have passed. Pass: 0
  errors of any kind from open to COMMIT (readers' COMMIT included); every
  writer's first BEGIN waited ≥ 100 ms; each reader completed ≥ 20
  transactions; the holder exited cleanly after ≥ 5 s; the verifier found
  exactly the 320 acked receipts.
- **S3 large rows.** 6 processes × 3 rows of 10 MiB, committed concurrently;
  the read-back sha256 must match the writer's.
- **S4 checkpoint.** For 15 s: 4 writers move money between 100 accounts; 4
  readers hold ~1 s read transactions, re-summing 10× (the total must stay
  100,000); one process runs `wal_checkpoint(TRUNCATE)` every 50 ms. TRUNCATE
  copies the WAL into the database and cuts it to 0 bytes. Pass: 0 torn reads;
  ≥ 500 transfers; ≥ 5 snapshots per reader; ≥ 1 TRUNCATE completing while
  writers and readers ran; max WAL ≤ 1 GiB; 0-byte WAL after a final TRUNCATE;
  no errors except **writer BUSY, which is allowed** (Findings 1).
- **S6 combined check 3.** This is S2's barrier with 32 writers × 1
  transaction. Each writes a receipt plus a single 10 MiB raw-log line. The
  writer hashes the line (sha256) *before* the insert and reports the hash,
  and the verifier re-hashes every line read back. Readers check each receipt
  has one log of the right length. The pass rules are S2's plus 32/32 hashes
  equal.

## Negative controls (1 run per driver): each check can fail

`run.sh` asserts every expected outcome; the final matrix reported
`surprises: 0` across 108 result lines. Where numbers differ they are
listed as modernc / ncruces / mattn.

| Control | Fault injected | Result (all 3 drivers) |
|---|---|---|
| S1 `ackearly` | ack printed before COMMIT | FAIL: acked receipts missing |
| S1 `notx` | no transaction | FAIL: 50/50 killed receipts visible, partial receipts |
| S1 `corrupt` | page 3 overwritten | FAIL: `btreeInitPage() returns error code 11` |
| S2 `busy0` | busy_timeout=0 | FAIL: 320/320 BUSY |
| S3 `flip` | byte flipped after hashing | FAIL: 6 sha256 mismatches |
| S4 `readtorn` | per-row reads outside a transaction | FAIL: 20,573 / 20,823 / 20,210 torn reads |
| S4 `nockpt` | no checkpoints | FAIL: 0 TRUNCATEs, WAL left at 667 / 599 / 700 MB |
| S6 `busy0` | busy_timeout=0 | FAIL: 32/32 BUSY, 0 writers contended |
| S6 `flip` | bit flipped after hashing | FAIL: 32 sha256 mismatches in the verifier |
| S1 `syncoff` | synchronous=OFF | PASS, as expected: `kill -9` keeps the OS page cache |

Power loss is not tested. strace confirmed each driver calls fsync on every
commit: for 320 commits, FULL made 463 / 426 / 373 fsync or fdatasync calls
(modernc / ncruces / mattn), and NORMAL made 64 / 72 / 41.

## Results (real configuration, synchronous=FULL, 3 runs each, linux/amd64)

Ranges are min–max over 3 runs. Commit latency is measured from BEGIN to COMMIT
returning, so it includes the 5 s held lock; in S2, p99 ≈ the hold.

| Driver | S1 crash (check 2) | S2 contention | S3 10 MiB rows | S4 checkpoint | S6 combined (check 3) |
|---|---|---|---|---|---|
| modernc | **3/3**: 50/50 kills in txn, 100/100 SIGKILL, 1117–1288 acked, 0 lost, 0 visible, 0 partial, integrity ok | **3/3**: 0 errors, 32/32 contended, 356–457 commits/s after release, p50 0.5–0.7 ms | **3/3**: 0 mismatches | **3/3**: 0 torn, 7.2k–31k transfers, 3–8 TRUNCATEs during run, max WAL 39–386 MB, final 0 B, writer BUSY 1–2 | **3/3**: 0 BUSY, 32/32 contended, 32/32 hashes equal, readers ≥ 5,768 txns, commit p50 5.7–6.0 s, max 7.0–7.5 s |
| ncruces | **3/3**: same, 990–1280 acked, 0 lost | **3/3**: 0 errors, 32/32, 980–995 commits/s, p50 9–11 ms | **3/3** | **3/3**: 0 torn, 1.6k–2.7k transfers, 7–8 TRUNCATEs, max WAL 8–11 MB, writer BUSY 0 | **3/3**: 0 BUSY, 32/32, 32/32 equal, ≥ 5,290 txns, p50 5.5–5.6 s, max 6.5–6.6 s |
| mattn (cgo control) | **3/3**: same, 1200–1397 acked, 0 lost | **3/3**: 0 errors, 32/32, 401–466 commits/s, p50 0.5–0.6 ms | **3/3** | **3/3**: 0 torn, 26k–30k transfers, 2–4 TRUNCATEs, max WAL 317–489 MB, writer BUSY 1–3 | **3/3**: 0 BUSY, 32/32, 32/32 equal, ≥ 5,698 txns, p50 5.5–5.7 s, max 6.8–7.2 s |

In S6, all clients were ready 0.36–0.41 s after the lock was taken. After the
release, the 32 × 10 MiB commits took 1.9–2.8 s. In S2 with synchronous=NORMAL
(1 run), commits/s were 655 (modernc), 4,604 (ncruces) and 1,152 (mattn).

**S5 build.** `build.sh` builds with `-trimpath -buildvcs=false`, so the
sha256 depends only on source and toolchain. Two builds, one from a clean
cache, gave identical hashes. All four Linux binaries have **no PT_INTERP
segment** (`readelf -lW`), so there is no dynamic loader, and `ldd` reports
"not a dynamic executable". The mattn control requests
`/lib64/ld-linux-x86-64.so.2`. Hashes at the report commit (go1.26.8):

| Artifact | sha256 |
|---|---|
| modernc linux/amd64 | `1f30cc793a629bf011d7323fa869a72d2d2305e865b0b654c68111640bffcdd1` |
| modernc linux/arm64 | `2e099cc434440792d337d792c167e986e3fc83f8fd834d92db10b0c435d6cbdb` |
| modernc darwin/arm64 (not run) | `8a59ec7269209f35fc5411e48b8157c12444b60d5910a50fd45f895e55ebd44b` |
| ncruces linux/amd64 | `29d5d2203b3b519a239c62d02b8bfea1e00d300d34d30eebeb5ae9e8ae8c87cc` |
| ncruces linux/arm64 | `14bdaed8c6248f22cbe261419124952d8d0b51d7f0e79cd258eb35ea22c7fce0` |
| ncruces darwin/arm64 (not run) | `55dd776f4fd04b828739dcc3824c051cf27aefb12f125424cbdb79fc681ac4bd` |

mattn builds only with cgo. Cross-building it would need a C cross-compiler
per target (e.g. `aarch64-linux-gnu-gcc` or `zig cc`) and a macOS SDK; this
was not attempted.

**linux/arm64 under qemu user emulation (1 run each, 40 processes on 4
CPUs).** ARM64_RESULTS

## Findings

1. **A TRUNCATE checkpoint can make writers see BUSY. All three drivers,
   including the C control, show it.** SQLite documents that FULL, RESTART and
   TRUNCATE checkpoints "block new database writers while pending" while
   waiting for readers. S4 therefore allows writer BUSY and reports it:
   modernc 1–2, ncruces 0, mattn 1–3 per run. S1, S2 and S6 allow no BUSY at
   all. Implication (inference): VIA should not run blocking checkpoints
   while receipts are being written.
2. **S6 leaves limited busy_timeout margin.** The slowest writer waited
   6.5–7.5 s of its 10 s timeout on this VM, where fsync takes about 120 µs.
   On slower disks the same load could exceed 10 s. VIA's timeout must
   exceed the hold plus the queue of large commits (inference).
3. **ncruces behaves differently under contention; the cause is unproven.**
   It replaces SQLite's busy handler with a retry every 0–2 ms (random),
   where SQLite's own handler backs off to 100 ms (`conn.go`,
   `Xgo_busy_timeout`). In S2 it used more CPU: user 6.0 s and system 5.2 s,
   against 3.4–3.7 s and 2.5–3.0 s for the others (one run). It reached the
   highest S2 throughput and completed the most TRUNCATEs in S4, with
   the fewest transfers and the smallest WAL. These observations fit the
   polling handler, but no lock-wait trace was taken, so the link is a
   hypothesis. The same goes for its emulated-arm64 failures.
4. **S4's overlap minimum was lowered from 3 to 1.** With continuously
   overlapping 1 s readers, the C control completed only 2–4 TRUNCATEs in
   15 s (2 in two smoke runs), so 3 would fail the control itself. A minimum
   of 1 still separates pass from the `nockpt` control, which completes 0.
   The 1 GiB WAL bound is a disk-safety bound, not evidence. The TRUNCATE
   count and the 0-byte final WAL are the evidence.

## Response to the review

Fixed: combined check 3 (S6, finding 1); confirmed in-transaction kills,
SIGKILL check and fresh-process verifier (2); readiness barrier, all COMMITs,
holder exit, verifier and reader minimum in S2 (3); S4 assertions with an
explicit BUSY allowance (4); scan errors, asserted controls, split S4 controls
and the PT_INTERP check (5); build wording, retained hash attestations and an
unproven arm64 cause (6, 7). Nothing was rejected. Unchanged: darwin still
has not been run (no Mac here), and the Linux binaries are not committed,
only their hashes.

## Limitations

- Linux amd64 native only. arm64 ran under qemu emulation; macOS was built
  but not run; Windows was not tested. One machine and one filesystem (ext4);
  no network filesystems.
- No power-loss or kernel-crash test (see the `syncoff` control).
- **Toolchain deviation:** the brief said to use the installed Go (1.24.7).
  The current drivers need newer Go (modernc v1.59.0 needs ≥1.25; ncruces
  v0.35.6 needs ≥1.26), so `go.mod` pins `toolchain go1.26.8`, which Go
  downloads itself. Revisit if VIA must build with an older Go.
- 3 runs per scenario is not a soak test; rare races (under about 1 in 300
  kills per driver) would not show up.

## Reproduce

From `spikes/sqlite-wal/` (DATA_DIR on a real disk, not tmpfs):

```sh
go vet ./...
sh build.sh                            # S5: builds, PT_INTERP/ldd checks, sha256
sh run.sh DATA_DIR > results.jsonl     # controls + 3 real runs; exits 1 on surprises
bin/sqlitewal-modernc -dir DATA_DIR -scenario 6 -runs 1             # one scenario
bin/sqlitewal-modernc -dir DATA_DIR -scenario 6 -runs 1 -neg flip   # one control
# arm64 under qemu (as root; binfmt_misc lets the harness re-exec itself):
apt-get install -y qemu-user-static
mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc
cat /usr/lib/binfmt.d/qemu-aarch64.conf > /proc/sys/fs/binfmt_misc/register
bin/sqlitewal-ncruces-linux-arm64 -dir DATA_DIR -runs 1
strace -f -qq -c -e trace=fsync,fdatasync bin/sqlitewal-ncruces -dir DATA_DIR -scenario 2 -runs 1
```

Each run prints one JSON line with `pass`, `metrics` and `failures`.
