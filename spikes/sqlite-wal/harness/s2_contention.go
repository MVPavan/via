package harness

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"time"
)

// Scenario 2: 32 writer and 8 reader processes start while a holder process
// keeps BEGIN IMMEDIATE open for 5 s. No caller may see SQLITE_BUSY/LOCKED.
//
// Negative control (-neg busy0): busy_timeout=0, so BUSY errors must appear.
const (
	writers       = 32
	readers       = 8
	txnsPerWriter = 10
)

type workerReport struct {
	OK        int     `json:"ok"`
	Busy      int     `json:"busy"`
	Other     int     `json:"other"`
	Bad       int     `json:"bad"` // invariant violations seen by readers
	LatencyUS []int64 `json:"lat_us,omitempty"`
	FirstErr  string  `json:"first_err,omitempty"`
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

func contentionScenario(ctx context.Context, path, neg, sync string, r *Result) error {
	o := defaultOpts()
	o.sync = sync
	if err := initDB(ctx, path, o); err != nil {
		return err
	}
	switch neg {
	case "":
	case "busy0":
		o.busyMS = 0
	default:
		return fmt.Errorf("unknown negative control %q", neg)
	}
	w := workerFlags{db: path, mode: "normal", opts: o}
	// The holder always uses the real busy_timeout so it reliably takes the lock.
	hw := w
	hw.opts.busyMS = 10000
	h, err := spawn("holder", hw.args("-hold", "5s")...)
	if err != nil {
		return err
	}
	if l := <-h.lines; l != "LOCKED" {
		return fmt.Errorf("holder: expected LOCKED, got %q", l)
	}
	start := time.Now()
	var ws, rs []*child
	for i := range readers {
		c, err := spawn("reader", w.args("-id", strconv.Itoa(i+1))...)
		if err != nil {
			return err
		}
		rs = append(rs, c)
	}
	for i := range writers {
		c, err := spawn("writer", w.args("-id", strconv.Itoa(i+1), "-n", strconv.Itoa(txnsPerWriter), "-children", "5")...)
		if err != nil {
			return err
		}
		ws = append(ws, c)
	}
	var total workerReport
	collect := func(c *child, who string) {
		lines := c.drain()
		if err := c.cmd.Wait(); err != nil {
			r.failf("%s exited: %v", who, err)
		}
		var rep workerReport
		if len(lines) == 0 || json.Unmarshal([]byte(lines[len(lines)-1]), &rep) != nil {
			r.failf("%s: no report", who)
			return
		}
		total.OK += rep.OK
		total.Busy += rep.Busy
		total.Other += rep.Other
		total.Bad += rep.Bad
		total.LatencyUS = append(total.LatencyUS, rep.LatencyUS...)
		if rep.FirstErr != "" && total.FirstErr == "" {
			total.FirstErr = rep.FirstErr
		}
	}
	for i, c := range ws {
		collect(c, fmt.Sprintf("writer %d", i+1))
	}
	writeSecs := time.Since(start).Seconds()
	commits := total.OK
	for _, c := range rs {
		c.stdin.Close()
	}
	for i, c := range rs {
		collect(c, fmt.Sprintf("reader %d", i+1))
	}
	readTxns := total.OK - commits
	hl := h.drain()
	h.cmd.Wait()
	var holdSecs float64
	if len(hl) > 0 {
		holdSecs, _ = strconv.ParseFloat(strings.TrimPrefix(hl[len(hl)-1], "RELEASED "), 64)
	}

	db, conn, err := open(ctx, path, defaultOpts())
	if err != nil {
		return err
	}
	defer db.Close()
	defer conn.Close()
	var rows int
	conn.QueryRowContext(ctx, "SELECT count(*) FROM receipts").Scan(&rows)
	if res := integrity(ctx, conn); res != "ok" {
		r.failf("integrity_check: %s", res)
	}

	slices.Sort(total.LatencyUS)
	pct := func(p int) float64 {
		if len(total.LatencyUS) == 0 {
			return 0
		}
		return float64(total.LatencyUS[min(len(total.LatencyUS)-1, len(total.LatencyUS)*p/100)]) / 1000
	}
	r.Metrics["commits"] = commits
	r.Metrics["expected_commits"] = writers * txnsPerWriter
	r.Metrics["rows_in_db"] = rows
	r.Metrics["reader_txns"] = readTxns
	r.Metrics["busy_errors"] = total.Busy
	r.Metrics["other_errors"] = total.Other
	r.Metrics["reader_invariant_violations"] = total.Bad
	r.Metrics["first_error"] = total.FirstErr
	r.Metrics["lock_held_s"] = holdSecs
	r.Metrics["wall_s"] = writeSecs
	r.Metrics["commits_per_s_incl_hold"] = float64(commits) / writeSecs
	if writeSecs > holdSecs {
		r.Metrics["commits_per_s_after_release"] = float64(commits) / (writeSecs - holdSecs)
	}
	r.Metrics["commit_p50_ms"] = pct(50)
	r.Metrics["commit_p99_ms"] = pct(99)
	r.Metrics["commit_max_ms"] = pct(100)
	r.Metrics["busy_timeout_ms"] = o.busyMS
	if total.Busy > 0 || total.Other > 0 {
		r.failf("%d BUSY/LOCKED and %d other errors surfaced; first: %s", total.Busy, total.Other, total.FirstErr)
	}
	if total.Bad > 0 {
		r.failf("%d reader invariant violations", total.Bad)
	}
	if rows != commits || commits != writers*txnsPerWriter {
		r.failf("commits %d, rows %d, expected %d", commits, rows, writers*txnsPerWriter)
	}
	return nil
}

// holder takes the write lock, prints LOCKED, holds it, commits, and prints
// how long it held the lock.
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
	time.Sleep(w.hold)
	if _, err := conn.ExecContext(ctx, "COMMIT"); err != nil {
		return err
	}
	fmt.Printf("RELEASED %.3f\n", time.Since(t).Seconds())
	return nil
}

// contentionWriter commits n receipts, each with child rows, and reports
// per-commit latency measured from BEGIN IMMEDIATE to COMMIT returning.
func contentionWriter(ctx context.Context, w workerFlags) error {
	var rep workerReport
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		rep.record(err)
		emit(rep)
		return nil
	}
	defer db.Close()
	payload := make([]byte, 256)
	for seq := 1; seq <= w.n; seq++ {
		t := time.Now()
		err := func() error {
			if _, err := conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
				return err
			}
			id := receiptID(w.id, seq)
			if _, err := conn.ExecContext(ctx, "INSERT INTO receipts(id, worker, seq, nchild) VALUES (?,?,?,?)", id, w.id, seq, w.children); err != nil {
				return err
			}
			for c := range w.children {
				if _, err := conn.ExecContext(ctx, "INSERT INTO children(receipt_id, idx, payload) VALUES (?,?,?)", id, c, payload); err != nil {
					return err
				}
			}
			_, err := conn.ExecContext(ctx, "COMMIT")
			return err
		}()
		if err != nil {
			rep.record(err)
			conn.ExecContext(ctx, "ROLLBACK")
			continue
		}
		rep.OK++
		rep.LatencyUS = append(rep.LatencyUS, time.Since(t).Microseconds())
	}
	emit(rep)
	return nil
}

// contentionReader runs read transactions until stdin closes, checking that
// every receipt has exactly its declared number of children.
func contentionReader(ctx context.Context, w workerFlags) error {
	var rep workerReport
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		rep.record(err)
		emit(rep)
		return nil
	}
	defer db.Close()
	stop := untilEOF()
	for !stop() {
		err := func() error {
			if _, err := conn.ExecContext(ctx, "BEGIN"); err != nil {
				return err
			}
			defer conn.ExecContext(ctx, "COMMIT")
			var want, got int
			if err := conn.QueryRowContext(ctx, "SELECT coalesce(sum(nchild),0) FROM receipts").Scan(&want); err != nil {
				return err
			}
			if err := conn.QueryRowContext(ctx, "SELECT count(*) FROM children").Scan(&got); err != nil {
				return err
			}
			if want != got {
				rep.Bad++
			}
			return nil
		}()
		if err != nil {
			rep.record(err)
			continue
		}
		rep.OK++
		time.Sleep(time.Millisecond)
	}
	emit(rep)
	return nil
}
