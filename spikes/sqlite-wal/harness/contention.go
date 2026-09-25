package harness

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/rand/v2"
	"slices"
	"strconv"
	"strings"
	"time"
)

// Scenarios 2 and 6 share one shape. A holder process takes the write lock.
// Writer and reader processes open their connections, prepare their
// payloads and report READY. The holder releases the lock only when every
// client is ready and at least 5 s have passed, so every writer's first
// BEGIN IMMEDIATE is already waiting. No BUSY/LOCKED error may surface
// anywhere from open to COMMIT. A fresh verifier process then checks the
// exact set of committed rows.
//
// Scenario 2: 32 writers x 10 transactions of 1 receipt + 5 child rows.
// Scenario 6 (chair check 3 combined): 32 writers x 1 transaction of 1
// receipt + one 10 MiB raw-log line. Each writer hashes its line (sha256)
// before the insert and reports the hash; the verifier re-hashes every
// line read back from SQLite.
//
// Negative controls (-neg): busy0 (busy_timeout=0: BUSY must surface) for
// both; flip (a byte flipped after hashing: the sha256 check must fire) for 6.
type contentionCfg struct {
	writers, readers, txns, children, size int
	minReaderTxns                          int
}

var (
	s2Cfg = contentionCfg{writers: 32, readers: 8, txns: 10, children: 5, minReaderTxns: 20}
	s6Cfg = contentionCfg{writers: 32, readers: 8, txns: 1, size: 10 << 20, minReaderTxns: 20}
)

const (
	minHold         = 5 * time.Second
	contendedWaitMS = 100 // a writer's first BEGIN must have waited at least this long
)

func contentionScenario(ctx context.Context, path, neg, sync string, r *Result) error {
	return runContention(ctx, path, neg, sync, s2Cfg, r)
}

func combinedScenario(ctx context.Context, path, neg, sync string, r *Result) error {
	return runContention(ctx, path, neg, sync, s6Cfg, r)
}

type workerReport struct {
	OK          int     `json:"ok"`
	Busy        int     `json:"busy"`
	Other       int     `json:"other"`
	Bad         int     `json:"bad"` // invariant violations seen by readers
	LatencyUS   []int64 `json:"lat_us,omitempty"`
	FirstWaitMS int64   `json:"first_wait_ms"` // writer: first BEGIN IMMEDIATE wait
	FirstErr    string  `json:"first_err,omitempty"`
	// S4 overlap evidence, as Unix nanoseconds: each reader snapshot's
	// [BEGIN, COMMIT] span, and a writer's first-start and last-commit times.
	Spans   [][2]int64 `json:"spans,omitempty"`
	FirstNS int64      `json:"first_ns,omitempty"`
	LastNS  int64      `json:"last_ns,omitempty"`
}

func (w *workerReport) record(err error) {
	if isBusy(err) {
		w.Busy++
	} else {
		w.Other++
	}
	if w.FirstErr == "" {
		w.FirstErr = err.Error()
	}
}

func (t *workerReport) add(rep workerReport) {
	t.OK += rep.OK
	t.Busy += rep.Busy
	t.Other += rep.Other
	t.Bad += rep.Bad
	t.LatencyUS = append(t.LatencyUS, rep.LatencyUS...)
	if t.FirstErr == "" {
		t.FirstErr = rep.FirstErr
	}
}

func runContention(ctx context.Context, path, neg, sync string, cfg contentionCfg, r *Result) error {
	o := defaultOpts()
	o.sync = sync
	if err := initDB(ctx, path, o); err != nil {
		return err
	}
	mode := "normal"
	switch {
	case neg == "":
	case neg == "busy0":
		o.busyMS = 0
	case neg == "flip" && cfg.size > 0:
		mode = "flip"
	default:
		return fmt.Errorf("unknown negative control %q", neg)
	}
	w := workerFlags{db: path, mode: mode, opts: o}
	hw := w // the holder always waits normally so it reliably takes the lock
	hw.opts.busyMS = 10000
	h, err := spawn("holder", hw.args()...)
	if err != nil {
		return err
	}
	defer h.cmd.Process.Kill()
	if l := <-h.lines; l != "LOCKED" {
		return fmt.Errorf("holder: expected LOCKED, got %q", l)
	}
	locked := time.Now()

	common := []string{"-n", strconv.Itoa(cfg.txns), "-children", strconv.Itoa(cfg.children), "-size", strconv.Itoa(cfg.size)}
	var ws, rs []*child
	for i := range cfg.readers {
		c, err := spawn("reader", w.args(append([]string{"-id", strconv.Itoa(i + 1)}, common...)...)...)
		if err != nil {
			return err
		}
		defer c.cmd.Process.Kill()
		rs = append(rs, c)
	}
	for i := range cfg.writers {
		c, err := spawn("writer", w.args(append([]string{"-id", strconv.Itoa(i + 1)}, common...)...)...)
		if err != nil {
			return err
		}
		defer c.cmd.Process.Kill()
		ws = append(ws, c)
	}
	// Barrier: every client must report READY while the lock is held.
	early := map[*child][]string{}
	deadline := time.After(2 * time.Minute)
	for _, c := range append(slices.Clone(rs), ws...) {
		for ready := false; !ready; {
			select {
			case l, ok := <-c.lines:
				if !ok {
					return fmt.Errorf("client exited before READY (%v)", c.cmd.Wait())
				}
				ready = l == "READY"
				early[c] = append(early[c], l)
			case <-deadline:
				return fmt.Errorf("clients not ready within 2 minutes")
			}
		}
	}
	allReady := time.Since(locked)
	time.Sleep(max(minHold-time.Since(locked), 300*time.Millisecond))
	fmt.Fprintln(h.stdin, "RELEASE")
	released := time.Now()

	var wt, rt workerReport
	prep := map[int]string{} // receipt id -> sha256 reported before the insert
	acked := map[int]string{}
	contended := 0
	for i, c := range ws {
		lines := append(early[c], c.drain()...)
		if err := c.cmd.Wait(); err != nil {
			r.failf("writer %d exited: %v", i+1, err)
		}
		var rep workerReport
		for _, l := range lines {
			f := strings.Fields(l)
			switch {
			case len(f) == 3 && f[0] == "PREP":
				prep[atoiMust(f[1])] = f[2]
			case len(f) == 2 && f[0] == "ACK":
				acked[atoiMust(f[1])] = ""
			case strings.HasPrefix(l, "{"):
				if err := json.Unmarshal([]byte(l), &rep); err != nil {
					r.failf("writer %d: bad report: %v", i+1, err)
				}
			}
		}
		wt.add(rep)
		if rep.OK > 0 && rep.FirstWaitMS >= contendedWaitMS {
			contended++
		}
	}
	done := time.Now()
	for _, c := range rs {
		c.stdin.Close()
	}
	var minReader = -1
	for i, c := range rs {
		lines := c.drain()
		if err := c.cmd.Wait(); err != nil {
			r.failf("reader %d exited: %v", i+1, err)
		}
		var rep workerReport
		if len(lines) == 0 || json.Unmarshal([]byte(lines[len(lines)-1]), &rep) != nil {
			r.failf("reader %d: no report", i+1)
		}
		rt.add(rep)
		if minReader < 0 || rep.OK < minReader {
			minReader = rep.OK
		}
	}
	hl := h.drain()
	if err := h.cmd.Wait(); err != nil {
		r.failf("holder exited: %v", err)
	}
	held := 0.0
	if len(hl) == 0 || !strings.HasPrefix(hl[len(hl)-1], "RELEASED ") {
		r.failf("holder did not report RELEASED: %q", hl)
	} else if held, err = strconv.ParseFloat(strings.TrimPrefix(hl[len(hl)-1], "RELEASED "), 64); err != nil {
		return err
	}

	// Every acked receipt must be present, with its pre-insert hash, and
	// nothing else may be committed.
	missingPrep := 0
	for id := range acked {
		acked[id] = prep[id]
		if cfg.size > 0 && prep[id] == "" {
			missingPrep++
		}
	}
	vrep, err := runVerifier(path, defaultOpts(), manifest{Present: acked, Exact: true})
	if err != nil {
		return err
	}
	for _, p := range vrep.problems() {
		r.failf("verifier: %s", p)
	}

	expected := cfg.writers * cfg.txns
	slices.Sort(wt.LatencyUS)
	pct := func(p int) float64 {
		if len(wt.LatencyUS) == 0 {
			return 0
		}
		return float64(wt.LatencyUS[min(len(wt.LatencyUS)-1, len(wt.LatencyUS)*p/100)]) / 1000
	}
	after := done.Sub(released).Seconds()
	r.Metrics["writers"] = cfg.writers
	r.Metrics["readers"] = cfg.readers
	r.Metrics["payload_bytes"] = cfg.size
	r.Metrics["commits"] = wt.OK
	r.Metrics["acked"] = len(acked)
	r.Metrics["expected_commits"] = expected
	r.Metrics["writers_contended"] = contended
	r.Metrics["all_ready_after_s"] = allReady.Seconds()
	r.Metrics["lock_held_s"] = held
	r.Metrics["reader_txns"] = rt.OK
	r.Metrics["min_reader_txns"] = minReader
	r.Metrics["busy_errors"] = wt.Busy + rt.Busy + vrep.Busy
	r.Metrics["other_errors"] = wt.Other + rt.Other
	r.Metrics["reader_invariant_violations"] = rt.Bad
	r.Metrics["first_error"] = wt.FirstErr + rt.FirstErr
	r.Metrics["verifier_rows"] = vrep.Receipts
	r.Metrics["verifier_logs_hashed"] = vrep.LogsChecked
	r.Metrics["sha_mismatches"] = vrep.ShaMismatch
	r.Metrics["acked_without_prep_hash"] = missingPrep
	r.Metrics["wall_after_release_s"] = after
	r.Metrics["commits_per_s_after_release"] = float64(wt.OK) / after
	r.Metrics["commit_p50_ms"] = pct(50)
	r.Metrics["commit_p99_ms"] = pct(99)
	r.Metrics["commit_max_ms"] = pct(100)
	r.Metrics["busy_timeout_ms"] = o.busyMS

	if n := wt.Busy + rt.Busy + wt.Other + rt.Other; n > 0 {
		r.failf("%d BUSY/LOCKED and %d other errors surfaced; first: %s%s", wt.Busy+rt.Busy, wt.Other+rt.Other, wt.FirstErr, rt.FirstErr)
	}
	if rt.Bad > 0 {
		r.failf("%d reader invariant violations", rt.Bad)
	}
	if wt.OK != expected || len(acked) != expected {
		r.failf("commits %d, acked %d, expected %d", wt.OK, len(acked), expected)
	}
	if cfg.size > 0 {
		if missingPrep > 0 {
			r.failf("%d acked receipts have no pre-insert hash", missingPrep)
		}
		if vrep.LogsChecked != expected {
			r.failf("verifier hashed %d raw logs, expected %d", vrep.LogsChecked, expected)
		}
	}
	if contended != cfg.writers {
		r.failf("only %d of %d writers waited >= %d ms on their first BEGIN", contended, cfg.writers, contendedWaitMS)
	}
	if minReader < cfg.minReaderTxns {
		r.failf("a reader completed only %d transactions (minimum %d)", minReader, cfg.minReaderTxns)
	}
	if held < minHold.Seconds() {
		r.failf("lock held %.2f s, want >= %.0f s", held, minHold.Seconds())
	}
	return nil
}

func atoiMust(s string) int {
	n, err := strconv.Atoi(s)
	if err != nil {
		panic(fmt.Sprintf("bad integer %q from worker", s))
	}
	return n
}

// holder takes the write lock, prints LOCKED, and commits when the
// orchestrator writes a line to its stdin. It prints the time it held the lock.
func holder(ctx context.Context, w workerFlags) error {
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		return err
	}
	defer db.Close()
	if _, err := conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
		return err
	}
	t := time.Now()
	fmt.Println("LOCKED")
	var line string
	fmt.Scanln(&line)
	if _, err := conn.ExecContext(ctx, "COMMIT"); err != nil {
		return err
	}
	fmt.Printf("RELEASED %.3f\n", time.Since(t).Seconds())
	return nil
}

// rawLog makes a single size-byte log line of printable characters.
func rawLog(id, size int) []byte {
	const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789 "
	b := make([]byte, size)
	rand.NewChaCha8([32]byte{byte(id), byte(id >> 8), byte(id >> 16), byte(id >> 24)}).Read(b)
	for i := range b {
		b[i] = alphabet[int(b[i])%len(alphabet)]
	}
	b[size-1] = '\n'
	return b
}

// contentionWriter prepares its payloads (printing "PREP id sha256" for each
// raw log), prints READY, then commits n receipts, printing "ACK id" after
// each COMMIT returns. Every error from open to COMMIT is counted.
func contentionWriter(ctx context.Context, w workerFlags) error {
	var rep workerReport
	defer func() { emit(rep) }()
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		rep.record(err)
		fmt.Println("READY")
		return nil
	}
	defer db.Close()
	logs := map[int][]byte{}
	for seq := 1; seq <= w.n && w.size > 0; seq++ {
		id := receiptID(w.id, seq)
		data := rawLog(id, w.size)
		sum := sha256.Sum256(data)
		fmt.Printf("PREP %d %s\n", id, hex.EncodeToString(sum[:]))
		if w.mode == "flip" {
			data[len(data)/2] ^= 0x01
		}
		logs[id] = data
	}
	payload := make([]byte, 256)
	fmt.Println("READY")
	for seq := 1; seq <= w.n; seq++ {
		id := receiptID(w.id, seq)
		t := time.Now()
		err := func() error {
			if _, err := conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
				return err
			}
			if seq == 1 {
				rep.FirstWaitMS = time.Since(t).Milliseconds()
			}
			if _, err := conn.ExecContext(ctx, "INSERT INTO receipts(id, worker, seq, nchild) VALUES (?,?,?,?)", id, w.id, seq, w.children); err != nil {
				return err
			}
			for c := range w.children {
				if _, err := conn.ExecContext(ctx, "INSERT INTO children(receipt_id, idx, payload) VALUES (?,?,?)", id, c, payload); err != nil {
					return err
				}
			}
			if data, ok := logs[id]; ok {
				if _, err := conn.ExecContext(ctx, "INSERT INTO logs(receipt_id, data) VALUES (?,?)", id, data); err != nil {
					return err
				}
			}
			_, err := conn.ExecContext(ctx, "COMMIT")
			return err
		}()
		if err != nil {
			rep.record(err)
			conn.ExecContext(ctx, "ROLLBACK") // cleanup only; the error is already counted
			continue
		}
		rep.OK++
		rep.LatencyUS = append(rep.LatencyUS, time.Since(t).Microseconds())
		fmt.Printf("ACK %d\n", id)
	}
	return nil
}

// contentionReader prints READY, then runs read transactions until stdin
// closes. Each checks that every receipt has its declared children and, when
// raw logs are in play, exactly one log of the expected length.
func contentionReader(ctx context.Context, w workerFlags) error {
	var rep workerReport
	defer func() { emit(rep) }()
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		rep.record(err)
		fmt.Println("READY")
		return nil
	}
	defer db.Close()
	fmt.Println("READY")
	stop := untilEOF()
	for !stop() {
		err := func() error {
			if _, err := conn.ExecContext(ctx, "BEGIN"); err != nil {
				return err
			}
			var want, got, badLogs int
			if err := conn.QueryRowContext(ctx, "SELECT coalesce(sum(nchild),0) FROM receipts").Scan(&want); err != nil {
				conn.ExecContext(ctx, "ROLLBACK")
				return err
			}
			if err := conn.QueryRowContext(ctx, "SELECT count(*) FROM children").Scan(&got); err != nil {
				conn.ExecContext(ctx, "ROLLBACK")
				return err
			}
			if w.size > 0 {
				q := `SELECT count(*) FROM receipts r LEFT JOIN logs l ON l.receipt_id = r.id WHERE l.receipt_id IS NULL OR length(l.data) != ?`
				if err := conn.QueryRowContext(ctx, q, w.size).Scan(&badLogs); err != nil {
					conn.ExecContext(ctx, "ROLLBACK")
					return err
				}
			}
			if want != got || badLogs != 0 {
				rep.Bad++
			}
			_, err := conn.ExecContext(ctx, "COMMIT")
			return err
		}()
		if err != nil {
			rep.record(err)
			continue
		}
		rep.OK++
		time.Sleep(time.Millisecond)
	}
	return nil
}
