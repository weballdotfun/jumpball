#!/usr/bin/env bash
# Starts a local validator with the built program, runs the end-to-end test, stops the validator.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"
KEY=$(mktemp -t jumpball-program.XXXX).json
solana-keygen new --no-bip39-passphrase --silent --force -o "$KEY" >/dev/null
export PROGRAM_ID=$(solana-keygen pubkey "$KEY")
LEDGER=$(mktemp -d -t jumpball-ledger.XXXX)
# Its own ports, so it never touches another local validator.
export RPC_URL=http://127.0.0.1:18899
solana-test-validator --reset --quiet --ledger "$LEDGER" --rpc-port 18899 --faucet-port 19900 --gossip-port 18001 --dynamic-port-range 18002-18030 \
  --bpf-program "$PROGRAM_ID" target/deploy/jumpball.so >/dev/null 2>&1 &
VAL=$!
trap 'kill $VAL 2>/dev/null; rm -rf "$LEDGER" "$KEY"' EXIT
for _ in $(seq 1 60); do solana -u "$RPC_URL" cluster-version >/dev/null 2>&1 && break; sleep 1; done
# Wait until slots are advancing (a fresh validator can pause before producing blocks).
S0=$(solana -u "$RPC_URL" slot); for _ in $(seq 1 60); do [ "$(solana -u "$RPC_URL" slot)" -gt "$((S0 + 5))" ] && break; sleep 1; done
npx tsx tests/localnet.ts
