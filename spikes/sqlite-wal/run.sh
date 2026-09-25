#!/bin/sh
# Runs every scenario for every driver: negative controls first (1 run each),
# then the real configuration (3 runs, synchronous=FULL), then scenario 2 once
# with synchronous=NORMAL. JSON result lines go to stdout. Every invocation is
# checked for its exit status, its number of result lines, and (for controls)
# the specific failure it must produce. Exits 1 on any surprise.
# Usage: sh run.sh DATA_DIR > results.jsonl   (DATA_DIR on a real disk, not tmpfs)
set -u
dir=${1:?usage: sh run.sh DATA_DIR}
here=$(dirname "$0")
mkdir -p "$dir"
df -T "$dir" | tail -1 >&2
surprises=0 lines=0

surprise() {
	echo "UNEXPECTED: $*" >&2
	surprises=$((surprises + 1))
}

# check DRIVER WANT_EXIT WANT_LINES WANT_PASS REASON ARGS...
# Runs the driver binary and requires: exit status WANT_EXIT, exactly
# WANT_LINES result lines, all with "pass":WANT_PASS, no harness error, and
# (when REASON is not "-") REASON in every line's failures.
check() {
	d=$1 want_exit=$2 want_lines=$3 want_pass=$4 reason=$5
	shift 5
	out=$("$here/bin/sqlitewal-$d" -dir "$dir" "$@")
	status=$?
	printf '%s\n' "$out"
	n=$(printf '%s\n' "$out" | grep -c '"scenario"')
	lines=$((lines + n))
	[ "$status" -eq "$want_exit" ] || surprise "$d $*: exit $status, want $want_exit"
	[ "$n" -eq "$want_lines" ] || surprise "$d $*: $n result lines, want $want_lines"
	ok=$(printf '%s\n' "$out" | grep -c "\"pass\":$want_pass")
	[ "$ok" -eq "$n" ] || surprise "$d $*: $ok of $n lines have pass=$want_pass"
	if printf '%s\n' "$out" | grep -q 'harness error'; then
		surprise "$d $*: harness error"
	fi
	if [ "$reason" != "-" ]; then
		hit=$(printf '%s\n' "$out" | grep -c -F "$reason")
		[ "$hit" -eq "$n" ] || surprise "$d $*: expected failure '$reason' missing"
	fi
}

for d in modernc ncruces mattn; do
	# Controls must FAIL (exit 1), each for its own reason.
	check "$d" 1 1 false "acked receipts missing" -scenario 1 -runs 1 -neg ackearly
	check "$d" 1 1 false "unexpected receipts" -scenario 1 -runs 1 -neg notx
	check "$d" 1 1 false "after deliberate corruption: integrity_check" -scenario 1 -runs 1 -neg corrupt
	check "$d" 1 1 false "BUSY/LOCKED" -scenario 2 -runs 1 -neg busy0
	check "$d" 1 1 false "read-back sha" -scenario 3 -runs 1 -neg flip
	check "$d" 1 1 false "torn/inconsistent reads" -scenario 4 -runs 1 -neg readtorn
	check "$d" 1 1 false "TRUNCATE checkpoints completed inside" -scenario 4 -runs 1 -neg nockpt
	check "$d" 1 1 false "BUSY/LOCKED" -scenario 6 -runs 1 -neg busy0
	check "$d" 1 1 false "sha256 mismatches" -scenario 6 -runs 1 -neg flip
	# synchronous=OFF must PASS: kill -9 keeps the OS page cache.
	check "$d" 0 1 true - -scenario 1 -runs 1 -neg syncoff
	# Real configuration: scenarios 1, 2, 3, 4, 6 x 3 runs.
	check "$d" 0 15 true - -runs 3
	check "$d" 0 1 true - -scenario 2 -runs 1 -sync NORMAL
done
# 3 drivers x (9 controls + syncoff + 15 real + 1 NORMAL) = 78 lines.
[ "$lines" -eq 78 ] || surprise "$lines result lines in total, want 78"
echo "result lines: $lines; surprises: $surprises" >&2
[ "$surprises" -eq 0 ]
