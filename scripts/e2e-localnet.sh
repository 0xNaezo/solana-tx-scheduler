#!/usr/bin/env bash
# End-to-end on a local test validator with faucet SOL only:
# plan -> live run -> collect -> analyze -> verify --full, then check that
# every transaction we sent was found in a block.
#
# Needs solana, solana-keygen, solana-test-validator and jq on PATH. Uses a
# validator already listening on E2E_RPC, otherwise starts a throwaway one.
# E2E_OUT=<dir> keeps the measurement data (this is how data/localnet is made).
set -euo pipefail
cd "$(dirname "$0")/.."

RPC=${E2E_RPC:-http://127.0.0.1:8899}
if [ -z "${BIN:-}" ]; then
  cargo build --release --locked
  BIN=target/release/solana-tx-scheduler
fi
WORK=$(mktemp -d)
D="$WORK/data"
VALIDATOR=
cleanup() {
  [ -z "$VALIDATOR" ] || kill "$VALIDATOR" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

if ! solana -u "$RPC" cluster-version >/dev/null 2>&1; then
  # The default keeps 10,000 shreds, about 100 slots: blocks of the first
  # windows would be gone before collect and verify read them.
  solana-test-validator --ledger "$WORK/ledger" --reset --quiet \
    --limit-ledger-size 10000000 \
    --rpc-port 8899 --faucet-port 9900 >"$WORK/validator.log" 2>&1 &
  VALIDATOR=$!
  for _ in $(seq 60); do
    solana -u "$RPC" cluster-version >/dev/null 2>&1 && break
    sleep 1
  done
  # A few slots so the leader schedule and fees settle.
  sleep 5
fi

# Fresh wallet under the 0.1 SOL dedicated-wallet guard.
solana-keygen new --no-bip39-passphrase --silent --force -o "$WORK/id.json" >/dev/null
solana -u "$RPC" airdrop 0.09 "$(solana-keygen pubkey "$WORK/id.json")" >/dev/null

s() { "$BIN" --rpc-url "$RPC" --data-dir "$D" "$@"; }
fail() { echo "e2e: $*" >&2; exit 1; }
s plan --windows 24 --seed 7

# Spend cap: 120000 lamports fit one window (53850) but not a second one plus
# the sweep reserve, so the run must stop after exactly one.
s run --live --max-spend-lamports 120000 --keypair "$WORK/id.json"
fired=$(jq -s '[.[] | select(.live and .skipped == null)] | length' "$D/raw/windows.jsonl")
[ "$fired" -eq 1 ] || fail "spend cap did not stop the run after one window (fired $fired)"

# The rest of the quota; the ledger is cumulative across runs.
s run --live --keypair "$WORK/id.json"
sleep 5
s collect
s analyze --out "$WORK/report"
s verify --full

sent=$(jq -s '[.[] | select(.live and .skipped == null) | .txs[] | select(.sent_at_us != null and .send_error == null)] | length' "$D/raw/windows.jsonl")
located=$(grep -c '"index":[0-9]' "$D/raw/landed.jsonl" || true)
echo "e2e: sent $sent, located in blocks $located"
[ "$sent" -gt 0 ] && [ "$sent" -eq "$located" ] || fail "not every sent transaction was found in a block"
if [ -n "${E2E_OUT:-}" ]; then
  rm -rf "$E2E_OUT"
  mkdir -p "$E2E_OUT"
  cp -r "$D/raw" "$D/blocks" "$E2E_OUT/"
  # The localnet snapshot is a 5 MB leader schedule of one node: not kept.
  jq '.snapshot_dir = "(not kept: localnet leader schedule, 5 MB)"' "$D/plan.json" >"$E2E_OUT/plan.json"
  echo "e2e: data kept in $E2E_OUT"
fi

# The end-of-experiment wallet commands: anchor a file, return the payers' lamports.
s anchor --file PREREGISTRATION.md --label e2e --live --keypair "$WORK/id.json"
grep -q '"signature":"' "$D/raw/anchors.jsonl" || fail "anchor not recorded"
s sweep --live --keypair "$WORK/id.json"
s sweep --keypair "$WORK/id.json" | grep -q "would sweep 0 lamports" || fail "derived payers not empty after sweep"
echo "e2e: OK"
