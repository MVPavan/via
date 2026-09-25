#!/usr/bin/env bash
# Find which test file leaves unwanted state behind (test pollution).
# Runs test files one at a time with the repository's own runner and halts at
# the first file after which the pollution path exists.
#
# Run from the repository root — test files are discovered with `find .`
# against the current working directory:
#
#   <systematic-debugging-skill-dir>/scripts/find-polluter.sh <pollution_path> <test_glob> <test-command> [args...]
#   <systematic-debugging-skill-dir>/scripts/find-polluter.sh 'scratchpad/leftover.db' 'tests/**/test_*.py' uv run pytest
#
# The test command receives one test file path appended per run.
#
# Exit codes: 0 = no polluter found, 1 = polluter found (named in output),
#             2 = usage error, or pollution existed before any test ran.
#
# Adapted from superpowers' systematic-debugging skill; the runner is an
# argument here so the script is not tied to any one ecosystem.

set -euo pipefail

if [ $# -lt 3 ]; then
  echo "Usage: $0 <pollution_path> <test_glob> <test-command> [args...]" >&2
  echo "Example: $0 'scratchpad/leftover.db' 'tests/**/test_*.py' uv run pytest" >&2
  exit 2
fi

POLLUTION_CHECK="$1"
TEST_PATTERN="$2"
shift 2

# The pollution path must be a file the tests are suspected of *creating* —
# never repository metadata, a source root, or anything outside the repo.
case "$POLLUTION_CHECK" in
  .git|.git/*|/*|..*|.|./|"")
    echo "ERROR: refusing pollution path '$POLLUTION_CHECK' — must be a repo-relative artifact the tests create, never .git, a source root, or an absolute/parent path." >&2
    exit 2 ;;
esac

# A polluter is caught right after its own run (exit 1 below), so pollution can
# only exist here if it predates the script — any hit would blame the wrong file.
if [ -e "$POLLUTION_CHECK" ]; then
  echo "ERROR: $POLLUTION_CHECK already exists before any test ran." >&2
  echo "       Remove it and re-run so the polluter can be attributed." >&2
  exit 2
fi

echo "Searching for the test that creates: $POLLUTION_CHECK"
echo "Test pattern: $TEST_PATTERN"
echo "Runner: $*"
echo ""

# find emits ./-prefixed paths, so accept the pattern with or without ./ .
TEST_PATTERN="${TEST_PATTERN#./}"
# find -path can't match '**/' against zero directory levels (tests/**/test_x.py
# would skip tests/test_x.py), so also try the pattern with '**/' collapsed.
TEST_FILES=$(find . \( -path "./$TEST_PATTERN" -o -path "./${TEST_PATTERN//\*\*\//}" \) | sort -u)
if [ -z "$TEST_FILES" ]; then
  TOTAL=0
else
  TOTAL=$(printf '%s\n' "$TEST_FILES" | wc -l | tr -d ' ')
fi

echo "Found $TOTAL test files"
echo ""

COUNT=0
for TEST_FILE in $TEST_FILES; do
  COUNT=$((COUNT + 1))
  echo "[$COUNT/$TOTAL] Running: $TEST_FILE"
  "$@" "$TEST_FILE" > /dev/null 2>&1 || true

  if [ -e "$POLLUTION_CHECK" ]; then
    echo ""
    echo "FOUND POLLUTER: $TEST_FILE"
    echo "Created: $POLLUTION_CHECK"
    ls -la "$POLLUTION_CHECK"
    echo ""
    echo "To investigate, run just this file with the same runner and read the test."
    exit 1
  fi
done

echo ""
echo "No polluter found - all tested files clean."
exit 0
