// Command sqlitewal-modernc runs the WAL scenarios on modernc.org/sqlite
// (SQLite translated from C to Go; builds with CGO_ENABLED=0).
package main

import (
	"github.com/MVPavan/via/spikes/sqlite-wal/harness"
	_ "modernc.org/sqlite"
)

func main() { harness.Main("sqlite", "modernc", "modernc.org/sqlite", "") }
