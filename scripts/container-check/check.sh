#!/usr/bin/env bash
# Runs inside the container: each harness through spawn, wait, resume, cancel
# (during a shell command), wait, close; then a leftover-process check.
set -u
umask 077
cd ~/work
step() { printf '\n===== %s\n' "$*"; }
run() { printf '$ %s\n' "$*"; "$@"; echo "[exit $?]"; }

step versions
via --version
for b in claude codex opencode pi; do printf '%s: ' "$b"; "$b" --version 2>&1 | head -1; done

step models
run via models

declare -A MODEL=([claude]=haiku [codex]=gpt-6-luna [opencode]=opencode/mimo-v2.6-flash-free [pi]=openai/gpt-6-luna)
for h in ${HARNESSES:-claude codex opencode pi}; do
  step "live: $h (${MODEL[$h]})"
  mkdir -p "$h" && cd "$h" && printf 'alpha\nbeta\n' > notes.txt
  via spawn --harness "$h" --model "${MODEL[$h]}" --bound full --network \
    --prompt "Read notes.txt and reply with its two words in reverse order, nothing else." \
    --background --json > receipt.json || { echo "[spawn failed]"; cat receipt.json; cd ..; continue; }
  jq -r .handle receipt.json > handle
  S=$(jq -r .session_id receipt.json)
  run via wait "$(jq -r .turn receipt.json)" --timeout-ms 300000
  T=$(via resume "$S" --handle-file handle --prompt "Now say only: done" --json | jq -r .turn)
  run via wait "$T" --timeout-ms 300000
  T=$(via resume "$S" --handle-file handle --json \
    --prompt "Run the shell command 'sleep 30; echo late' and then say finished." | jq -r .turn)
  sleep 8
  run via cancel "$S" --handle-file handle
  run via wait "$T" --timeout-ms 120000
  run via close "$S" --handle-file handle
  cd ..
done

step "stop and leftovers (expect none below the header)"
run via daemon stop
sleep 6
ps -eo pid,etime,args | grep -vE 'ps -eo|grep|check.sh|bash$'
