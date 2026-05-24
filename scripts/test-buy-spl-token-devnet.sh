#!/usr/bin/env bash
#
# Buy SPL Token end-to-end devnet test (x402-buy-spl-token v0.3 buyer-commit).
#
# Drives a complete sla-escrow purchase against the preview deployment:
#
#   1. Probe the seller's `GET /api/v1/buy-spl-token` (no PAYMENT-SIGNATURE)
#      → expect HTTP 402 with accepts[0].extra.commitMaterial (session quote)
#        and extra.merchantWallet (FundPayment.seller / ReleasePayment payout).
#   2. Buyer composes TransferSla from commitMaterial session fields + a fresh
#      payment_uid, then POST {pr402-facilitator}/build-sla-escrow-payment-tx.
#   3. Sign the FundPayment locally with the buyer keypair (pr402 example
#      `e2e_sign_sla_escrow_tx`).
#   4. Re-issue the same GET with `PAYMENT-SIGNATURE: <body>`.
#   5. Expect HTTP 200 with `{ status: "completed", paymentUid, slaHash, ... }`.
#   6. Verify on-chain + evidence registry.
#   7. Replay step 4 (idempotency).
#   8. (Optional) Drive ReleasePayment once the oracle approves.
#
# Prerequisites:
#   - `solana` (1.18+), `curl`, `jq`, `python3`, `openssl`, `cargo`
#   - `cargo run --example e2e_sign_sla_escrow_tx` from the pr402 crate.
#   - Funded buyer keypair on devnet (SOL + USDC).
#   - Deployed seller with catalog, merchant wallet, seller signer, registry.
#
# Environment overrides (defaults in []):
#   SELLER_BASE_URL          [https://preview.spl-token.hashspace.me]
#   FACILITATOR_URL          [https://preview.agent.pay402.me]
#   PR402_ROOT               [<workspace>/pr402]
#   BUYER_KEYPAIR            [<workspace>/demo-wallets/buyer-keypair.json]
#   ORACLE_PUBKEY            []  auto-picked from facilitator /supported
#   TOKEN_MINT               [5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP]
#   USDC_MINT                [4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU]
#   QUANTITY                 [1]
#   RECIPIENT_OWNER          [buyer pubkey]
#   BUYER_NONCE              [random 64 lowercase hex chars]
#   RPC_URL                  [https://api.devnet.solana.com]
#   VERCEL_BYPASS_TOKEN      []
#   SKIP_REPLAY              [0]
#   SKIP_ONCHAIN_VERIFY      [0]
#   SKIP_RELEASE             [0]
#
# Exit codes:
#   0  end-to-end success.
#   1+ specific step failed.

set -euo pipefail

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_ROOT="$(cd "${CRATE_ROOT}/.." && pwd)"
DEMO_WALLETS_DEFAULT="${WORKSPACE_ROOT}/demo-wallets"

SELLER_BASE_URL="${SELLER_BASE_URL:-https://preview.spl-token.hashspace.me}"
FACILITATOR_URL="${FACILITATOR_URL:-https://preview.ipay.sh}"
PR402_ROOT="${PR402_ROOT:-${WORKSPACE_ROOT}/pr402}"

BUYER_KEYPAIR="${BUYER_KEYPAIR:-${DEMO_WALLETS_DEFAULT}/buyer-keypair.json}"
ORACLE_PUBKEY="${ORACLE_PUBKEY:-}"

TOKEN_MINT="${TOKEN_MINT:-5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP}"
USDC_MINT="${USDC_MINT:-4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU}"
QUANTITY="${QUANTITY:-1}"
RPC_URL="${RPC_URL:-https://api.devnet.solana.com}"
DEVNET_CHAIN_ID="solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1"

VERCEL_BYPASS_TOKEN="${VERCEL_BYPASS_TOKEN:-}"
SKIP_REPLAY="${SKIP_REPLAY:-0}"
SKIP_ONCHAIN_VERIFY="${SKIP_ONCHAIN_VERIFY:-0}"

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "❌ missing command: $1" >&2
        exit 1
    }
}
require_cmd curl
require_cmd jq
require_cmd solana
require_cmd python3
require_cmd openssl
require_cmd cargo

[[ -f "${BUYER_KEYPAIR}" ]] || {
    echo "❌ buyer keypair not found at ${BUYER_KEYPAIR}" >&2
    exit 1
}
[[ -d "${PR402_ROOT}" && -f "${PR402_ROOT}/Cargo.toml" ]] || {
    echo "❌ pr402 crate root not found at ${PR402_ROOT} (set PR402_ROOT)" >&2
    exit 1
}

BUYER_PUBKEY="$(solana address -k "${BUYER_KEYPAIR}")"
RECIPIENT_OWNER="${RECIPIENT_OWNER:-${BUYER_PUBKEY}}"
BUYER_NONCE="${BUYER_NONCE:-$(openssl rand -hex 32)}"

if [[ ! "${BUYER_NONCE}" =~ ^[0-9a-f]{64}$ ]]; then
    echo "❌ BUYER_NONCE is not exactly 64 lowercase hex chars: ${BUYER_NONCE}" >&2
    exit 1
fi

TMPDIR_RUN="$(mktemp -d -t buy-spl-token-e2e-XXXXXX)"
trap 'rm -rf "${TMPDIR_RUN}"' EXIT

attach_bypass() {
    local url="$1"
    if [[ -z "${VERCEL_BYPASS_TOKEN}" ]]; then
        echo "${url}"
        return
    fi
    local sep='?'
    [[ "${url}" == *"?"* ]] && sep='&'
    echo "${url}${sep}x-vercel-set-bypass-cookie=true&x-vercel-protection-bypass=${VERCEL_BYPASS_TOKEN}"
}

echo "================================================================"
echo " Buy SPL Token devnet end-to-end test (v0.3 buyer-commit)"
echo "  Seller URL:        ${SELLER_BASE_URL}"
echo "  Facilitator URL:   ${FACILITATOR_URL}"
echo "  Buyer pubkey:      ${BUYER_PUBKEY}"
echo "  Recipient owner:   ${RECIPIENT_OWNER}"
echo "  Token mint:        ${TOKEN_MINT}"
echo "  Quantity:          ${QUANTITY}"
echo "  USDC mint:         ${USDC_MINT}"
echo "  RPC:               ${RPC_URL}"
echo "  Buyer nonce:       ${BUYER_NONCE}"
echo "================================================================"
echo ""

# ---------------------------------------------------------------------------
# Step 0: pin oracleAuthority
# ---------------------------------------------------------------------------

if [[ -z "${ORACLE_PUBKEY}" ]]; then
    SUPPORTED_OUT="${TMPDIR_RUN}/supported.json"
    SUPPORTED_CODE="$(curl -sS -o "${SUPPORTED_OUT}" -w '%{http_code}' \
        "${FACILITATOR_URL}/api/v1/facilitator/supported")"
    if [[ "${SUPPORTED_CODE}" != "200" ]]; then
        echo "❌ failed to fetch ${FACILITATOR_URL}/api/v1/facilitator/supported (HTTP ${SUPPORTED_CODE})" >&2
        exit 1
    fi
    ORACLE_PUBKEY="$(jq -r '
        .kinds[]
        | select(.scheme=="sla-escrow")
        | .extra.oracleAuthorities
        | map(select(startswith("FaciL")|not))
        | .[0] // empty
    ' "${SUPPORTED_OUT}")"
    if [[ -z "${ORACLE_PUBKEY}" || "${ORACLE_PUBKEY}" == "null" ]]; then
        ORACLE_PUBKEY="$(jq -r '.kinds[] | select(.scheme=="sla-escrow") | .extra.oracleAuthorities[0]' "${SUPPORTED_OUT}")"
    fi
    if [[ -z "${ORACLE_PUBKEY}" || "${ORACLE_PUBKEY}" == "null" ]]; then
        echo "❌ no sla-escrow oracleAuthority published on ${FACILITATOR_URL}/api/v1/facilitator/supported" >&2
        exit 1
    fi
    echo "    Auto-picked oracleAuthority from facilitator /supported: ${ORACLE_PUBKEY}"
    echo ""
fi

# ---------------------------------------------------------------------------
# Preflight: seller must be deployed (Vercel routes vercel.json → buy_spl_token_api)
# ---------------------------------------------------------------------------

HEALTH_URL="$(attach_bypass "${SELLER_BASE_URL}/health")"
HEALTH_CODE="$(curl -sS -o /dev/null -w '%{http_code}' "${HEALTH_URL}")"
if [[ "${HEALTH_CODE}" == "404" ]]; then
    echo "❌ Seller ${SELLER_BASE_URL} returned HTTP 404 on /health." >&2
    echo "   The preview deployment is missing or has no Vercel routes." >&2
    echo "   Ensure vercel.json is in the repo root and redeploy x402-buy-spl-token." >&2
    echo "   Legacy seller (seller-commit, not v0.3): https://preview.spl-token.signer-payer.me" >&2
    exit 1
fi
if [[ "${HEALTH_CODE}" != "200" ]]; then
    echo "⚠️  Seller /health returned HTTP ${HEALTH_CODE} (expected 200). Continuing…" >&2
fi

# ---------------------------------------------------------------------------
# Step 1: probe 402 envelope (commitMaterial, no seller slaHash)
# ---------------------------------------------------------------------------

echo ">>> [1/7] Probe seller's 402 envelope (no PAYMENT-SIGNATURE)"
BUY_PATH="/api/v1/buy-spl-token?token=${TOKEN_MINT}&quantity=${QUANTITY}&recipient_owner=${RECIPIENT_OWNER}&buyer_nonce=${BUYER_NONCE}"
PROBE_URL="$(attach_bypass "${SELLER_BASE_URL}${BUY_PATH}")"
PROBE_OUT="${TMPDIR_RUN}/402.json"
PROBE_CODE="$(curl -sS -o "${PROBE_OUT}" -w '%{http_code}' "${PROBE_URL}")"
if [[ "${PROBE_CODE}" != "402" ]]; then
    echo "❌ Step 1: expected HTTP 402, got ${PROBE_CODE}" >&2
    head -c 1024 "${PROBE_OUT}" >&2
    echo "" >&2
    if [[ "${PROBE_CODE}" == "404" ]]; then
        echo "    /health was reachable but /api/v1/buy-spl-token is not routed." >&2
        echo "    Check vercel.json routes and redeploy." >&2
    fi
    exit 1
fi

ACCEPTS_LINE="$(jq -c '.accepts[0]' "${PROBE_OUT}")"
COMMIT="$(echo "${ACCEPTS_LINE}" | jq -c '.extra.commitMaterial')"
COMMIT_VARIANT="$(echo "${ACCEPTS_LINE}" | jq -r '.extra.commitVariant // empty')"
MERCHANT_WALLET="$(echo "${ACCEPTS_LINE}" | jq -r '.extra.merchantWallet // empty')"
BENEFICIARY="$(echo "${ACCEPTS_LINE}" | jq -r '.extra.beneficiary // empty')"

SELLER_PUBKEY="$(echo "${COMMIT}" | jq -r '.sellerPubkey')"
TOKEN_NAME="$(echo "${COMMIT}" | jq -r '.tokenName')"
TOKEN_DECIMALS="$(echo "${COMMIT}" | jq -r '.tokenDecimals')"
DELIVER_AMOUNT_RAW="$(echo "${COMMIT}" | jq -r '.deliverAmountRaw')"
PAYMENT_AMOUNT_CM="$(echo "${COMMIT}" | jq -r '.paymentAmountRaw')"
COMMIT_QUANTITY="$(echo "${COMMIT}" | jq -r '.quantity')"
COMMIT_BUYER_NONCE="$(echo "${COMMIT}" | jq -r '.buyerNonce')"
CLUSTER="$(echo "${COMMIT}" | jq -r '.cluster')"
PROFILE_ID="$(echo "${COMMIT}" | jq -r '.profileId')"
SLA_VERSION="$(echo "${COMMIT}" | jq -r '.version')"
PAY_TO_FROM_402="$(echo "${ACCEPTS_LINE}" | jq -r '.payTo')"
USDC_AMOUNT_RAW="$(echo "${ACCEPTS_LINE}" | jq -r '.amount')"
NETWORK="$(echo "${ACCEPTS_LINE}" | jq -r '.network')"
ASSET="$(echo "${ACCEPTS_LINE}" | jq -r '.asset')"
TIMEOUT_SEC="$(echo "${ACCEPTS_LINE}" | jq -r '.maxTimeoutSeconds')"
RESOURCE_FIELD="$(jq -c '.resource' "${PROBE_OUT}")"

[[ -n "${COMMIT}" && "${COMMIT}" != "null" ]] || {
    echo "❌ extra.commitMaterial missing in 402 (expected buyer-commit / v0.3 seller)" >&2
    echo "   ${SELLER_BASE_URL} may be the legacy spl-token-balance-serverless deployment." >&2
    echo "   Redeploy x402-buy-spl-token to this host, or point SELLER_BASE_URL at a v0.3 preview." >&2
    exit 1
}
[[ "${COMMIT_VARIANT}" == "buyer-commit" ]] || {
    echo "❌ extra.commitVariant is '${COMMIT_VARIANT}', expected 'buyer-commit'" >&2
    exit 1
}
[[ -n "${MERCHANT_WALLET}" && "${MERCHANT_WALLET}" != "null" ]] || {
    echo "❌ extra.merchantWallet missing (required on v0.3.1)" >&2
    exit 1
}
[[ -n "${SELLER_PUBKEY}" && "${SELLER_PUBKEY}" != "null" ]] || {
    echo "❌ commitMaterial.sellerPubkey missing" >&2
    exit 1
}
[[ -n "${DELIVER_AMOUNT_RAW}" && "${DELIVER_AMOUNT_RAW}" != "null" ]] || {
    echo "❌ commitMaterial.deliverAmountRaw missing" >&2
    exit 1
}
[[ "${PAYMENT_AMOUNT_CM}" == "${USDC_AMOUNT_RAW}" ]] || {
    echo "❌ commitMaterial.paymentAmountRaw (${PAYMENT_AMOUNT_CM}) ≠ accepts[].amount (${USDC_AMOUNT_RAW})" >&2
    exit 1
}
[[ "${COMMIT_BUYER_NONCE}" == "${BUYER_NONCE}" ]] || {
    echo "❌ commitMaterial.buyerNonce (${COMMIT_BUYER_NONCE}) ≠ request buyer_nonce (${BUYER_NONCE})" >&2
    exit 1
}
[[ "${COMMIT_QUANTITY}" == "${QUANTITY}" ]] || {
    echo "❌ commitMaterial.quantity (${COMMIT_QUANTITY}) ≠ request quantity (${QUANTITY})" >&2
    exit 1
}
[[ "${NETWORK}" == "${DEVNET_CHAIN_ID}" ]] || {
    echo "⚠️  402 network is ${NETWORK}, expected devnet ${DEVNET_CHAIN_ID}." >&2
    echo "    Continuing — the FundPayment will use this network as-is." >&2
}

if [[ -n "${BENEFICIARY}" && "${BENEFICIARY}" != "null" ]]; then
    FUND_PAYMENT_SELLER="${BENEFICIARY}"
else
    FUND_PAYMENT_SELLER="${MERCHANT_WALLET}"
fi

PAYMENT_UID_HEX="${PAYMENT_UID_HEX:-$(openssl rand -hex 32)}"
[[ "${PAYMENT_UID_HEX}" =~ ^[0-9a-f]{64}$ ]] || {
    echo "❌ PAYMENT_UID_HEX is not 64 lowercase hex chars: ${PAYMENT_UID_HEX}" >&2
    exit 1
}

SLA_HASH_HEX="$(python3 - "${TOKEN_MINT}" "${TOKEN_DECIMALS}" "${DELIVER_AMOUNT_RAW}" \
            "${RECIPIENT_OWNER}" "${SELLER_PUBKEY}" "${BUYER_NONCE}" \
            "${PAYMENT_UID_HEX}" "${CLUSTER}" "${PROFILE_ID}" "${SLA_VERSION}" <<'PY'
import json, sys, hashlib
mint, decimals, min_amount, recipient, seller, nonce, uid, cluster, profile, version = sys.argv[1:]
sla = {
    "buyer_nonce": nonce,
    "cluster": cluster,
    "expected_transfers": [{
        "decimals": int(decimals),
        "direction": "in",
        "min_amount": str(min_amount),
        "mint": mint,
        "recipient_owner": recipient,
        "sender_owner": seller,
    }],
    "payment_uid": uid,
    "profile_id": profile,
    "version": int(version),
}
canonical = json.dumps(sla, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
print(hashlib.sha256(canonical).hexdigest())
PY
)"
[[ "${SLA_HASH_HEX}" =~ ^[0-9a-f]{64}$ ]] || {
    echo "❌ buyer-side SLA hash failed: ${SLA_HASH_HEX}" >&2
    exit 1
}

echo "    commitVariant:     ${COMMIT_VARIANT}"
echo "    sellerPubkey (S):  ${SELLER_PUBKEY}"
echo "    merchantWallet:    ${MERCHANT_WALLET}"
echo "    fundPayment.seller:${FUND_PAYMENT_SELLER}"
echo "    payTo (escrow):    ${PAY_TO_FROM_402}"
echo "    amount (USDC raw): ${USDC_AMOUNT_RAW}"
echo "    deliverAmountRaw:  ${DELIVER_AMOUNT_RAW}"
echo "    quantity:          ${COMMIT_QUANTITY}"
echo "    asset (USDC mint): ${ASSET}"
echo "    network:           ${NETWORK}"
echo "    cluster:           ${CLUSTER}"
echo "    token:             ${TOKEN_NAME} (mint=${TOKEN_MINT}, decimals=${TOKEN_DECIMALS})"
echo "    timeout seconds:   ${TIMEOUT_SEC}"
echo "    payment_uid:       ${PAYMENT_UID_HEX}"
echo "    sla_hash:          ${SLA_HASH_HEX}"
if [[ "${FUND_PAYMENT_SELLER}" != "${SELLER_PUBKEY}" ]]; then
    echo "    (distinct delivery signer vs. FundPayment.seller — production-shaped setup)"
fi
if [[ "${MERCHANT_WALLET}" == "${PAY_TO_FROM_402}" ]]; then
    echo "⚠️  merchantWallet equals payTo (escrow PDA). v0.3.1 expects these to differ." >&2
fi
echo ""

# ---------------------------------------------------------------------------
# Step 2: build unsigned FundPayment tx via pr402 facilitator
# ---------------------------------------------------------------------------

echo ">>> [2/7] POST ${FACILITATOR_URL}/api/v1/facilitator/build-sla-escrow-payment-tx"

# pr402 resolves FundPayment.seller from extra.beneficiary ?? extra.merchantWallet.
# That pubkey receives USDC on ReleasePayment — NOT extra.commitMaterial.sellerPubkey
# (the delivery hot key that signs SPL TransferChecked).
ACCEPTED_LINE="${ACCEPTS_LINE}"

BUILD_BODY="$(jq -n \
    --arg payer "${BUYER_PUBKEY}" \
    --argjson accepted "${ACCEPTED_LINE}" \
    --argjson resource "${RESOURCE_FIELD}" \
    --arg sla_hash "${SLA_HASH_HEX}" \
    --arg payment_uid_hex "${PAYMENT_UID_HEX}" \
    --arg oracle "${ORACLE_PUBKEY}" \
    '{
        payer:           $payer,
        accepted:        $accepted,
        resource:        $resource,
        slaHash:         $sla_hash,
        paymentUidHex:   $payment_uid_hex,
        oracleAuthority: $oracle,
        skipSourceBalanceCheck: false,
        facilitatorPaysTransactionFees: false
    }')"

BUILD_OUT="${TMPDIR_RUN}/build.json"
BUILD_CODE="$(curl -sS -o "${BUILD_OUT}" -w '%{http_code}' \
    -H "Content-Type: application/json" \
    -X POST "${FACILITATOR_URL}/api/v1/facilitator/build-sla-escrow-payment-tx" \
    -d "${BUILD_BODY}")"

if [[ "${BUILD_CODE}" != "200" ]]; then
    echo "❌ Step 2: build-sla-escrow-payment-tx HTTP ${BUILD_CODE}" >&2
    cat "${BUILD_OUT}" >&2
    echo "" >&2
    echo "    Common causes:" >&2
    echo "    - oracleAuthority ${ORACLE_PUBKEY} not in accepted.extra.oracleAuthorities" >&2
    echo "    - buyer ${BUYER_PUBKEY} has no devnet USDC ATA / insufficient balance" >&2
    echo "    - slaHash does not match seller recompute from commitMaterial + payment_uid" >&2
    echo "    - facilitator program id differs from the seller's" >&2
    exit 1
fi

UNSIGNED_TX_B64="$(jq -r '.transaction' "${BUILD_OUT}")"
RECENT_BH="$(jq -r '.recentBlockhash // .blockhash // empty' "${BUILD_OUT}")"
PR402_PAYMENT_UID_HEX="$(jq -r '.paymentUidHex // empty' "${BUILD_OUT}")"
if [[ -n "${PR402_PAYMENT_UID_HEX}" && "${PR402_PAYMENT_UID_HEX}" != "${PAYMENT_UID_HEX}" ]]; then
    echo "❌ pr402 returned a different paymentUidHex (${PR402_PAYMENT_UID_HEX}) than we requested (${PAYMENT_UID_HEX})." >&2
    echo "   This breaks the SLA hash binding; aborting." >&2
    exit 1
fi

[[ -n "${UNSIGNED_TX_B64}" && "${UNSIGNED_TX_B64}" != "null" ]] || {
    echo "❌ build response missing .transaction (base64 vtx)" >&2
    cat "${BUILD_OUT}" >&2
    exit 1
}
[[ -n "${RECENT_BH}" ]] || {
    echo "❌ build response missing .recentBlockhash / .blockhash" >&2
    cat "${BUILD_OUT}" >&2
    exit 1
}

echo "    payment_uid (hex):  ${PAYMENT_UID_HEX}"
echo "    recent blockhash:   ${RECENT_BH}"
echo "    unsigned tx bytes:  ${#UNSIGNED_TX_B64} chars (base64)"
echo ""

# ---------------------------------------------------------------------------
# Step 3: sign FundPayment locally
# ---------------------------------------------------------------------------

echo ">>> [3/7] Sign FundPayment tx locally (cargo run --example e2e_sign_sla_escrow_tx)"
SIGN_OUT="${TMPDIR_RUN}/signed.b64"
(
    cd "${PR402_ROOT}"
    echo "${UNSIGNED_TX_B64}" | cargo run --quiet --example e2e_sign_sla_escrow_tx -- \
        "${BUYER_KEYPAIR}" "${RECENT_BH}"
) > "${SIGN_OUT}"

SIGNED_TX_B64="$(tr -d '[:space:]' <"${SIGN_OUT}")"
[[ -n "${SIGNED_TX_B64}" ]] || {
    echo "❌ signing produced empty output" >&2
    exit 1
}
echo "    signed tx bytes:    ${#SIGNED_TX_B64} chars (base64)"
echo ""

# ---------------------------------------------------------------------------
# Step 4: build x402 v2 PAYMENT-SIGNATURE body
# ---------------------------------------------------------------------------

echo ">>> [4/7] Build x402 v2 PAYMENT-SIGNATURE body"
PAYMENT_BODY="$(jq -nc \
    --argjson accepted "${ACCEPTED_LINE}" \
    --arg tx "${SIGNED_TX_B64}" \
    '{
        x402Version: 2,
        paymentRequirements: $accepted,
        paymentPayload: {
            x402Version: 2,
            accepted:    $accepted,
            payload:     { transaction: $tx }
        }
    }')"
PAYMENT_HEADER_FILE="${TMPDIR_RUN}/payment-signature.json"
printf '%s' "${PAYMENT_BODY}" > "${PAYMENT_HEADER_FILE}"
echo "    body bytes:        $(wc -c <"${PAYMENT_HEADER_FILE}")"
echo ""

# ---------------------------------------------------------------------------
# Step 5: paid GET with PAYMENT-SIGNATURE
# ---------------------------------------------------------------------------

echo ">>> [5/7] GET ${SELLER_BASE_URL}${BUY_PATH%%\?*}?... (with PAYMENT-SIGNATURE)"
PAID_OUT="${TMPDIR_RUN}/paid.json"
PAID_CODE="$(curl -sS -o "${PAID_OUT}" -w '%{http_code}' \
    --max-time 60 \
    -H "Content-Type: application/json" \
    -H "PAYMENT-SIGNATURE: $(cat "${PAYMENT_HEADER_FILE}")" \
    "${PROBE_URL}")"

if [[ "${PAID_CODE}" != "200" ]]; then
    echo "❌ Step 5: paid GET HTTP ${PAID_CODE}" >&2
    cat "${PAID_OUT}" >&2
    echo "" >&2
    echo "    Diagnostics:" >&2
    echo "    - 402 'sla_hash_mismatch'   → buyer SLA differs from seller recompute (check commitMaterial.deliverAmountRaw + payment_uid)." >&2
    echo "    - 402 'settlement_failed'   → pr402 verify/settle rejected (oracleAuthority, mint, amount mismatch)." >&2
    echo "    - 502 'transfer_failed'     → seller RPC could not land SPL TransferChecked." >&2
    echo "    - 502 'submit_delivery_failed' → SubmitDelivery exhausted retries (merchant signer must match FundPayment.seller)." >&2
    echo "    - 500 'evidence_schema_invalid' → evidence document failed JSON schema validation." >&2
    echo "    - 409 'concurrent_request'  → another retry holds the advisory lock." >&2
    echo "    - 409 'order_failed'        → an earlier attempt was marked failed." >&2
    exit 1
fi

PAYMENT_UID="$(jq -r '.paymentUid' "${PAID_OUT}")"
TRANSFER_SIG="$(jq -r '.transferSignature' "${PAID_OUT}")"
EVIDENCE_URL="$(jq -r '.evidenceUrl' "${PAID_OUT}")"
DELIVERY_SIG="$(jq -r '.deliverySignature' "${PAID_OUT}")"
PAID_SLA_HASH="$(jq -r '.slaHash' "${PAID_OUT}")"

echo "    status:             $(jq -r '.status' "${PAID_OUT}")"
echo "    paymentUid:         ${PAYMENT_UID}"
echo "    slaHash (echoed):   ${PAID_SLA_HASH}"
echo "    transferSignature:  ${TRANSFER_SIG}"
echo "    evidenceUrl:        ${EVIDENCE_URL}"
echo "    deliverySignature:  ${DELIVERY_SIG}"

[[ "${PAID_SLA_HASH}" == "${SLA_HASH_HEX}" ]] || {
    echo "❌ slaHash echoed by the 200 (${PAID_SLA_HASH}) does not match buyer-computed (${SLA_HASH_HEX})" >&2
    exit 1
}

echo ""

# ---------------------------------------------------------------------------
# Step 6: on-chain + registry verification
# ---------------------------------------------------------------------------

echo ">>> [6/7] On-chain verification"
if [[ "${SKIP_ONCHAIN_VERIFY}" == "1" ]]; then
    echo "    SKIP_ONCHAIN_VERIFY=1 — skipping solana confirm + evidence fetch"
else
    echo "    solana confirm ${TRANSFER_SIG} (SPL TransferChecked)"
    if ! solana confirm -u "${RPC_URL}" "${TRANSFER_SIG}" 2>&1 | sed 's/^/      /'; then
        echo "❌ TransferChecked signature did not confirm" >&2
        exit 1
    fi

    echo "    solana confirm ${DELIVERY_SIG} (SubmitDelivery)"
    if ! solana confirm -u "${RPC_URL}" "${DELIVERY_SIG}" 2>&1 | sed 's/^/      /'; then
        echo "❌ SubmitDelivery signature did not confirm" >&2
        exit 1
    fi

    echo "    curl ${EVIDENCE_URL} (evidence registry)"
    EVIDENCE_OUT="${TMPDIR_RUN}/evidence.json"
    EVIDENCE_CODE="$(curl -sS -o "${EVIDENCE_OUT}" -w '%{http_code}' "${EVIDENCE_URL}")"
    if [[ "${EVIDENCE_CODE}" != "200" ]]; then
        echo "❌ evidence fetch HTTP ${EVIDENCE_CODE}" >&2
        cat "${EVIDENCE_OUT}" >&2
        exit 1
    fi
    EVIDENCE_TX_SIG="$(jq -r '.tx_signature' "${EVIDENCE_OUT}")"
    EVIDENCE_PAYMENT_UID="$(jq -r '.payment_uid' "${EVIDENCE_OUT}")"
    [[ "${EVIDENCE_TX_SIG}" == "${TRANSFER_SIG}" ]] || {
        echo "❌ evidence.tx_signature (${EVIDENCE_TX_SIG}) ≠ paid response transferSignature (${TRANSFER_SIG})" >&2
        exit 1
    }
    [[ "${EVIDENCE_PAYMENT_UID}" == "${PAYMENT_UID}" ]] || {
        echo "❌ evidence.payment_uid (${EVIDENCE_PAYMENT_UID}) ≠ paid response paymentUid (${PAYMENT_UID})" >&2
        exit 1
    }
    echo "    evidence document: tx=${EVIDENCE_TX_SIG}, payment_uid=${EVIDENCE_PAYMENT_UID} ✓"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 7: idempotency replay
# ---------------------------------------------------------------------------

echo ">>> [7/7] Replay paid GET (idempotency / no second on-chain action)"
if [[ "${SKIP_REPLAY}" == "1" ]]; then
    echo "    SKIP_REPLAY=1 — skipping idempotency replay"
else
    REPLAY_OUT="${TMPDIR_RUN}/replay.json"
    REPLAY_CODE="$(curl -sS -o "${REPLAY_OUT}" -w '%{http_code}' \
        --max-time 60 \
        -H "Content-Type: application/json" \
        -H "PAYMENT-SIGNATURE: $(cat "${PAYMENT_HEADER_FILE}")" \
        "${PROBE_URL}")"
    if [[ "${REPLAY_CODE}" != "200" ]]; then
        echo "❌ replay HTTP ${REPLAY_CODE}" >&2
        cat "${REPLAY_OUT}" >&2
        exit 1
    fi
    R_TRANSFER="$(jq -r '.transferSignature' "${REPLAY_OUT}")"
    R_DELIVERY="$(jq -r '.deliverySignature' "${REPLAY_OUT}")"
    R_EVIDENCE="$(jq -r '.evidenceUrl' "${REPLAY_OUT}")"
    [[ "${R_TRANSFER}" == "${TRANSFER_SIG}" ]] || {
        echo "❌ replay transferSignature differs (orig=${TRANSFER_SIG}, replay=${R_TRANSFER})" >&2
        exit 1
    }
    [[ "${R_DELIVERY}" == "${DELIVERY_SIG}" ]] || {
        echo "❌ replay deliverySignature differs" >&2
        exit 1
    }
    [[ "${R_EVIDENCE}" == "${EVIDENCE_URL}" ]] || {
        echo "❌ replay evidenceUrl differs" >&2
        exit 1
    }
    echo "    replay returns identical signatures ✓"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 8: ReleasePayment (FundPayment.seller receives USDC)
# ---------------------------------------------------------------------------

if [[ "${SKIP_RELEASE:-0}" == "1" ]]; then
    echo ">>> [8/8] Skipping ReleasePayment (SKIP_RELEASE=1)"
else
    echo ">>> [8/8] Drive ReleasePayment (buyer signs; permissionless once oracle approved)"

    SLA_ESCROW_PROGRAM_ID="${SLA_ESCROW_PROGRAM_ID:-s5zkKiy8FD9nFdAhQZoHHV3G8s4QCPzE4cR9U4Hr4ZH}"
    RELEASE_POLL_DEADLINE_SEC="${RELEASE_POLL_DEADLINE_SEC:-60}"

    RELEASE_OUT="${TMPDIR_RUN}/release.txt"
    set +e
    RPC_URL="${RPC_URL}" \
    SLA_ESCROW_PROGRAM_ID="${SLA_ESCROW_PROGRAM_ID}" \
    USDC_MINT="${USDC_MINT}" \
    PAYMENT_UID_HEX="${PAYMENT_UID_HEX}" \
    RELEASE_SELLER_PUBKEY="${FUND_PAYMENT_SELLER}" \
    ORACLE_PUBKEY="${ORACLE_PUBKEY}" \
    BUYER_KEYPAIR="${BUYER_KEYPAIR}" \
    RELEASE_POLL_DEADLINE_SEC="${RELEASE_POLL_DEADLINE_SEC}" \
    python3 - <<'PY' > "${RELEASE_OUT}" 2>&1
import json, os, sys, time

from solana.rpc.api import Client
from solana.rpc.types import TxOpts
from solana.rpc.commitment import Confirmed
from solders.instruction import AccountMeta, Instruction
from solders.keypair import Keypair
from solders.message import MessageV0
from solders.pubkey import Pubkey
from solders.transaction import VersionedTransaction
from spl.token.constants import ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID
from spl.token.instructions import get_associated_token_address

RPC_URL          = os.environ["RPC_URL"]
PROGRAM_ID       = Pubkey.from_string(os.environ["SLA_ESCROW_PROGRAM_ID"])
USDC_MINT        = Pubkey.from_string(os.environ["USDC_MINT"])
PAYMENT_UID_HEX  = os.environ["PAYMENT_UID_HEX"]
RELEASE_SELLER   = Pubkey.from_string(os.environ["RELEASE_SELLER_PUBKEY"])
ORACLE_PUBKEY    = Pubkey.from_string(os.environ["ORACLE_PUBKEY"])
BUYER_KEYPAIR    = os.environ["BUYER_KEYPAIR"]
DEADLINE_SEC     = int(os.environ.get("RELEASE_POLL_DEADLINE_SEC", "60"))

with open(BUYER_KEYPAIR, "r", encoding="utf-8") as f:
    arr = json.load(f)
buyer = Keypair.from_bytes(bytes(arr))

def find_pda(seeds, prog):
    pda, _bump = Pubkey.find_program_address(seeds, prog)
    return pda

bank          = find_pda([b"bank"], PROGRAM_ID)
config        = find_pda([b"config"], PROGRAM_ID)
escrow        = find_pda([b"escrow", bytes(USDC_MINT), bytes(bank)], PROGRAM_ID)
payment_uid   = bytes.fromhex(PAYMENT_UID_HEX)
payment       = find_pda([b"payment", payment_uid, bytes(bank)], PROGRAM_ID)

print(f"bank   : {bank}")
print(f"config : {config}")
print(f"escrow : {escrow}")
print(f"payment: {payment}")
print(f"release seller (FundPayment.seller): {RELEASE_SELLER}")

client = Client(RPC_URL, commitment=Confirmed, timeout=30)
deadline = time.monotonic() + DEADLINE_SEC

approved = False
while time.monotonic() < deadline:
    info = client.get_account_info(payment, commitment=Confirmed)
    if info.value is None:
        print("  …payment account not yet visible, retrying")
    else:
        data = info.value.data
        rs = data[-1]
        st = data[-2]
        print(f"  payment.state={st} resolution_state={rs}")
        if st != 0:
            print(f"❌ payment already in terminal state {st} (1=Released, 2=Refunded). Nothing to do.")
            sys.exit(2)
        if rs == 1:
            approved = True
            break
        if rs == 2:
            print("ℹ️  oracle rejected — driving RefundPayment instead is out of scope here")
            sys.exit(3)
    time.sleep(2.5)
if not approved:
    print(f"❌ oracle verdict did not arrive within {DEADLINE_SEC}s")
    sys.exit(4)

DISCRIMINATOR = bytes([1])  # EscrowInstruction::ReleasePayment

escrow_tokens   = get_associated_token_address(escrow, USDC_MINT)
seller_tokens   = get_associated_token_address(RELEASE_SELLER, USDC_MINT)
oracle_tokens   = get_associated_token_address(ORACLE_PUBKEY, USDC_MINT)
SYS_PROGRAM     = Pubkey.from_string("11111111111111111111111111111111")

accounts = [
    AccountMeta(pubkey=buyer.pubkey(),                    is_signer=True,  is_writable=True),
    AccountMeta(pubkey=bank,                              is_signer=False, is_writable=False),
    AccountMeta(pubkey=config,                            is_signer=False, is_writable=False),
    AccountMeta(pubkey=escrow,                            is_signer=False, is_writable=True),
    AccountMeta(pubkey=payment,                           is_signer=False, is_writable=True),
    AccountMeta(pubkey=USDC_MINT,                         is_signer=False, is_writable=False),
    AccountMeta(pubkey=escrow_tokens,                     is_signer=False, is_writable=True),
    AccountMeta(pubkey=seller_tokens,                     is_signer=False, is_writable=True),
    AccountMeta(pubkey=RELEASE_SELLER,                    is_signer=False, is_writable=True),
    AccountMeta(pubkey=TOKEN_PROGRAM_ID,                  is_signer=False, is_writable=False),
    AccountMeta(pubkey=ASSOCIATED_TOKEN_PROGRAM_ID,       is_signer=False, is_writable=False),
    AccountMeta(pubkey=SYS_PROGRAM,                       is_signer=False, is_writable=False),
    AccountMeta(pubkey=oracle_tokens,                     is_signer=False, is_writable=True),
    AccountMeta(pubkey=ORACLE_PUBKEY,                     is_signer=False, is_writable=True),
]

ix = Instruction(program_id=PROGRAM_ID, data=DISCRIMINATOR, accounts=accounts)

bh = client.get_latest_blockhash().value.blockhash
msg = MessageV0.try_compile(buyer.pubkey(), [ix], [], bh)
tx = VersionedTransaction(msg, [buyer])

resp = client.send_transaction(tx, opts=TxOpts(skip_preflight=False, preflight_commitment=Confirmed))
sig = resp.value
print(f"sent ReleasePayment: {sig}")

wait_deadline = time.monotonic() + 30
while time.monotonic() < wait_deadline:
    st = client.get_signature_statuses([sig]).value[0]
    if st is not None and st.confirmation_status is not None:
        print(f"  confirmed: {st.confirmation_status}")
        break
    time.sleep(1)
else:
    print("⚠️  confirmation did not land within 30s; tx may still be in-flight")

info = client.get_account_info(payment, commitment=Confirmed)
if info.value is not None:
    data = info.value.data
    print(f"  payment.state(after)={data[-2]} resolution_state(after)={data[-1]}  (1=Released)")

print(f"RELEASE_SIGNATURE={sig}")
PY
    PY_RC=$?
    set -e
    cat "${RELEASE_OUT}"
    if [[ ${PY_RC} -ne 0 ]]; then
        echo "❌ Step 8: ReleasePayment failed (exit=${PY_RC})" >&2
        exit 1
    fi
    RELEASE_SIG="$(grep -E '^RELEASE_SIGNATURE=' "${RELEASE_OUT}" | cut -d= -f2)"
    echo "    releaseSignature:   ${RELEASE_SIG}"
fi
echo ""

echo "================================================================"
echo " ✅ Buy SPL Token end-to-end devnet test PASSED"
echo "    paymentUid:        ${PAYMENT_UID}"
echo "    slaHash:           ${SLA_HASH_HEX}"
echo "    transferSignature: ${TRANSFER_SIG}"
echo "    deliverySignature: ${DELIVERY_SIG}"
echo "    evidenceUrl:       ${EVIDENCE_URL}"
[[ -n "${RELEASE_SIG:-}" ]] && echo "    releaseSignature:  ${RELEASE_SIG}"
echo "================================================================"
