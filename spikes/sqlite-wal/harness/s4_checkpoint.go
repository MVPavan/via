package harness

import (
	"context"
	"encoding/json"
	"fmt"
	"math/rand/v2"
	"os"
	"strconv"
	"sync/atomic"
	"time"
)

// Scenario 4: a checkpointer runs PRAGMA wal_checkpoint(TRUNCATE) in a loop
// while writers move money between accounts and readers hold long read
// transactions. The total balance must never change inside a reader's
// snapshot, and the WAL must shrink to 0 bytes once readers finish.
//
// Negative control (-neg torn): readers sum balances with one statement per
// account outside a transaction (torn reads must be seen), and automatic and
// explicit checkpoints are off (the WAL must stay large).
const (
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
	case "torn":
		mode = "torn"
		o.noAutoCkpt = true
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
	var maxWAL atomic.Int64
	stopSampling := make(chan struct{})
	sampled := make(chan struct{})
	go func() {
		defer close(sampled)
		for {
			if fi, err := os.Stat(path + "-wal"); err == nil {
				maxWAL.Store(max(maxWAL.Load(), fi.Size()))
			}
			select {
			case <-stopSampling:
				return
			case <-time.After(20 * time.Millisecond):
			}
		}
	}()
	time.Sleep(ckptRunDuration)

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

	db, conn, err := open(ctx, path, o)
	if err != nil {
		return err
	}
	defer db.Close()
	defer conn.Close()
	var total int
	conn.QueryRowContext(ctx, "SELECT sum(balance) FROM accounts").Scan(&total)
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
			if w.mode == "torn" {
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
		return busy
	}
	stop := untilEOF()
	for !stop() {
		if w.mode != "torn" {
			ckpt()
		}
		time.Sleep(50 * time.Millisecond)
	}
	if w.mode != "torn" {
		rep.FinalBusy = ckpt()
	}
	if fi, err := os.Stat(w.db + "-wal"); err == nil {
		rep.FinalWAL = fi.Size()
	}
	emit(rep)
	return nil
}
