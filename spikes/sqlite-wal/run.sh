#!/bin/sh
# Runs every scenario for every driver: negative controls first (1 run each),
# then the real configuration (3 runs, synchronous=FULL), then scenario 2 once
# with synchronous=NORMAL. Each JSON result line goes to stdout; the script
# checks every outcome against its expectation and exits 1 on any surprise.
# Usage: sh run.sh DATA_DIR > results.jsonl   (DATA_DIR on a real disk, not tmpfs)
set -u
dir=${1:?usage: sh run.sh DATA_DIR}
here=$(dirname "$0")
mkdir -p "$dir"
df -T "$dir" | tail -1 >&2
surprises=0

# expect WANT DRIVER ARGS...: run the driver binary, then require every
# result line to report "pass":WANT (true or false).
expect() {
	want=$1 d=$2
	shift 2
	out=$("$here/bin/sqlitewal-$d" -dir "$dir" "$@")
	printf '%s\n' "$out"
	n=$(printf '%s\n' "$out" | grep -c '"scenario"')
	ok=$(printf '%s\n' "$out" | grep -c "\"pass\":$want")
	if [ "$n" -eq 0 ] || [ "$n" -ne "$ok" ]; then
		echo "UNEXPECTED: $d $* (want pass=$want, $ok of $n matched)" >&2
		surprises=$((surprises + 1))
	fi
}

for d in modernc ncruces mattn; do
	# Controls that must FAIL: each proves a check can detect its fault.
	for c in "1 ackearly" "1 notx" "1 corrupt" "2 busy0" "3 flip" \
		"4 readtorn" "4 nockpt" "6 busy0" "6 flip"; do
		set -- $c
		expect false "$d" -scenario "$1" -runs 1 -neg "$2"
	done
	# synchronous=OFF must PASS: kill -9 keeps the OS page cache.
	expect true "$d" -scenario 1 -runs 1 -neg syncoff
	expect true "$d" -runs 3
	expect true "$d" -scenario 2 -runs 1 -sync NORMAL
done
echo "surprises: $surprises" >&2
[ "$surprises" -eq 0 ]
