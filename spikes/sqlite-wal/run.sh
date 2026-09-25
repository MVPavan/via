#!/bin/sh
# Runs every scenario for every driver: negative controls first (1 run each,
# expected to FAIL except s1 syncoff), then the real configuration (3 runs,
# synchronous=FULL), then scenario 2 once with synchronous=NORMAL.
# Usage: sh run.sh DATA_DIR   (DATA_DIR must be on a real disk, not tmpfs)
set -u
dir=${1:?usage: sh run.sh DATA_DIR}
here=$(dirname "$0")
mkdir -p "$dir"
df -T "$dir" | tail -1 >&2
for d in modernc ncruces mattn; do
	bin="$here/bin/sqlitewal-$d"
	for neg in "1 ackearly" "1 notx" "1 syncoff" "1 corrupt" "2 busy0" "3 flip" "4 torn"; do
		set -- $neg
		"$bin" -dir "$dir" -scenario "$1" -runs 1 -neg "$2"
	done
	"$bin" -dir "$dir" -runs 3
	"$bin" -dir "$dir" -scenario 2 -runs 1 -sync NORMAL
done
