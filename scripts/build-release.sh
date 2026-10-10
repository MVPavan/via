#!/usr/bin/env bash
# Build the static Linux release binary, verify it and write it to dist/.
# Usage (from the repository root): scripts/build-release.sh
set -euo pipefail

TARGET=x86_64-unknown-linux-musl
BIN="target/$TARGET/release/via"

die() { echo "build-release: $*" >&2; exit 1; }

[ -f Cargo.toml ] && [ -d crates/via-cli ] || die "run from the repository root"
rustup target list --installed | grep -qx "$TARGET" \
  || die "Rust target $TARGET missing: rustup target add $TARGET"
command -v musl-gcc >/dev/null \
  || die "musl-gcc missing: install musl-tools (Debian/Ubuntu)"

CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --locked -p via-cli --bin via --target "$TARGET"

# Static check: no program interpreter and file(1) agrees.
if readelf -lW "$BIN" | grep -q INTERP; then
  die "$BIN has a program interpreter (dynamically linked)"
fi
file -b "$BIN" | grep -Eq 'static(-pie)? linked' \
  || die "$BIN is not statically linked: $(file -b "$BIN")"

# Feature exclusion check needs the test-only fake agent.
cargo build --locked -p via-fake-agent
python3 scripts/check-release-features.py "$BIN"

VERSION=$(cargo metadata --format-version 1 --no-deps --locked \
  | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="via-cli"))')
REPORTED=$("$BIN" --version)
[ "$REPORTED" = "via $VERSION" ] \
  || die "version mismatch: binary says '$REPORTED', via-cli is $VERSION"

mkdir -p dist
NAME="via-$VERSION-$TARGET"
cp "$BIN" "dist/$NAME"
(cd dist && sha256sum "$NAME" > "$NAME.sha256")
echo "build-release: dist/$NAME ($REPORTED)"
cat "dist/$NAME.sha256"
