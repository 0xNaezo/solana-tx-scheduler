#!/bin/sh
# Container entrypoint. With arguments: run that subcommand. Without: the
# whole experiment, plan -> run -> collect -> analyze -> verify.
#
# LIVE=1 writes data/ and report/ and needs an explicit MAX_SPEND_LAMPORTS
# (the binary also enforces its compiled-in hard cap). Anything else is a
# rehearsal in data/rehearsal/ (gitignored) that sends nothing and leaves the
# committed data/ and report/ alone, followed by a check that report/REPORT.md
# regenerates from the committed data.
set -eu

if [ -z "${RPC_URL:-}" ]; then
  if [ -n "${HELIUS_API:-}" ]; then
    RPC_URL="https://mainnet.helius-rpc.com/?api-key=${HELIUS_API}"
  else
    RPC_URL="https://api.mainnet-beta.solana.com"
  fi
fi
export RPC_URL
bin=solana-tx-scheduler

if [ $# -gt 0 ]; then
  exec "$bin" "$@"
fi

if [ "${LIVE:-0}" = 1 ]; then
  if [ -z "${MAX_SPEND_LAMPORTS:-}" ]; then
    echo "LIVE=1 needs MAX_SPEND_LAMPORTS (the compiled-in hard cap is 50000000)" >&2
    exit 2
  fi
  dir=data out=report
  set -- --live --max-spend-lamports "$MAX_SPEND_LAMPORTS"
  if [ -n "${MAX_WINDOWS:-}" ]; then set -- "$@" --max-windows "$MAX_WINDOWS"; fi
else
  # Real leader windows and QUIC handshakes; nothing is sent.
  dir=data/rehearsal out=data/rehearsal/report
  rm -rf "$dir"
  set -- --max-windows "${MAX_WINDOWS:-24}"
fi
if [ -n "${MAX_MINUTES:-}" ]; then set -- "$@" --max-minutes "$MAX_MINUTES"; fi

# As PID 1 the shell ignores signals it does not trap. Forward INT/TERM to the
# running step (`run` finishes the window in flight) and stop after it.
child=
interrupted=0
trap 'interrupted=1; [ -z "$child" ] || kill -TERM "$child" 2>/dev/null || true' INT TERM
step() {
  [ "$interrupted" = 0 ] || exit 130
  "$bin" "$@" --data-dir "$dir" &
  child=$!
  # `wait` returns early when a trapped signal arrives; wait again until the step exits.
  while :; do
    status=0
    wait "$child" || status=$?
    kill -0 "$child" 2>/dev/null || break
  done
  child=
  if [ "$interrupted" = 1 ]; then
    echo "interrupted during '$1'; start again to resume" >&2
    exit 130
  fi
  return "$status"
}

# Keep an existing plan so an interrupted live run resumes with the same design.
if [ ! -f "$dir/plan.json" ] || [ "${REPLAN:-0}" = 1 ]; then
  step plan
fi
step run "$@"
# Let the last windows reach confirmed commitment before reading them back.
sleep 20
step collect
step analyze --out "$out"
step verify --offline

if [ "${LIVE:-0}" = 1 ]; then
  cat <<'NEXT'
Live run finished. To close the experiment:
  docker compose run --rm scheduler anchor --file data/blocks/MANIFEST.sha256 --label blocks --live
  docker compose run --rm scheduler sweep --live
  docker compose run --rm scheduler analyze
then commit data/ and report/, and publish data/blocks/*.json.zst as a release asset.
NEXT
elif [ ! -s data/raw/windows.jsonl ]; then
  echo "No committed mainnet data yet: the check that report/REPORT.md regenerates was skipped."
else
  dir=data
  step analyze --out data/rehearsal/committed >/dev/null
  if cmp -s data/rehearsal/committed/REPORT.md report/REPORT.md; then
    echo "PASS: report/REPORT.md regenerates byte-for-byte from the committed data/"
  else
    echo "FAIL: report/REPORT.md differs from what the committed data/ gives"
    exit 1
  fi
  echo "On-chain check of every recorded signature: docker compose run --rm scheduler verify"
fi
