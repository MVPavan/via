// Package harness runs multi-process SQLite WAL stress scenarios. One binary
// per driver calls Main; the binary is both orchestrator and worker (it
// re-executes itself with "worker <kind>").
package harness

import (
	"bufio"
	"context"
	"database/sql"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strings"
)

// DriverName is the database/sql driver name registered by the importing binary.
var DriverName string

// connOpts are the per-connection settings. Every connection in every process
// applies them explicitly, because PRAGMAs are per connection.
type connOpts struct {
	journal    string // WAL in the real configuration
	sync       string // FULL or NORMAL (OFF only in a negative control)
	busyMS     int    // 10000 in the real configuration
	noAutoCkpt bool   // negative control: disable automatic checkpoints
}

func defaultOpts() connOpts { return connOpts{journal: "WAL", sync: "FULL", busyMS: 10000} }

// open returns a pool limited to one connection plus that dedicated connection,
// so BEGIN/COMMIT and PRAGMAs all run on the same SQLite connection.
func open(ctx context.Context, path string, o connOpts) (*sql.DB, *sql.Conn, error) {
	db, err := sql.Open(DriverName, path)
	if err != nil {
		return nil, nil, err
	}
	db.SetMaxOpenConns(1)
	conn, err := db.Conn(ctx)
	if err != nil {
		db.Close()
		return nil, nil, err
	}
	fail := func(err error) (*sql.DB, *sql.Conn, error) {
		conn.Close()
		db.Close()
		return nil, nil, err
	}
	// busy_timeout first: switching journal_mode itself may need a lock.
	if _, err := conn.ExecContext(ctx, fmt.Sprintf("PRAGMA busy_timeout=%d", o.busyMS)); err != nil {
		return fail(fmt.Errorf("busy_timeout: %w", err))
	}
	var mode string
	if err := conn.QueryRowContext(ctx, "PRAGMA journal_mode="+o.journal).Scan(&mode); err != nil {
		return fail(fmt.Errorf("journal_mode: %w", err))
	}
	if !strings.EqualFold(mode, o.journal) {
		return fail(fmt.Errorf("journal_mode: asked %s, got %s", o.journal, mode))
	}
	for _, p := range []string{"PRAGMA synchronous=" + o.sync, "PRAGMA foreign_keys=ON"} {
		if _, err := conn.ExecContext(ctx, p); err != nil {
			return fail(fmt.Errorf("%s: %w", p, err))
		}
	}
	if o.noAutoCkpt {
		if _, err := conn.ExecContext(ctx, "PRAGMA wal_autocheckpoint=0"); err != nil {
			return fail(err)
		}
	}
	return db, conn, nil
}

const schema = `
CREATE TABLE IF NOT EXISTS receipts (id INTEGER PRIMARY KEY, worker INT NOT NULL, seq INT NOT NULL, nchild INT NOT NULL);
CREATE TABLE IF NOT EXISTS children (id INTEGER PRIMARY KEY, receipt_id INT NOT NULL REFERENCES receipts(id), idx INT NOT NULL, payload BLOB);
CREATE INDEX IF NOT EXISTS children_receipt ON children(receipt_id);
CREATE TABLE IF NOT EXISTS blobs (worker INT NOT NULL, row INT NOT NULL, sha TEXT NOT NULL, data BLOB NOT NULL, PRIMARY KEY (worker, row));
CREATE TABLE IF NOT EXISTS accounts (id INTEGER PRIMARY KEY, balance INT NOT NULL);
CREATE TABLE IF NOT EXISTS pad (id INTEGER PRIMARY KEY, data BLOB);
`

// initDB creates a fresh database file (removing old files) with the schema.
func initDB(ctx context.Context, path string, o connOpts) error {
	for _, suf := range []string{"", "-wal", "-shm", "-journal"} {
		os.Remove(path + suf)
	}
	db, conn, err := open(ctx, path, o)
	if err != nil {
		return err
	}
	defer db.Close()
	defer conn.Close()
	for stmt := range strings.SplitSeq(schema, ";") {
		if strings.TrimSpace(stmt) == "" {
			continue
		}
		if _, err := conn.ExecContext(ctx, stmt); err != nil {
			return err
		}
	}
	return nil
}

func integrity(ctx context.Context, conn *sql.Conn) string {
	var res string
	if err := conn.QueryRowContext(ctx, "PRAGMA integrity_check").Scan(&res); err != nil {
		return "error: " + err.Error()
	}
	return res
}

// isBusy reports whether an error is SQLITE_BUSY or SQLITE_LOCKED. The three
// drivers word these differently; all include "locked" or "busy".
func isBusy(err error) bool {
	s := strings.ToLower(err.Error())
	return strings.Contains(s, "busy") || strings.Contains(s, "locked")
}

// child is a worker process with line-oriented stdout.
type child struct {
	cmd   *exec.Cmd
	stdin io.WriteCloser
	lines chan string
}

func spawn(kind string, args ...string) (*child, error) {
	self, err := os.Executable()
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(self, append([]string{"worker", kind}, args...)...)
	cmd.Stderr = os.Stderr
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	out, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	c := &child{cmd: cmd, stdin: stdin, lines: make(chan string, 1024)}
	go func() {
		sc := bufio.NewScanner(out)
		sc.Buffer(make([]byte, 1<<20), 64<<20)
		for sc.Scan() {
			c.lines <- sc.Text()
		}
		close(c.lines)
	}()
	return c, nil
}

// drain returns all remaining stdout lines; it ends when the child closes stdout.
func (c *child) drain() []string {
	var out []string
	for l := range c.lines {
		out = append(out, l)
	}
	return out
}

// untilEOF returns a function reporting whether stdin has closed; the
// orchestrator closes a worker's stdin to ask it to stop.
func untilEOF() func() bool {
	done := make(chan struct{})
	go func() {
		io.Copy(io.Discard, os.Stdin)
		close(done)
	}()
	return func() bool {
		select {
		case <-done:
			return true
		default:
			return false
		}
	}
}
