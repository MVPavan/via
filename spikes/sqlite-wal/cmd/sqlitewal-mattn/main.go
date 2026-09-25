// Command sqlitewal-mattn is the control: github.com/mattn/go-sqlite3 wraps
// the real C SQLite through cgo, so it needs CGO_ENABLED=1 and a C compiler.
package main

import (
	_ "github.com/mattn/go-sqlite3"

	"github.com/MVPavan/via/spikes/sqlite-wal/harness"
)

func main() { harness.Main("sqlite3", "mattn", "github.com/mattn/go-sqlite3", "cgo") }
