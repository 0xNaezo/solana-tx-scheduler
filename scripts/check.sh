#!/bin/sh
# Everything a reviewer can check without SOL or keys:
#   1. the reports regenerate byte for byte from the committed raw data;
#   2. every calibration signature sits at its recorded slot and index;
#   3. the mainnet signatures do too: against the block archive if
#      ARCHIVE_URL (the release asset) is set, else 20 blocks re-fetched
#      from RPC_URL (default: the public mainnet endpoint);
#   4. the analysis code matches the hashes in PREREGISTRATION.md, and the
#      files anchored on-chain still hash to their memos.
# Needs cargo, or BIN pointing at a built binary. About 2 minutes plus the build.
set -eu
cd "$(dirname "$0")/.."

if [ -z "${BIN:-}" ]; then
  cargo build --release --locked
  BIN=target/release/solana-tx-scheduler
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0
check() {
  if "$@"; then echo "  ok"; else echo "  FAILED"; fail=1; fi
}
regenerated() {
  "$BIN" --data-dir "$1" analyze --out "$tmp/$2" >/dev/null
  for f in REPORT.md summary.json; do
    cmp -s "$tmp/$2/$f" "$2/$f" || { echo "  $2/$f differs"; return 1; }
  done
}

echo "1. Reports regenerate from the committed data"
check regenerated data report
check regenerated data/localnet report/localnet
check regenerated data/localnet2 report/localnet2

echo "2. Calibration runs against their block archives"
check "$BIN" --data-dir data/localnet verify --offline --full
check "$BIN" --data-dir data/localnet2 verify --offline --full

echo "3. Mainnet data"
if [ ! -s data/raw/landed.jsonl ]; then
  echo "  no mainnet data committed yet"
elif [ -n "${ARCHIVE_URL:-}" ]; then
  curl -fsSL "$ARCHIVE_URL" -o "$tmp/blocks.tar"
  tar -xf "$tmp/blocks.tar" -C data/blocks
  check "$BIN" verify --offline --full
else
  check "$BIN" ${RPC_URL:+--rpc-url "$RPC_URL"} verify --sample 20
fi

echo "4. On-chain anchors"
echo "  analysis code frozen in PREREGISTRATION.md:"
check sh -c "grep -E '^[0-9a-f]{64}  src/' PREREGISTRATION.md | sha256sum -c --quiet"
if [ -s data/raw/anchors.jsonl ]; then
  while IFS= read -r line; do
    field() { printf '%s' "$line" | sed -n "s/.*\"$1\":\"\\([^\"]*\\)\".*/\\1/p"; }
    file=$(field file)
    want=$(field sha256)
    sig=$(field signature)
    have=$(sha256sum "$file" 2>/dev/null | cut -d' ' -f1)
    if [ "$have" = "$want" ]; then
      echo "  ok  $file = memo in https://solscan.io/tx/$sig"
    else
      echo "  FAILED  $file does not hash to its memo ($want)"
      fail=1
    fi
  done <data/raw/anchors.jsonl
else
  echo "  no anchors committed yet"
fi

[ "$fail" = 0 ] && echo "all checks passed" || { echo "some checks FAILED"; exit 1; }
