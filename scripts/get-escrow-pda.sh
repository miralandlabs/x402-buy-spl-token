#!/usr/bin/env bash
# get-escrow-pda.sh — print the SLA-Escrow PDA for `X402_PAY_TO` on the
# x402-buy-spl-token deployment (sla-escrow rail).
#
# Calls pr402's /api/v1/facilitator/sellers/{wallet}/rails/{scheme}?asset=<mint>
# which returns the escrow PDA derived from
#     find_program_address([b"escrow", mint, bank_pda], program_id)
# i.e. the canonical per-asset escrow account.
#
# Note: GET /api/v1/facilitator/sellers/{wallet}/preview ignores `asset` and
# always returns the SOL preview-mint escrow PDA. Do NOT use that for `payTo`.
#
# Usage:
#   ./scripts/get-escrow-pda.sh                            # devnet defaults
#   ./scripts/get-escrow-pda.sh --network mainnet
#   FACILITATOR_URL=https://preview.agent.pay402.me ./scripts/get-escrow-pda.sh
#
# Output (single line, base58): paste into Vercel env `X402_PAY_TO`.
set -euo pipefail

NETWORK="devnet"
FACILITATOR_URL="${FACILITATOR_URL:-https://preview.agent.pay402.me}"
WALLET="${WALLET:-BeALNhc8tykF6wJBZWyXGEkb9Mfvk8JZk8miUL2JDuhw}"   # any valid pubkey works; not used in derivation

while [[ $# -gt 0 ]]; do
  case "$1" in
    --network) NETWORK="$2"; shift 2 ;;
    --facilitator-url) FACILITATOR_URL="$2"; shift 2 ;;
    --wallet) WALLET="$2"; shift 2 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

case "$NETWORK" in
  devnet)  USDC=4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU ;;
  mainnet|mainnet-beta) USDC=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v ;;
  *) echo "unsupported --network $NETWORK (use devnet|mainnet)" >&2; exit 1 ;;
esac

URL="${FACILITATOR_URL%/}/api/v1/facilitator/sellers/${WALLET}/rails/sla-escrow?asset=${USDC}"

PDA=$(curl -fsS "$URL" | jq -r '.vaultPda // empty')

if [[ -z "$PDA" || "$PDA" == "null" ]]; then
  echo "ERROR: facilitator seller rail did not return vaultPda for asset=$USDC" >&2
  echo "  url: $URL" >&2
  curl -sS "$URL" | jq . >&2 || true
  exit 1
fi

echo "$PDA"
