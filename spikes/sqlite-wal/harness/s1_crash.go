package harness

import (
	"bytes"
	"context"
	"fmt"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"time"
)

// Scenario 1: kill -9 a writer 50 times; acked receipts must survive and no
// transaction may be partly visible.
//
// Negative controls (-neg):
//
//	ackearly   writer acks before COMMIT (the lost-receipt check must fire)
//	notx       writer inserts without a transaction (the partial check must fire)
//	syncoff    synchronous=OFF (expected to PASS: kill -9 keeps the OS page
//	           cache, so only power loss would expose it)
//	corrupt    after the kills, overwrite a page and re-run integrity_check
const kills = 50

func crashScenario(ctx context.Context, path, neg, sync string, r *Result) error {
	o := defaultOpts()
	o.sync = sync
	mode := "normal"
	switch neg {
	case "ackearly", "notx":
		mode = neg
	case "syncoff":
		o.sync = "OFF"
	case "", "corrupt":
	default:
		return fmt.Errorf("unknown negative control %q", neg)
	}
	if err := initDB(ctx, path, o); err != nil {
		return err
	}
	w := workerFlags{db: path, mode: mode, opts: o}
	var acked, inTxn, lost, partial, orphans, badIntegrity int
	for i := range kills {
		c, err := spawn("crash-writer", w.args("-id", strconv.Itoa(i+1))...)
		if err != nil {
			return err
		}
		// Wait for the first ack, then kill at a random point in later work.
		var seen []string
		timeout := time.After(30 * time.Second)
	wait:
		for {
			select {
			case l, ok := <-c.lines:
				if !ok {
					return fmt.Errorf("kill %d: writer exited early", i)
				}
				seen = append(seen, l)
				if strings.HasPrefix(l, "ACK") {
					break wait
				}
			case <-timeout:
				c.cmd.Process.Kill()
				return fmt.Errorf("kill %d: no ack within 30s", i)
			}
		}
		time.Sleep(time.Duration(rand.IntN(40_000)) * time.Microsecond)
		c.cmd.Process.Kill() // SIGKILL on Unix
		c.cmd.Wait()
		seen = append(seen, c.drain()...)
		if strings.HasPrefix(seen[len(seen)-1], "BEGIN") {
			inTxn++
		}
		var ackSeqs []int
		for _, l := range seen {
			if s, ok := strings.CutPrefix(l, "ACK "); ok {
				n, _ := strconv.Atoi(s)
				ackSeqs = append(ackSeqs, n)
			}
		}
		acked += len(ackSeqs)

		db, conn, err := open(ctx, path, o)
		if err != nil {
			return fmt.Errorf("kill %d: reopen: %w", i, err)
		}
		if res := integrity(ctx, conn); res != "ok" {
			badIntegrity++
			r.failf("kill %d: integrity_check: %s", i, res)
		}
		for _, seq := range ackSeqs {
			var n int
			if err := conn.QueryRowContext(ctx, "SELECT count(*) FROM receipts WHERE id=?", receiptID(i+1, seq)).Scan(&n); err != nil {
				return err
			}
			if n != 1 {
				lost++
				r.failf("kill %d: acked receipt seq %d missing", i, seq)
			}
		}
		var p, orph int
		conn.QueryRowContext(ctx, `SELECT count(*) FROM receipts r WHERE r.nchild != (SELECT count(*) FROM children c WHERE c.receipt_id = r.id)`).Scan(&p)
		conn.QueryRowContext(ctx, `SELECT count(*) FROM children c WHERE NOT EXISTS (SELECT 1 FROM receipts r WHERE r.id = c.receipt_id)`).Scan(&orph)
		if p > partial || orph > orphans {
			r.failf("kill %d: partial transactions visible: %d receipts with wrong child count, %d orphan children", i, p, orph)
		}
		partial, orphans = p, orph
		conn.Close()
		db.Close()
	}
	if neg == "corrupt" {
		res, err := corruptAndCheck(ctx, path, o)
		if err != nil {
			return err
		}
		r.Metrics["integrity_after_corruption"] = res[:min(len(res), 80)]
		if res != "ok" {
			r.failf("integrity_check after deliberate corruption: %s", res)
		}
	}
	r.Metrics["kills"] = kills
	r.Metrics["acked_receipts"] = acked
	r.Metrics["kills_landing_inside_txn"] = inTxn
	r.Metrics["lost_acked_receipts"] = lost
	r.Metrics["partial_receipts"] = partial
	r.Metrics["orphan_children"] = orphans
	r.Metrics["integrity_failures"] = badIntegrity
	r.Metrics["synchronous"] = o.sync
	return nil
}

func receiptID(worker, seq int) int { return worker*1_000_000 + seq }

// crashWriter loops forever writing one receipt plus N children per
// transaction. It prints "BEGIN seq" before and "ACK seq" after the commit.
func crashWriter(ctx context.Context, w workerFlags) error {
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		return err
	}
	defer db.Close()
	payload := make([]byte, 4096)
	for seq := 1; ; seq++ {
		fmt.Printf("BEGIN %d\n", seq)
		inTx := w.mode != "notx"
		if inTx {
			if _, err := conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
				return err
			}
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
		if w.mode == "ackearly" {
			fmt.Printf("ACK %d\n", seq)
		}
		if inTx {
			if _, err := conn.ExecContext(ctx, "COMMIT"); err != nil {
				return err
			}
		}
		if w.mode != "ackearly" {
			fmt.Printf("ACK %d\n", seq)
		}
	}
}

// corruptAndCheck proves integrity_check can fail: it checkpoints the run's
// disposable database, overwrites page 3 with junk, and re-runs the check.
func corruptAndCheck(ctx context.Context, path string, o connOpts) (string, error) {
	db, conn, err := open(ctx, path, o)
	if err != nil {
		return "", err
	}
	if _, err := conn.ExecContext(ctx, "PRAGMA wal_checkpoint(TRUNCATE)"); err != nil {
		return "", err
	}
	conn.Close()
	db.Close()
	if err := overwritePage(path, 3); err != nil {
		return "", err
	}
	db, conn, err = open(ctx, path, o)
	if err != nil {
		return "open failed: " + err.Error(), nil
	}
	defer db.Close()
	defer conn.Close()
	return integrity(ctx, conn), nil
}

func overwritePage(path string, page int64) error {
	f, err := os.OpenFile(path, os.O_WRONLY, 0)
	if err != nil {
		return err
	}
	junk := bytes.Repeat([]byte{0xA5}, 4096)
	if _, err := f.WriteAt(junk, (page-1)*4096); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}
