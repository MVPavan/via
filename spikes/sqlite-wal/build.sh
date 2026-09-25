#!/bin/sh
# Scenario 5: build every driver binary. Pure-Go drivers cross-build with
# CGO_ENABLED=0; the cgo control builds for the host only. For each Linux
# artifact, check it has no PT_INTERP segment (no dynamic loader) and that
# ldd calls it static; print sha256 attestations for every artifact.
set -eu
cd "$(dirname "$0")"
mkdir -p bin
for d in modernc ncruces; do
	for target in linux/amd64 linux/arm64 darwin/arm64; do
		GOOS=${target%/*} GOARCH=${target#*/} CGO_ENABLED=0 \
			go build -trimpath -o "bin/sqlitewal-$d-${target%/*}-${target#*/}" "./cmd/sqlitewal-$d"
	done
done
CGO_ENABLED=0 go build -o bin/ ./cmd/sqlitewal-modernc ./cmd/sqlitewal-ncruces
CGO_ENABLED=1 go build -o bin/ ./cmd/sqlitewal-mattn
status=0
for f in bin/*-linux-*; do
	if readelf -lW "$f" | grep -q INTERP; then
		echo "FAIL $f has a PT_INTERP segment"; status=1
	elif ! ldd "$f" 2>&1 | grep -q "not a dynamic executable"; then
		echo "FAIL $f: ldd does not report static"; status=1
	else
		echo "static $f (no PT_INTERP; ldd: not a dynamic executable)"
	fi
done
readelf -lW bin/sqlitewal-mattn | grep -o "Requesting program interpreter: [^]]*" | sed 's/^/cgo control: /'
go version
sha256sum bin/*-linux-* bin/*-darwin-*
exit $status
