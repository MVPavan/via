package harness

import (
	"context"
	"encoding/json"
	"fmt"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
)

// Scenario 4: a checkpointer runs PRAGMA wal_checkpoint(TRUNCATE) in a loop
// while writers move money between accounts and readers hold long read
// transactions. The total balance must never change inside a reader's
// snapshot, and the WAL must shrink to 0 bytes once readers finish.
//
// Pass requires: no torn reads; transfers and reader snapshots above stated
// minimums; at least minTruncations TRUNCATE checkpoints completed (busy=0)
// while writers and readers were running, so they overlapped; max WAL below
// walSanityBound; and a 0-byte WAL after the final TRUNCATE. Writer BUSY
// errors are allowed and reported (see below); every other error fails.
//
// Negative controls (-neg):
//
//	readtorn  readers sum one account per statement outside a transaction
//	          (torn reads must be seen); checkpoints run as normal
//	nockpt    no explicit or automatic checkpoints (no truncations, WAL
//	          stays large); readers as normal
const (
	minTransfers   = 500
	minSnapshots   = 5       // per reader; each snapshot lasts ~1 s
	minTruncations = 1       // proves overlap; the nockpt control gets 0
	walSanityBound = 1 << 30 // disk-safety bound, not evidence of checkpointing

	accounts        = 100
	startBalance    = 1000
	ckptWriters     = 4
	ckptReaders     = 4
	ckptRunDuration = 15 * time.Second
)

type ckptReport struct {
	Calls      int    `json:"calls"`
	BusyResult int    `json:"busy_result"` // checkpoint returned busy=1 (not an error)
	Errors     int    `json:"errors"`
	FirstErr   string `json:"first_err,omitempty"`
	FinalBusy  int    `json:"final_busy"`
	FinalWAL   int64  `json:"final_wal_bytes"`
}

func checkpointScenario(ctx context.Context, path, neg, sync string, r *Result) error {
	o := defaultOpts()
	o.sync = sync
	mode := "normal"
	switch neg {
	case "":
	case "readtorn", "nockpt":
		mode = neg
		o.noAutoCkpt = neg == "nockpt"
	default:
		return fmt.Errorf("unknown negative control %q", neg)
	}
	if err := initDB(ctx, path, o); err != nil {
		return err
	}
	if err := seedAccounts(ctx, path, o); err != nil {
		return err
	}
	w := workerFlags{db: path, mode: mode, opts: o}
	ck, err := spawn("checkpointer", w.args()...)
	if err != nil {
		return err
	}
	var ws, rs []*child
	for i := range ckptWriters {
		c, err := spawn("transfer", w.args("-id", strconv.Itoa(i+1))...)
		if err != nil {
			return err
		}
		ws = append(ws, c)
	}
	for i := range ckptReaders {
		c, err := spawn("longreader", w.args("-id", strconv.Itoa(i+1))...)
		if err != nil {
			return err
		}
		rs = append(rs, c)
	}
	// Sample the WAL file size while everything runs.
	var maxWAL, truncations atomic.Int64
	var running atomic.Bool
	running.Store(true)
	stopSampling := make(chan struct{})
	sampled := make(chan struct{})
	go func() {
		defer close(sampled)
		prev := int64(0)
		for {
			if fi, err := os.Stat(path + "-wal"); err == nil {
				maxWAL.Store(max(maxWAL.Load(), fi.Size()))
				if fi.Size() < prev && running.Load() {
					truncations.Add(1)
				}
				prev = fi.Size()
			}
			select {
			case <-stopSampling:
				return
			case <-time.After(20 * time.Millisecond):
			}
		}
	}()
	time.Sleep(ckptRunDuration)
	running.Store(false)
	runEnd := time.Now().UnixNano()

	var wr, rr workerReport
	collect := func(cs []*child, into *workerReport, who string) {
		for _, c := range cs {
			c.stdin.Close()
		}
		for i, c := range cs {
			lines := c.drain()
			if err := c.cmd.Wait(); err != nil {
				r.failf("%s %d exited: %v", who, i+1, err)
			}
			var rep workerReport
			if len(lines) == 0 || json.Unmarshal([]byte(lines[len(lines)-1]), &rep) != nil {
				r.failf("%s %d: no report", who, i+1)
				continue
			}
			into.OK += rep.OK
			into.Busy += rep.Busy
			into.Other += rep.Other
			into.Bad += rep.Bad
			if into.FirstErr == "" {
				into.FirstErr = rep.FirstErr
			}
		}
	}
	collect(ws, &wr, "writer")
	collect(rs, &rr, "reader")
	walBeforeFinal := int64(0)
	if fi, err := os.Stat(path + "-wal"); err == nil {
		walBeforeFinal = fi.Size()
	}
	// Readers and writers are gone; the checkpointer now runs a final
	// TRUNCATE and measures the WAL while its own connection is still open
	// (closing the last connection would delete the WAL and hide the result).
	ck.stdin.Close()
	cl := ck.drain()
	if err := ck.cmd.Wait(); err != nil {
		r.failf("checkpointer exited: %v", err)
	}
	close(stopSampling)
	<-sampled
	var cr ckptReport
	if len(cl) == 0 || json.Unmarshal([]byte(cl[len(cl)-1]), &cr) != nil {
		r.failf("checkpointer: no report")
	}
	// Completed TRUNCATEs that finished while writers and readers were
	// still running, and the largest WAL left right after one of them.
	overlapping, walAfterCkpt := 0, 0
	for _, l := range cl {
		f := strings.Fields(l)
		if len(f) != 4 || f[0] != "CKPT" {
			continue
		}
		if atoiMust(f[1]) < int(runEnd) && f[2] == "0" {
			overlapping++
			walAfterCkpt = max(walAfterCkpt, atoiMust(f[3]))
		}
	}

	db, conn, err := open(ctx, path, o)
	if err != nil {
		return err
	}
	defer db.Close()
	defer conn.Close()
	var total int
	if err := conn.QueryRowContext(ctx, "SELECT sum(balance) FROM accounts").Scan(&total); err != nil {
		return fmt.Errorf("final balance: %w", err)
	}
	if res := integrity(ctx, conn); res != "ok" {
		r.failf("integrity_check: %s", res)
	}

	r.Metrics["transfers"] = wr.OK
	r.Metrics["reader_snapshots"] = rr.OK
	r.Metrics["torn_reads"] = rr.Bad
	r.Metrics["writer_busy_errors"] = wr.Busy
	r.Metrics["writer_other_errors"] = wr.Other
	r.Metrics["reader_errors"] = rr.Busy + rr.Other
	r.Metrics["checkpoint_calls"] = cr.Calls
	r.Metrics["checkpoint_busy_results"] = cr.BusyResult
	r.Metrics["checkpoint_errors"] = cr.Errors
	r.Metrics["checkpoint_ok_calls"] = cr.Calls - cr.BusyResult
	r.Metrics["truncates_completed_during_run"] = overlapping
	r.Metrics["max_wal_bytes_right_after_truncate"] = walAfterCkpt
	r.Metrics["wal_shrinks_seen_by_20ms_sampler"] = truncations.Load()
	r.Metrics["max_wal_bytes"] = maxWAL.Load()
	r.Metrics["wal_bytes_when_readers_done"] = walBeforeFinal
	r.Metrics["final_wal_bytes"] = cr.FinalWAL
	r.Metrics["final_checkpoint_busy"] = cr.FinalBusy
	r.Metrics["final_total_balance"] = total
	if rr.Bad > 0 {
		r.failf("%d torn/inconsistent reads", rr.Bad)
	}
	// Writer BUSY errors are recorded, not failed: a TRUNCATE checkpoint
	// blocks new writers while it waits for long readers (documented SQLite
	// behaviour), so a writer can exhaust busy_timeout. The C control shows it too.
	r.Metrics["writer_first_error"] = wr.FirstErr
	if n := wr.Other + rr.Busy + rr.Other + cr.Errors; n > 0 {
		r.failf("%d reader/checkpointer/non-BUSY writer errors; first: %s%s%s", n, wr.FirstErr, rr.FirstErr, cr.FirstErr)
	}
	if wr.OK < minTransfers {
		r.failf("only %d transfers (minimum %d)", wr.OK, minTransfers)
	}
	if rr.OK < minSnapshots*ckptReaders {
		r.failf("only %d reader snapshots (minimum %d)", rr.OK, minSnapshots*ckptReaders)
	}
	if overlapping < minTruncations {
		r.failf("only %d TRUNCATE checkpoints completed while writers and readers ran (minimum %d)", overlapping, minTruncations)
	}
	if m := maxWAL.Load(); m > walSanityBound {
		r.failf("WAL reached %d bytes (bound %d)", m, walSanityBound)
	}
	if cr.FinalWAL != 0 || cr.FinalBusy != 0 {
		r.failf("WAL not truncated after readers finished: %d bytes, busy=%d", cr.FinalWAL, cr.FinalBusy)
	}
	if total != accounts*startBalance {
		r.failf("final total balance %d, want %d", total, accounts*startBalance)
	}
	return nil
}

func seedAccounts(ctx context.Context, path string, o connOpts) error {
	db, conn, err := open(ctx, path, o)
	if err != nil {
		return err
	}
	defer db.Close()
	defer conn.Close()
	if _, err := conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
		return err
	}
	for i := 1; i <= accounts; i++ {
		if _, err := conn.ExecContext(ctx, "INSERT INTO accounts(id, balance) VALUES (?,?)", i, startBalance); err != nil {
			return err
		}
	}
	_, err = conn.ExecContext(ctx, "COMMIT")
	return err
}

// transferWriter moves money between two random accounts per transaction and
// appends a 4 KB pad row so the WAL grows, until stdin closes.
func transferWriter(ctx context.Context, w workerFlags) error {
	var rep workerReport
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		rep.record(err)
		emit(rep)
		return nil
	}
	defer db.Close()
	pad := make([]byte, 4096)
	stop := untilEOF()
	for !stop() {
		a, b, amt := 1+rand.IntN(accounts), 1+rand.IntN(accounts), 1+rand.IntN(50)
		err := func() error {
			if _, err := conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
				return err
			}
			for _, q := range []struct {
				sql  string
				args []any
			}{
				{"UPDATE accounts SET balance = balance - ? WHERE id = ?", []any{amt, a}},
				{"UPDATE accounts SET balance = balance + ? WHERE id = ?", []any{amt, b}},
				{"INSERT INTO pad(data) VALUES (?)", []any{pad}},
			} {
				if _, err := conn.ExecContext(ctx, q.sql, q.args...); err != nil {
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
	}
	emit(rep)
	return nil
}

// longReader holds each read transaction for about 1 s, re-summing the
// balances 10 times inside it. Every sum must equal the starting total.
func longReader(ctx context.Context, w workerFlags) error {
	var rep workerReport
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		rep.record(err)
		emit(rep)
		return nil
	}
	defer db.Close()
	want := accounts * startBalance
	stop := untilEOF()
	for !stop() {
		err := func() error {
			if w.mode == "readtorn" {
				// One statement per account: each sees a different snapshot.
				sum := 0
				for id := 1; id <= accounts; id++ {
					var b int
					if err := conn.QueryRowContext(ctx, "SELECT balance FROM accounts WHERE id=?", id).Scan(&b); err != nil {
						return err
					}
					sum += b
				}
				if sum != want {
					rep.Bad++
				}
				return nil
			}
			if _, err := conn.ExecContext(ctx, "BEGIN"); err != nil {
				return err
			}
			defer conn.ExecContext(ctx, "COMMIT")
			for range 10 {
				var sum, n int
				if err := conn.QueryRowContext(ctx, "SELECT sum(balance), count(*) FROM accounts").Scan(&sum, &n); err != nil {
					return err
				}
				if sum != want || n != accounts {
					rep.Bad++
				}
				time.Sleep(100 * time.Millisecond)
			}
			return nil
		}()
		if err != nil {
			rep.record(err)
			continue
		}
		rep.OK++
	}
	emit(rep)
	return nil
}

// checkpointer runs wal_checkpoint(TRUNCATE) every 50 ms until stdin closes,
// then runs one final TRUNCATE and reports the WAL size.
func checkpointer(ctx context.Context, w workerFlags) error {
	var rep ckptReport
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		return err
	}
	defer db.Close()
	ckpt := func() int {
		var busy, logPages, done int
		if err := conn.QueryRowContext(ctx, "PRAGMA wal_checkpoint(TRUNCATE)").Scan(&busy, &logPages, &done); err != nil {
			rep.Errors++
			if rep.FirstErr == "" {
				rep.FirstErr = err.Error()
			}
			return -1
		}
		rep.Calls++
		rep.BusyResult += busy
		// "CKPT <unix-nanos when done> <busy> <wal bytes right after>"
		fmt.Printf("CKPT %d %d %d\n", time.Now().UnixNano(), busy, walSize(w.db))
		return busy
	}
	stop := untilEOF()
	for !stop() {
		if w.mode != "nockpt" {
			ckpt()
		}
		time.Sleep(50 * time.Millisecond)
	}
	if w.mode != "nockpt" {
		rep.FinalBusy = ckpt()
	}
	if fi, err := os.Stat(w.db + "-wal"); err == nil {
		rep.FinalWAL = fi.Size()
	}
	emit(rep)
	return nil
}
