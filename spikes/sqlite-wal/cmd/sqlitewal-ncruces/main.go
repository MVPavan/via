// Command sqlitewal-ncruces runs the WAL scenarios on github.com/ncruces/go-sqlite3
// (SQLite compiled to Wasm, then translated to Go; builds with CGO_ENABLED=0).
package main

import (
	"fmt"

	_ "github.com/ncruces/go-sqlite3/driver"
	"github.com/ncruces/go-sqlite3/vfs"

	"github.com/MVPavan/via/spikes/sqlite-wal/harness"
)

func main() {
	extra := fmt.Sprintf("SupportsFileLocking=%v SupportsSharedMemory=%v", vfs.SupportsFileLocking, vfs.SupportsSharedMemory)
	harness.Main("sqlite3", "ncruces", "github.com/ncruces/go-sqlite3", extra)
}
