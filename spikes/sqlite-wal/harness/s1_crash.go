package harness

import (
	"bytes"
	"context"
	"fmt"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// Scenario 1: kill -9 a writer; acked receipts must survive, and no
// transaction may be partly visible.
//
// Gate kills (50): the writer commits 0-4 transactions, then starts one,
// writes the receipt and half its children, prints "INTXN" and stops. The
// kill lands there, so every gate kill is confirmed inside a transaction.
// The writer's page cache is tiny (cache_size=8 pages), so SQLite spills
// uncommitted pages into the WAL before the kill; recovery must ignore them.
//
// Random kills (50, supplementary): the kill lands 0-40 ms after the first
// ack, so it may hit a COMMIT in progress. Its position is reported, not
// assumed.
//
// After every kill, a newly spawned verifier process checks integrity,
// every acked receipt, the killed transaction's absence, and child counts.
//
// Negative controls (-neg):
//
//	ackearly   ack before COMMIT; gate kills pause after that ack (lost receipts)
//	notx       no transaction (partial receipts, killed receipt visible)
//	syncoff    synchronous=OFF (expected to PASS: kill -9 keeps the OS cache)
//	corrupt    after the kills, overwrite page 3; the verifier must fail
const (
	gateKills   = 50
	randomKills = 50
)

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
	var acked, confirmed, spilled, randomAfterBegin, sigkilled int
	var tally verifyReport
	allAcked := map[int]string{}
	for i := range gateKills + randomKills {
		worker := i + 1
		gate := i < gateKills
		pauseAt := 0
		if gate {
			pauseAt = 1 + rand.IntN(5)
		}
		c, err := spawn("crash-writer", w.args("-id", strconv.Itoa(worker), "-n", strconv.Itoa(pauseAt))...)
		if err != nil {
			return err
		}
		var seen []string
		timeout := time.After(30 * time.Second)
		want := "ACK"
		if gate {
			want = "INTXN"
		}
	wait:
		for {
			select {
			case l, ok := <-c.lines:
				if !ok {
					c.cmd.Wait()
					return fmt.Errorf("kill %d: writer exited early", worker)
				}
				seen = append(seen, l)
				if strings.HasPrefix(l, want) {
					break wait
				}
			case <-timeout:
				c.cmd.Process.Kill()
				c.cmd.Wait()
				return fmt.Errorf("kill %d: no %s within 30s", worker, want)
			}
		}
		if !gate {
			time.Sleep(time.Duration(rand.IntN(40_000)) * time.Microsecond)
		}
		c.cmd.Process.Kill() // SIGKILL on Unix
		c.cmd.Wait()
		if ws, ok := c.cmd.ProcessState.Sys().(syscall.WaitStatus); !ok || ws.Signal() != syscall.SIGKILL {
			r.failf("kill %d: writer did not die from SIGKILL: %v", worker, c.cmd.ProcessState)
		} else {
			sigkilled++
		}
		seen = append(seen, c.drain()...)

		var absent []int
		for _, l := range seen {
			f := strings.Fields(l)
			seq, err := strconv.Atoi(f[1])
			if err != nil {
				return fmt.Errorf("kill %d: bad writer line %q", worker, l)
			}
			switch f[0] {
			case "ACK":
				acked++
				allAcked[receiptID(worker, seq)] = ""
			case "INTXN":
				confirmed++
				absent = append(absent, receiptID(worker, seq))
				if len(f) == 4 && f[3] > f[2] {
					spilled++ // WAL grew inside the open transaction
				}
			}
		}
		if last := strings.Fields(seen[len(seen)-1])[0]; !gate && last == "BEGUN" {
			randomAfterBegin++
		}
		// In notx mode the "transaction" autocommits, so the killed receipt
		// is expected to be visible; the check must catch it.
		rep, err := runVerifier(path, o, manifest{Present: allAcked, Absent: absent})
		if err != nil {
			return fmt.Errorf("kill %d: %w", worker, err)
		}
		accumulate(&tally, rep)
		for _, p := range rep.problems() {
			r.failf("kill %d (%s): %s", worker, map[bool]string{true: "gate", false: "random"}[gate], p)
		}
	}
	if neg == "corrupt" {
		if err := corrupt(ctx, path, o); err != nil {
			return err
		}
		rep, err := runVerifier(path, o, manifest{Present: allAcked})
		if err != nil {
			return err
		}
		r.Metrics["integrity_after_corruption"] = rep.Integrity[:min(len(rep.Integrity), 80)]
		for _, p := range rep.problems() {
			r.failf("after deliberate corruption: %s", p)
		}
	}
	r.Metrics["writers_ended_by_sigkill"] = sigkilled
	r.Metrics["gate_kills"] = gateKills
	r.Metrics["gate_kills_confirmed_in_txn"] = confirmed
	r.Metrics["gate_kills_with_uncommitted_wal_frames"] = spilled
	r.Metrics["random_kills"] = randomKills
	r.Metrics["random_kills_after_begin_before_ack"] = randomAfterBegin
	r.Metrics["acked_receipts"] = acked
	r.Metrics["lost_acked_receipts"] = tally.Missing
	r.Metrics["killed_txn_visible"] = tally.Unexpected
	r.Metrics["partial_receipts_seen"] = tally.Partial
	r.Metrics["integrity_failures"] = tally.integrityFailures
	r.Metrics["synchronous"] = o.sync
	if neg == "" && confirmed != gateKills {
		r.failf("only %d of %d gate kills confirmed inside a transaction", confirmed, gateKills)
	}
	return nil
}

// accumulate sums verifier counts across kills.
func accumulate(t *verifyReport, v verifyReport) {
	t.Missing += v.Missing
	t.Unexpected += v.Unexpected
	t.Partial += v.Partial
	t.Orphans += v.Orphans
	if v.Integrity != "ok" {
		t.integrityFailures++
	}
}

func receiptID(worker, seq int) int { return worker*1_000_000 + seq }

func walSize(db string) int64 {
	fi, err := os.Stat(db + "-wal")
	if err != nil {
		return 0
	}
	return fi.Size()
}

// crashWriter loops writing one receipt plus N children per transaction. It
// prints "BEGUN seq" once BEGIN IMMEDIATE succeeds and "ACK seq" after COMMIT
// returns. With -n K it stops inside transaction K, after printing
// "INTXN K walBytesAtBegin walBytesNow", and waits to be killed.
func crashWriter(ctx context.Context, w workerFlags) error {
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		return err
	}
	defer db.Close()
	if _, err := conn.ExecContext(ctx, "PRAGMA cache_size=8"); err != nil {
		return err
	}
	payload := make([]byte, 4096)
	pause := func(seq int, walAtBegin int64) {
		fmt.Printf("INTXN %d %020d %020d\n", seq, walAtBegin, walSize(w.db))
		time.Sleep(time.Hour) // wait for SIGKILL (a bare select{} trips Go's deadlock detector)
		os.Exit(3)
	}
	for seq := 1; ; seq++ {
		inTx := w.mode != "notx"
		walAtBegin := walSize(w.db)
		if inTx {
			if _, err := conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
				return err
			}
		}
		fmt.Printf("BEGUN %d\n", seq)
		id := receiptID(w.id, seq)
		if _, err := conn.ExecContext(ctx, "INSERT INTO receipts(id, worker, seq, nchild) VALUES (?,?,?,?)", id, w.id, seq, w.children); err != nil {
			return err
		}
		for c := range w.children {
			if c == w.children/2 && seq == w.n && w.mode != "ackearly" {
				pause(seq, walAtBegin)
			}
			if _, err := conn.ExecContext(ctx, "INSERT INTO children(receipt_id, idx, payload) VALUES (?,?,?)", id, c, payload); err != nil {
				return err
			}
		}
		if w.mode == "ackearly" {
			fmt.Printf("ACK %d\n", seq)
			if seq == w.n {
				pause(seq, walAtBegin)
			}
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

// corrupt checkpoints the run's disposable database and overwrites page 3.
func corrupt(ctx context.Context, path string, o connOpts) error {
	db, conn, err := open(ctx, path, o)
	if err != nil {
		return err
	}
	if _, err := conn.ExecContext(ctx, "PRAGMA wal_checkpoint(TRUNCATE)"); err != nil {
		return err
	}
	conn.Close()
	db.Close()
	f, err := os.OpenFile(path, os.O_WRONLY, 0)
	if err != nil {
		return err
	}
	if _, err := f.WriteAt(bytes.Repeat([]byte{0xA5}, 4096), 2*4096); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}
