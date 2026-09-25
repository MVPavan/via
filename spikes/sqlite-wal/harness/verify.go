package harness

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
)

// manifest is what the orchestrator expects to find. It is written to a file
// and checked by a freshly spawned verifier process, so recovery and
// read-back never reuse a process that took part in the writes.
type manifest struct {
	// Present maps receipt id to the sha256 its writer computed before the
	// insert ("" when the receipt has no raw-log payload).
	Present map[int]string `json:"present"`
	Absent  []int          `json:"absent,omitempty"` // receipts that must not exist
	Exact   bool           `json:"exact"`            // no receipts beyond Present
}

type verifyReport struct {
	Integrity   string `json:"integrity"`
	Receipts    int    `json:"receipts"`
	Missing     int    `json:"missing"`
	Unexpected  int    `json:"unexpected"` // Absent ids found, or extras when Exact
	Partial     int    `json:"partial"`    // receipts whose child count is wrong
	Orphans     int    `json:"orphans"`    // children without a receipt
	LogsChecked int    `json:"logs_checked"`
	ShaMismatch int    `json:"sha_mismatch"`
	Busy        int    `json:"busy"`
	Err         string `json:"err,omitempty"`

	integrityFailures int // summed by accumulate; not reported by the verifier
}

func (v verifyReport) problems() []string {
	var p []string
	if v.Err != "" {
		p = append(p, "verifier error: "+v.Err)
	}
	if v.Integrity != "ok" {
		p = append(p, "integrity_check: "+v.Integrity)
	}
	for _, c := range []struct {
		n    int
		what string
	}{{v.Missing, "acked receipts missing"}, {v.Unexpected, "unexpected receipts"},
		{v.Partial, "partial transactions"}, {v.Orphans, "orphan children"},
		{v.ShaMismatch, "sha256 mismatches"}, {v.Busy, "BUSY/LOCKED in verifier"}} {
		if c.n > 0 {
			p = append(p, fmt.Sprintf("%d %s", c.n, c.what))
		}
	}
	return p
}

// runVerifier writes the manifest next to the database, runs a verifier
// process, and returns its report.
func runVerifier(path string, o connOpts, m manifest) (verifyReport, error) {
	var rep verifyReport
	mf := path + ".manifest.json"
	b, err := json.Marshal(m)
	if err != nil {
		return rep, err
	}
	if err := os.WriteFile(mf, b, 0o644); err != nil {
		return rep, err
	}
	defer os.Remove(mf)
	w := workerFlags{db: path, mode: mf, opts: o}
	c, err := spawn("verify", w.args()...)
	if err != nil {
		return rep, err
	}
	c.stdin.Close()
	lines := c.drain()
	werr := c.cmd.Wait()
	if len(lines) == 0 || json.Unmarshal([]byte(lines[len(lines)-1]), &rep) != nil {
		return rep, fmt.Errorf("verifier gave no report (exit: %v)", werr)
	}
	if werr != nil {
		return rep, fmt.Errorf("verifier exited: %v", werr)
	}
	return rep, nil
}

// verifier is the worker side of runVerifier; w.mode carries the manifest path.
func verifier(ctx context.Context, w workerFlags) error {
	var rep verifyReport
	defer func() { emit(rep) }()
	fail := func(err error) error {
		if isBusy(err) {
			rep.Busy++
		}
		rep.Err = err.Error()
		return nil
	}
	b, err := os.ReadFile(w.mode)
	if err != nil {
		return fail(err)
	}
	var m manifest
	if err := json.Unmarshal(b, &m); err != nil {
		return fail(err)
	}
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		return fail(err)
	}
	defer db.Close()
	defer conn.Close()
	rep.Integrity = integrity(ctx, conn)
	if err := conn.QueryRowContext(ctx, "SELECT count(*) FROM receipts").Scan(&rep.Receipts); err != nil {
		return fail(err)
	}
	if err := conn.QueryRowContext(ctx, `SELECT count(*) FROM receipts r WHERE r.nchild != (SELECT count(*) FROM children c WHERE c.receipt_id = r.id)`).Scan(&rep.Partial); err != nil {
		return fail(err)
	}
	if err := conn.QueryRowContext(ctx, `SELECT count(*) FROM children c WHERE NOT EXISTS (SELECT 1 FROM receipts r WHERE r.id = c.receipt_id)`).Scan(&rep.Orphans); err != nil {
		return fail(err)
	}
	exists := func(id int) (bool, error) {
		var n int
		err := conn.QueryRowContext(ctx, "SELECT count(*) FROM receipts WHERE id=?", id).Scan(&n)
		return n == 1, err
	}
	for id, sha := range m.Present {
		ok, err := exists(id)
		if err != nil {
			return fail(err)
		}
		if !ok {
			rep.Missing++
			continue
		}
		if sha == "" {
			continue
		}
		var data []byte
		if err := conn.QueryRowContext(ctx, "SELECT data FROM logs WHERE receipt_id=?", id).Scan(&data); err != nil {
			return fail(fmt.Errorf("log %d: %w", id, err))
		}
		rep.LogsChecked++
		sum := sha256.Sum256(data)
		if hex.EncodeToString(sum[:]) != sha {
			rep.ShaMismatch++
		}
	}
	for _, id := range m.Absent {
		ok, err := exists(id)
		if err != nil {
			return fail(err)
		}
		if ok {
			rep.Unexpected++
		}
	}
	if m.Exact && rep.Receipts != len(m.Present) {
		rep.Unexpected += max(0, rep.Receipts-len(m.Present))
	}
	return nil
}
