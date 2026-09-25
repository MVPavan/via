#!/bin/sh
# Scenario 5: build every driver binary. Pure-Go drivers cross-build with
# CGO_ENABLED=0; the cgo control builds for the host only.
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
