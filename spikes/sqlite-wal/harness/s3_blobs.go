package harness

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"math/rand/v2"
	"strconv"
	"strings"
)

// Scenario 3: several processes each commit 10 MB BLOB rows concurrently;
// every row read back must hash to what its writer hashed before inserting.
//
// Negative control (-neg flip): the writer flips one byte after hashing,
// so the sha256 check must fire.
const (
	blobWriters = 6
	blobRows    = 3
)

func blobScenario(ctx context.Context, path, neg, sync string, r *Result) error {
	o := defaultOpts()
	o.sync = sync
	if err := initDB(ctx, path, o); err != nil {
		return err
	}
	mode := "normal"
	switch neg {
	case "":
	case "flip":
		mode = "flip"
	default:
		return fmt.Errorf("unknown negative control %q", neg)
	}
	w := workerFlags{db: path, mode: mode, opts: o}
	var cs []*child
	for i := range blobWriters {
		c, err := spawn("blob", w.args("-id", strconv.Itoa(i+1), "-n", strconv.Itoa(blobRows))...)
		if err != nil {
			return err
		}
		cs = append(cs, c)
	}
	acked := map[string]string{} // "worker row" -> sha
	for i, c := range cs {
		for _, l := range c.drain() {
			f := strings.Fields(l)
			if len(f) == 4 && f[0] == "ACK" {
				acked[f[1]+" "+f[2]] = f[3]
			}
		}
		if err := c.cmd.Wait(); err != nil {
			r.failf("blob writer %d exited: %v", i+1, err)
		}
	}

	db, conn, err := open(ctx, path, o)
	if err != nil {
		return err
	}
	defer db.Close()
	defer conn.Close()
	rows, err := conn.QueryContext(ctx, "SELECT worker, row, sha, data FROM blobs")
	if err != nil {
		return err
	}
	var n, mismatches, bytesRead int
	for rows.Next() {
		var wk, row int
		var sha string
		var data []byte
		if err := rows.Scan(&wk, &row, &sha, &data); err != nil {
			return err
		}
		n++
		bytesRead += len(data)
		sum := sha256.Sum256(data)
		got := hex.EncodeToString(sum[:])
		key := fmt.Sprintf("%d %d", wk, row)
		if got != sha || got != acked[key] {
			mismatches++
			r.failf("row %s: read-back sha %s, stored %s, acked %s", key, got[:12], sha[:12], acked[key])
		}
	}
	if err := rows.Err(); err != nil {
		return err
	}
	if res := integrity(ctx, conn); res != "ok" {
		r.failf("integrity_check: %s", res)
	}
	want := blobWriters * blobRows
	if n != want || len(acked) != want {
		r.failf("rows %d, acked %d, expected %d", n, len(acked), want)
	}
	r.Metrics["writers"] = blobWriters
	r.Metrics["rows"] = n
	r.Metrics["acked"] = len(acked)
	r.Metrics["row_bytes"] = 10 << 20
	r.Metrics["bytes_read"] = bytesRead
	r.Metrics["sha_mismatches"] = mismatches
	return nil
}

// blobWriter commits n rows of 10 MB pseudo-random data and prints
// "ACK worker row sha256" after each COMMIT returns.
func blobWriter(ctx context.Context, w workerFlags) error {
	db, conn, err := open(ctx, w.db, w.opts)
	if err != nil {
		return err
	}
	defer db.Close()
	for row := 1; row <= w.n; row++ {
		data := make([]byte, w.size)
		rand.NewChaCha8([32]byte{byte(w.id), byte(row)}).Read(data)
		sum := sha256.Sum256(data)
		sha := hex.EncodeToString(sum[:])
		if w.mode == "flip" && row == 2 {
			data[len(data)/2] ^= 0xFF
		}
		if _, err := conn.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
			return err
		}
		if _, err := conn.ExecContext(ctx, "INSERT INTO blobs(worker, row, sha, data) VALUES (?,?,?,?)", w.id, row, sha, data); err != nil {
			return err
		}
		if _, err := conn.ExecContext(ctx, "COMMIT"); err != nil {
			return err
		}
		fmt.Printf("ACK %d %d %s\n", w.id, row, sha)
	}
	return nil
}
