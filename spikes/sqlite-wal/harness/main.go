package harness

import (
	"context"
	"database/sql"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"runtime/debug"
	"time"
)

// workerFlags are shared by every worker kind; each kind reads what it needs.
type workerFlags struct {
	db, mode string
	opts     connOpts
	id, n    int
	children int
	size     int
	hold     time.Duration
}

func parseWorker(args []string) workerFlags {
	var w workerFlags
	fs := flag.NewFlagSet("worker", flag.ExitOnError)
	fs.StringVar(&w.db, "db", "", "database path")
	fs.StringVar(&w.mode, "mode", "normal", "normal or a negative-control mode")
	fs.StringVar(&w.opts.journal, "journal", "WAL", "journal_mode")
	fs.StringVar(&w.opts.sync, "sync", "FULL", "synchronous")
	fs.IntVar(&w.opts.busyMS, "busy", 10000, "busy_timeout in ms")
	fs.BoolVar(&w.opts.noAutoCkpt, "noautockpt", false, "set wal_autocheckpoint=0")
	fs.IntVar(&w.id, "id", 0, "worker id")
	fs.IntVar(&w.n, "n", 10, "transactions or rows")
	fs.IntVar(&w.children, "children", 20, "child rows per receipt")
	fs.IntVar(&w.size, "size", 10<<20, "blob size in bytes")
	fs.DurationVar(&w.hold, "hold", 5*time.Second, "lock hold time")
	fs.Parse(args)
	return w
}

// args renders the flags a child needs to reproduce the parent's settings.
func (w workerFlags) args(extra ...string) []string {
	a := []string{"-db", w.db, "-journal", w.opts.journal, "-sync", w.opts.sync,
		"-busy", fmt.Sprint(w.opts.busyMS), "-mode", w.mode}
	if w.opts.noAutoCkpt {
		a = append(a, "-noautockpt")
	}
	return append(a, extra...)
}

// Result is one scenario run; it is printed as one JSON line.
type Result struct {
	Driver   string         `json:"driver"`
	Scenario int            `json:"scenario"`
	Run      int            `json:"run"`
	Neg      string         `json:"neg,omitempty"`
	Sync     string         `json:"sync"`
	Pass     bool           `json:"pass"`
	Seconds  float64        `json:"seconds"`
	Metrics  map[string]any `json:"metrics"`
	Failures []string       `json:"failures,omitempty"`
}

// Main is the entry point for a driver binary. module is the driver's Go
// module path, reported with its version; extra is driver-specific detail.
func Main(driverName, label, module, extra string) {
	DriverName = driverName
	if len(os.Args) >= 3 && os.Args[1] == "worker" {
		if err := runWorker(os.Args[2], parseWorker(os.Args[3:])); err != nil {
			fmt.Fprintf(os.Stderr, "worker %s: %v\n", os.Args[2], err)
			os.Exit(1)
		}
		return
	}
	fs := flag.NewFlagSet("run", flag.ExitOnError)
	scenario := fs.Int("scenario", 0, "scenario 1-4 or 6 (5 is build.sh), or 0 for all")
	runs := fs.Int("runs", 3, "runs per scenario")
	dir := fs.String("dir", "", "directory for database files (must not be tmpfs)")
	neg := fs.String("neg", "", "negative control to run instead of the real configuration")
	sync := fs.String("sync", "FULL", "synchronous for the real configuration")
	fs.Parse(os.Args[1:])
	if *dir == "" {
		fmt.Fprintln(os.Stderr, "usage: -dir DIR [-scenario N] [-runs N] [-neg NAME] [-sync FULL|NORMAL]")
		os.Exit(2)
	}
	fmt.Fprintf(os.Stderr, "driver=%s %s@%s sqlite=%s %s\n", label, module, moduleVersion(module), sqliteVersion(), extra)
	scenarios := []int{1, 2, 3, 4, 6}
	if *scenario != 0 {
		scenarios = []int{*scenario}
	}
	exit := 0
	for _, s := range scenarios {
		for r := 1; r <= *runs; r++ {
			path, _ := filepath.Abs(filepath.Join(*dir, fmt.Sprintf("%s-s%d-r%d%s.sqlite", label, s, r, *neg)))
			res := Result{Driver: label, Scenario: s, Run: r, Neg: *neg, Sync: *sync, Metrics: map[string]any{}}
			start := time.Now()
			f, ok := scenarioFuncs[s]
			if !ok {
				fmt.Fprintf(os.Stderr, "unknown scenario %d\n", s)
				os.Exit(2)
			}
			err := f(context.Background(), path, *neg, *sync, &res)
			res.Seconds = time.Since(start).Seconds()
			if err != nil {
				res.Failures = append(res.Failures, "harness error: "+err.Error())
			}
			res.Pass = len(res.Failures) == 0
			if !res.Pass {
				exit = 1
			}
			line, _ := json.Marshal(res)
			fmt.Println(string(line))
			for _, suf := range []string{"", "-wal", "-shm", ".manifest.json"} {
				os.Remove(path + suf)
			}
		}
	}
	os.Exit(exit)
}

var scenarioFuncs = map[int]func(ctx context.Context, path, neg, sync string, r *Result) error{
	1: crashScenario,
	2: contentionScenario,
	3: blobScenario,
	4: checkpointScenario,
	6: combinedScenario,
}

func runWorker(kind string, w workerFlags) error {
	ctx := context.Background()
	switch kind {
	case "crash-writer":
		return crashWriter(ctx, w)
	case "holder":
		return holder(ctx, w)
	case "writer":
		return contentionWriter(ctx, w)
	case "reader":
		return contentionReader(ctx, w)
	case "blob":
		return blobWriter(ctx, w)
	case "transfer":
		return transferWriter(ctx, w)
	case "longreader":
		return longReader(ctx, w)
	case "checkpointer":
		return checkpointer(ctx, w)
	case "verify":
		return verifier(ctx, w)
	}
	return fmt.Errorf("unknown worker kind %q", kind)
}

// failf records a failure, keeping at most 20 so a broken run stays readable.
func (r *Result) failf(format string, a ...any) {
	if len(r.Failures) < 20 {
		msg := fmt.Sprintf(format, a...)
		r.Failures = append(r.Failures, msg[:min(len(msg), 300)])
	} else if len(r.Failures) == 20 {
		r.Failures = append(r.Failures, "... more failures omitted")
	}
}

func moduleVersion(module string) string {
	if bi, ok := debug.ReadBuildInfo(); ok {
		for _, d := range bi.Deps {
			if d.Path == module {
				return d.Version
			}
		}
	}
	return "unknown"
}

func sqliteVersion() string {
	db, err := sql.Open(DriverName, ":memory:")
	if err != nil {
		return err.Error()
	}
	defer db.Close()
	var v string
	if err := db.QueryRow("SELECT sqlite_version()").Scan(&v); err != nil {
		return err.Error()
	}
	return v
}

func emit(v any) {
	b, _ := json.Marshal(v)
	fmt.Println(string(b))
}
