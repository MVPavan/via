# Spike: pure-Go SQLite under multi-process WAL (via-str)

Question (lang-council chair condition 2 / gate 7,
[chair.md](../lang-council/chair.md) §1 and §6): can a pure-Go SQLite driver,
built with `CGO_ENABLED=0` (no C compiler, so a single static binary), handle
several processes writing one WAL-mode database safely? WAL (write-ahead log)
is the SQLite mode where commits append to a `-wal` file and readers keep
reading a consistent snapshot while one writer works.

## Verdict on gate 7

- **modernc.org/sqlite (SQLite translated from C to Go): PASS.** Every
  scenario passed 3/3 on linux/amd64, with numbers close to the C control. It
  also passed every run under arm64 emulation.
- **ncruces/go-sqlite3 (SQLite compiled to WebAssembly, then translated to
  Go): PASS on linux/amd64, with a caveat.** It passed 3/3 natively, but
  under arm64 emulation it failed scenario 2 in 3/3 runs (13–17 writers
  timed out after 10 s). The likely cause is its busy handler (see
  Findings). Treat it as the second choice until it runs on real arm64.

Gate 7 asks for checks 2 (kill -9) and 3 (contention, 10 MB rows) with
`CGO_ENABLED=0`. Both pure-Go drivers meet it on amd64. Go keeps its build
advantage: one `go build` produced static Linux binaries for amd64 and arm64,
plus a darwin/arm64 (macOS) binary, all from this Linux host.

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
| qemu | qemu-aarch64-static 8.2.2 (installed for this spike) |

**How ncruces does WAL on Linux.** v0.35.x no longer ships a WebAssembly
runtime. SQLite's Wasm build is translated to Go source (wasm2go). WAL needs
memory shared between processes (the `-shm` index file). The driver maps
that file with `mmap(MAP_SHARED)` into the translated module's memory
(`internal/sqlite3_wrap/mmap_unix.go`, `vfs/shm_ofd.go`). It locks files with
OFD locks (Linux 3.15+; per-open-file locks, not per-process). The binary
reported `SupportsFileLocking=true SupportsSharedMemory=true`, so WAL is fully
supported. It did not need the `EXCLUSIVE` locking fallback.

## Design

`spikes/sqlite-wal/` has one binary per driver. Each binary is both the
orchestrator and the worker: it re-runs itself as child processes. Every
connection in every process runs `busy_timeout=10000` (wait up to 10 s for a
lock), `journal_mode=WAL` (and checks that it took effect),
`synchronous=FULL` (fsync on every commit), and `foreign_keys=ON`. Each
process uses one dedicated connection. Writers use `BEGIN IMMEDIATE`, which
takes the write lock at the start of the transaction.

1. **Crash:** 50 times, start a writer and wait for its first ack. Then
   `kill -9` it 0–40 ms later. Each transaction writes 1 receipt + 20 child
   rows (4 KB each). The writer prints `ACK` only after `COMMIT` returns.
   After each kill, the orchestrator reopens the database and checks three
   things: `integrity_check`, every acked receipt present, and child count
   = declared count.
2. **Contention:** a holder process takes the write lock for 5 s. Then 32
   writer processes (10 transactions each) and 8 reader processes start.
   Readers check that sum(declared children) = count(children).
3. **Large rows:** 6 processes each commit 3 × 10 MiB random BLOBs at the same
   time. The read-back sha256 must match the stored hash and the acked hash.
4. **Checkpoint:** for 15 s, 4 writers move money between 100 accounts and add
   a 4 KB row to grow the WAL. 4 readers hold ~1 s read transactions and
   re-sum balances 10× each (the total must stay 100,000). One process runs
   `PRAGMA wal_checkpoint(TRUNCATE)` (copy the WAL into the database and cut
   the WAL to 0 bytes) every 50 ms. At the end, a final TRUNCATE runs; the WAL
   must be 0 bytes while a connection is still open.

## Negative controls (1 run per driver): the checks can fail

| Scenario / control | modernc | ncruces | mattn | Expected |
|---|---|---|---|---|
| 1 `ackearly`: ack before COMMIT | FAIL, 14 lost | FAIL, 8 lost | FAIL, 9 lost | FAIL |
| 1 `notx`: no transaction | FAIL, 47 partial | FAIL, 49 partial | FAIL, 48 partial | FAIL |
| 1 `corrupt`: overwrite page 3 | FAIL (`btreeInitPage` error 11) | FAIL (same) | FAIL (same) | FAIL |
| 1 `syncoff`: synchronous=OFF | PASS | PASS | PASS | PASS (see below) |
| 2 `busy0`: busy_timeout=0 | FAIL, 320/320 BUSY | FAIL, 320/320 BUSY | FAIL, 320/320 BUSY | FAIL |
| 3 `flip`: 1 byte flipped after hashing | FAIL, 6 mismatches | FAIL, 6 | FAIL, 6 | FAIL |
| 4 `torn`: per-row reads, no checkpoints | FAIL, 21,034 torn, WAL 421 MiB | FAIL, 21,604, 433 MiB | FAIL, 25,118, 520 MiB | FAIL |

`synchronous=OFF` passing is correct and expected. `kill -9` ends the process,
but its writes stay in the operating system's page cache. Only power loss or a
kernel crash loses data that was not fsynced. **This spike does not test power
loss.** It does show that the drivers really call fsync. Under strace, for 320
commits, synchronous=FULL made 463 fsync calls with modernc, 426 fdatasync
calls with ncruces and 373 fsync calls with mattn. NORMAL made 64, 72 and 41.

## Results (real configuration, synchronous=FULL, 3 runs each, linux/amd64)

Ranges are min–max over the 3 runs.

| Driver | S1 crash | S2 contention | S3 10 MiB rows | S4 checkpoint | S5 build |
|---|---|---|---|---|---|
| modernc | **3/3 PASS**: 980–1366 acked, 0 lost, 0 partial, 0 integrity failures | **3/3 PASS**: 0 BUSY, 320/320 commits, 365–556 commits/s after release, p50 0.5 ms, p99 5438–5640 ms | **3/3 PASS**: 18 rows, 0 mismatches | **3/3 PASS**: 0 torn, 16,783–29,631 transfers, max WAL 152–240 MiB, final WAL 0 B | **PASS**: static amd64/arm64, darwin/arm64 built |
| ncruces | **3/3 PASS**: 883–1102 acked, 0 lost, 0 partial, 0 integrity failures | **3/3 PASS**: 0 BUSY, 320/320, 1103–1253/s, p50 0.9–2.0 ms, p99 5095–5138 ms | **3/3 PASS**: 18 rows, 0 mismatches | **3/3 PASS**: 0 torn, **1,009–2,002 transfers**, max WAL 4.8–7.1 MiB, final WAL 0 B | **PASS**: same |
| mattn (control) | **3/3 PASS**: 1105–1212 acked, 0 lost, 0 partial | **3/3 PASS**: 0 BUSY, 320/320, 402–639/s, p50 0.5–0.6 ms, p99 5337–5538 ms | **3/3 PASS** | **3/3 PASS**: 0 torn, 20,891–35,463 transfers, max WAL 184–544 MiB, final WAL 0 B | needs cgo (below) |

In S1, 49–50 of 50 kills landed after a writer printed `BEGIN` and before its
`ACK`, meaning inside a transaction or its commit. The p99 commit latency in
S2 includes the 5 s lock hold, because every writer's first transaction waits
for the holder. p50 is the typical commit.

**synchronous=NORMAL, S2, 1 run:** all three pass with 0 BUSY. Commits/s
after release: modernc 1,095, ncruces 4,947, mattn 1,549.

**S5 build.** With `CGO_ENABLED=0`, both pure-Go drivers built for
linux/amd64, linux/arm64 and darwin/arm64. `ldd` reports "not a dynamic
executable" for the Linux binaries. The darwin binary is Mach-O arm64; it was
not run, and `ldd` means nothing for it. mattn builds only with cgo; its Linux
binary links `libc.so.6` dynamically. Cross-building mattn would need a C
cross-compiler per target (for example `aarch64-linux-gnu-gcc`, or `zig cc`),
and a macOS SDK or osxcross for darwin. This was not attempted.

**linux/arm64 under qemu user emulation.** Children re-exec through a
binfmt_misc handler (the kernel runs foreign binaries through qemu).
modernc passed S1–S4 in 1/1 runs and S2 in 3/3 (0 BUSY, p50 3.6–4.5 ms).
ncruces passed S1, S3 and S4, but **failed S2 in 3/3 runs**: 13–17 BUSY,
p50 986–1092 ms, max 10.1–10.5 s.

## Findings

1. **Checkpoint TRUNCATE can starve writers. This is SQLite behaviour, not a
   driver fault.** In S4, `writer_busy_errors` was 0–2 for modernc, 0–1 for
   ncruces and 2–4 for the C control. SQLite documents that FULL, RESTART and
   TRUNCATE checkpoints "block new database writers while pending" while they
   wait for readers. With long readers, a writer can use up its 10 s timeout.
   S4 therefore records writer BUSY rather than failing on it. Implication
   (inference): VIA should not run blocking checkpoints while receipts are
   being written. Automatic PASSIVE checkpoints did not block anyone here.
2. **ncruces polls for locks.** It replaces SQLite's busy handler with a retry
   loop that sleeps 0–2 ms at random between attempts (`conn.go`,
   `Xgo_busy_timeout`); SQLite's own handler backs off up to 100 ms. In S2 on
   amd64 it used more CPU: user 6.0 s and system 5.2 s, against 3.7 s and 3.0 s
   (modernc) and 3.4 s and 2.5 s (mattn). This likely explains both its higher
   native throughput and its timeouts when CPU-starved. Under qemu, 40
   processes share 4 CPUs. This is inferred, not proven.
3. **ncruces wrote 10–20× fewer transfers in S4** (1,009–2,002 against
   17k–35k), with no errors. The cause was not established. Without the
   checkpointer (the `torn` control), it wrote 26,684, like the others.
4. On this VM, fsync takes about 120 µs, so commit latencies are optimistic
   for real disks.

## Limitations

- Real runs were on Linux amd64 only. arm64 ran only under qemu emulation;
  macOS was only cross-built; Windows was not tested. Everything ran on a
  single machine and a single filesystem (ext4). No network filesystems.
- No power-loss or kernel-crash test. `kill -9` cannot show whether data that
  was not fsynced survives (see the `syncoff` control).
- **Toolchain deviation:** the brief said to use the installed Go (1.24.7).
  The current driver releases require newer Go: modernc v1.59.0 needs
  ≥1.25, and ncruces v0.35.6 needs ≥1.26. `go.mod` pins `toolchain go1.26.8`,
  which Go downloads itself. Older driver versions would have run on 1.24.7.
  Revisit this if VIA must build with an older Go.
- S2 uses 10 transactions per writer. That is smaller than the chair's "32
  concurrent runs" load in wall time; each run lasts about 5.5 s.
- Each scenario ran 3 times, so this is not a soak test. Rare races (under
  1 in about 3,000 kills) would not show up.

## Reproduce

From `spikes/sqlite-wal/` (DATA_DIR on a real disk, not tmpfs):

```sh
go version                      # go.mod selects go1.26.8 automatically
go vet ./...
sh build.sh                     # S5: cross-builds + host binaries into bin/
for f in bin/*-linux-*; do ldd "$f"; done
sh run.sh DATA_DIR > results.jsonl   # negatives, 3 real runs, S2 NORMAL
bin/sqlitewal-modernc -dir DATA_DIR -scenario 2 -runs 1 -neg busy0   # one control
# arm64 under qemu (needs binfmt_misc; as root):
apt-get install -y qemu-user-static
mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc
cat /usr/lib/binfmt.d/qemu-aarch64.conf > /proc/sys/fs/binfmt_misc/register
bin/sqlitewal-ncruces-linux-arm64 -dir DATA_DIR -runs 1
# CPU and fsync evidence:
strace -f -qq -c -e trace=fsync,fdatasync bin/sqlitewal-ncruces -dir DATA_DIR -scenario 2 -runs 1
```

Each run prints one JSON line with `pass`, `metrics` and `failures`.
