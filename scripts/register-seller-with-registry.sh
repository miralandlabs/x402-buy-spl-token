#!/usr/bin/env bash
#
# Register a seller wallet with an evidence registry (oracle-onchain-transfer
# or any sibling implementation that exposes the canonical
# `/v1/registry/seller/{challenge,register}` endpoints) and print the bearer
# token the registry returned.
#
# Two-step flow per `oracles/oracle-common/src/registry/auth.rs`:
#
#   1. GET  /v1/registry/seller/challenge?wallet=<seller_pubkey>
#         → { "challenge": "<random nonce>", "expires_at": "<rfc3339>" }
#
#   2. Sign challenge.as_bytes() with the seller's ed25519 secret key
#      (NOT `solana sign-offchain-message` — that adds a Solana-specific
#      message prefix the registry rejects).
#
#   3. POST /v1/registry/seller/register
#         { "wallet": <seller_pubkey>,
#           "signature": <base58 ed25519 sig over challenge bytes>,
#           "challenge": <challenge from step 1> }
#         → { "id": <int>, "token": "<RAW_BEARER>" }
#
#   4. Save <RAW_BEARER> as Vercel env `REGISTRY_BEARER_TOKEN` on the
#      x402-buy-spl-token deployment. The raw token is shown exactly once;
#      the registry stores only SHA256(token).
#
# Usage:
#
#   scripts/register-seller-with-registry.sh \
#       <registry_base_url> \
#       <seller_keypair_json>
#       [--copy]              # macOS: pbcopy the bearer to clipboard
#       [--no-newline]        # capture-friendly: no trailing \n
#       [--print-pubkey]      # echo the derived pubkey to stderr
#
# Example (devnet preview):
#
#   scripts/register-seller-with-registry.sh \
#       https://oracle.innoloyalty.com/devnet \
#       ../demo-wallets/seller-keypair.json \
#       --print-pubkey --copy
#
# Output: the raw bearer token on stdout. Diagnostics on stderr.
#
# Safety:
#   - The keypair JSON is opened only inside python3; the secret never
#     reaches a shell variable.
#   - The bearer token is also assembled inside python3 and printed once
#     to stdout. The script never logs it elsewhere.
#   - Read-only against the keypair file.

set -euo pipefail

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "❌ missing command: $1" >&2
        exit 1
    }
}

# --- Args ---
REGISTRY_BASE_URL=""
KEYPAIR_FILE=""
PRINT_PUBKEY=0
COPY_CLIPBOARD=0
APPEND_NEWLINE=1   # default: emit a trailing \n so the prompt falls clean.
                   # Use `--no-newline` for capture-into-variable invocations.

POSITIONAL=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --print-pubkey) PRINT_PUBKEY=1 ;;
        --copy)         COPY_CLIPBOARD=1 ;;
        --newline)      APPEND_NEWLINE=1 ;;
        --no-newline)   APPEND_NEWLINE=0 ;;
        -h|--help)      usage 0 ;;
        --)
            shift
            while [[ $# -gt 0 ]]; do
                POSITIONAL+=("$1")
                shift
            done
            break
            ;;
        -*) echo "unknown flag: $1" >&2; usage 2 ;;
        *)  POSITIONAL+=("$1") ;;
    esac
    shift
done

if [[ ${#POSITIONAL[@]} -ne 2 ]]; then
    echo "❌ expected 2 positional arguments: <registry_base_url> <seller_keypair_json>" >&2
    usage 2
fi
REGISTRY_BASE_URL="${POSITIONAL[0]}"
KEYPAIR_FILE="${POSITIONAL[1]}"

# Strip any trailing slash for clean URL composition.
REGISTRY_BASE_URL="${REGISTRY_BASE_URL%/}"

if [[ ! -f "${KEYPAIR_FILE}" ]]; then
    echo "❌ keypair file not found: ${KEYPAIR_FILE}" >&2
    exit 1
fi
if [[ ! "${REGISTRY_BASE_URL}" =~ ^https?:// ]]; then
    echo "❌ registry URL must start with http:// or https:// (got ${REGISTRY_BASE_URL})" >&2
    exit 1
fi

require_cmd python3
require_cmd curl
require_cmd jq

# Single workspace for all tempfiles so we set EXIT trap once and forget it.
TMPDIR_RUN="$(mktemp -d -t registry-register-XXXXXX)"
trap 'rm -rf "${TMPDIR_RUN}"' EXIT

# --- Pre-flight reachability probe ---------------------------------------
# Registration consumes a one-shot bearer: the registry shows the raw token
# exactly once on the POST /seller/register response and stores only its
# SHA-256. If we run the full flow against an unhealthy registry, the
# operator may have to re-register and rotate. Probe /v1/registry/info
# (no auth, no DB write) to fail fast on common misconfigurations:
#   - registry bound only to loopback while we hit it from outside
#     (curl reports "Empty reply from server" — see Posture A vs B in
#     scripts/docker/onchain-transfer-{devnet,mainnet}.env.example)
#   - wrong base URL or a stale DNS entry
#   - process started but Postgres is unreachable; the binary will hang
#     for ~5s on every request before closing the connection
INFO_OUT="${TMPDIR_RUN}/info.json"
INFO_CODE="$(curl -sS -o "${INFO_OUT}" -w '%{http_code}' --max-time 8 \
    "${REGISTRY_BASE_URL}/v1/registry/info" || echo 000)"
case "${INFO_CODE}" in
    200) ;;  # registry is healthy — proceed.
    000)
        echo "❌ pre-flight: ${REGISTRY_BASE_URL}/v1/registry/info — connection failed / timeout" >&2
        echo "   Common causes (in order of likelihood):" >&2
        echo "   1. Registry bound to 127.0.0.1 only (Posture B) and you're hitting it from outside." >&2
        echo "      Operator runs:  sudo ss -tlnp 'sport = :PORT'  →  Local Address must be 0.0.0.0:PORT" >&2
        echo "      Fix: set BIND_ADDR=0.0.0.0:PORT in the unit's env file and restart, or front" >&2
        echo "      with nginx for TLS termination per Posture B." >&2
        echo "   2. Process is wedged (waiting on Postgres). Operator checks systemd / docker logs." >&2
        echo "   3. Wrong base URL — typo in REGISTRY_BASE_URL." >&2
        exit 1
        ;;
    *)
        echo "❌ pre-flight: ${REGISTRY_BASE_URL}/v1/registry/info → HTTP ${INFO_CODE}" >&2
        head -c 1024 "${INFO_OUT}" >&2 || true
        echo "" >&2
        echo "   Aborting before consuming the one-shot bearer flow." >&2
        exit 1
        ;;
esac

# --- Probe Python crypto availability up front ---
# We need a working ed25519 implementation. Either `pynacl` or `cryptography`
# is fine; both are in standard scientific-Python distributions. A dedicated
# probe gives a clear install hint instead of failing inside the heredoc.
CRYPTO_LIB="$(
    python3 - <<'PY'
try:
    import nacl.signing  # type: ignore
    print("pynacl")
except Exception:
    try:
        from cryptography.hazmat.primitives.asymmetric import ed25519  # type: ignore
        print("cryptography")
    except Exception:
        print("none")
PY
)"
if [[ "${CRYPTO_LIB}" == "none" ]]; then
    echo "❌ no Python ed25519 library found" >&2
    echo "   Install one of (any will work):" >&2
    echo "     pip3 install pynacl       # smaller, faster" >&2
    echo "     pip3 install cryptography # larger, often already present" >&2
    exit 1
fi

# --- Step 1: derive the wallet pubkey from the keypair file ---
WALLET_PUBKEY="$(
    python3 - "${KEYPAIR_FILE}" <<'PY'
import json, sys
ALPHABET = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
def b58encode(b):
    n = int.from_bytes(b, "big")
    out = bytearray()
    while n > 0:
        n, r = divmod(n, 58)
        out.append(ALPHABET[r])
    pad = sum(1 for x in b[:len(b)] if x == 0 and (b.find(bytes([x])) == 0))
    # Simpler / correct leading-zero handling:
    pad = 0
    for byte in b:
        if byte == 0:
            pad += 1
        else:
            break
    return ("1" * pad) + out.decode("ascii")[::-1]

with open(sys.argv[1], "r", encoding="utf-8") as f:
    arr = json.load(f)
if not isinstance(arr, list) or len(arr) != 64:
    print("❌ keypair JSON must be a 64-int array", file=sys.stderr)
    sys.exit(1)
raw = bytes(arr)
# pubkey is bytes[32..64] in Solana's secret||public layout.
print(b58encode(raw[32:]), end="")
PY
)"
[[ -n "${WALLET_PUBKEY}" ]] || {
    echo "❌ failed to derive wallet pubkey from ${KEYPAIR_FILE}" >&2
    exit 1
}

if [[ "${PRINT_PUBKEY}" == "1" ]]; then
    echo "wallet pubkey:    ${WALLET_PUBKEY}" >&2
fi

# --- Step 2: GET /v1/registry/seller/challenge ---
CHALLENGE_URL="${REGISTRY_BASE_URL}/v1/registry/seller/challenge?wallet=${WALLET_PUBKEY}"

CHAL_OUT="${TMPDIR_RUN}/challenge.json"
CHAL_CODE="$(curl -sS -o "${CHAL_OUT}" -w '%{http_code}' "${CHALLENGE_URL}")"
if [[ "${CHAL_CODE}" != "200" ]]; then
    echo "❌ GET ${CHALLENGE_URL} → HTTP ${CHAL_CODE}" >&2
    cat "${CHAL_OUT}" >&2 || true
    echo "" >&2
    echo "    Common causes:" >&2
    echo "    - registry base URL does not host the seller-registration routes" >&2
    echo "    - registry deployment requires VPN / IP allow-list" >&2
    exit 1
fi

CHALLENGE="$(jq -r '.challenge' "${CHAL_OUT}")"
EXPIRES_AT="$(jq -r '.expires_at' "${CHAL_OUT}")"
[[ -n "${CHALLENGE}" && "${CHALLENGE}" != "null" ]] || {
    echo "❌ challenge missing in response body" >&2
    cat "${CHAL_OUT}" >&2
    exit 1
}
echo "registry URL:     ${REGISTRY_BASE_URL}" >&2
echo "challenge:        ${CHALLENGE}" >&2
echo "expires at:       ${EXPIRES_AT}" >&2

# --- Step 3: sign the challenge with the seller's ed25519 secret ---
# The registry verifies `vk.verify(challenge.as_bytes(), sig)` — i.e. the
# raw UTF-8 bytes of the challenge string, with NO Solana off-chain
# message prefix. We compute the signature inside python3 using whichever
# library probed above.
SIGNATURE_B58="$(
    python3 - "${KEYPAIR_FILE}" "${CHALLENGE}" "${CRYPTO_LIB}" <<'PY'
import json, sys

ALPHABET = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
def b58encode(b):
    n = int.from_bytes(b, "big")
    out = bytearray()
    while n > 0:
        n, r = divmod(n, 58)
        out.append(ALPHABET[r])
    pad = 0
    for byte in b:
        if byte == 0:
            pad += 1
        else:
            break
    return ("1" * pad) + out.decode("ascii")[::-1]

keypair_path, challenge, lib = sys.argv[1], sys.argv[2], sys.argv[3]

with open(keypair_path, "r", encoding="utf-8") as f:
    arr = json.load(f)
raw = bytes(arr)
secret_seed = raw[:32]   # ed25519 32-byte seed; pubkey at raw[32:]

if lib == "pynacl":
    from nacl.signing import SigningKey
    sk = SigningKey(secret_seed)
    sig = sk.sign(challenge.encode("utf-8")).signature   # 64 raw bytes
elif lib == "cryptography":
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    sk = Ed25519PrivateKey.from_private_bytes(secret_seed)
    sig = sk.sign(challenge.encode("utf-8"))             # 64 raw bytes
else:
    print("internal error: unknown CRYPTO_LIB", file=sys.stderr)
    sys.exit(1)

if len(sig) != 64:
    print(f"internal error: ed25519 sig is {len(sig)} bytes, expected 64", file=sys.stderr)
    sys.exit(1)

print(b58encode(sig), end="")
PY
)"
[[ -n "${SIGNATURE_B58}" ]] || {
    echo "❌ ed25519 signing returned empty output" >&2
    exit 1
}

# --- Step 4: POST /v1/registry/seller/register ---
REGISTER_URL="${REGISTRY_BASE_URL}/v1/registry/seller/register"
REG_BODY="$(jq -n \
    --arg wallet    "${WALLET_PUBKEY}" \
    --arg signature "${SIGNATURE_B58}" \
    --arg challenge "${CHALLENGE}" \
    '{wallet: $wallet, signature: $signature, challenge: $challenge}')"

REG_OUT="${TMPDIR_RUN}/register.json"
REG_CODE="$(curl -sS -o "${REG_OUT}" -w '%{http_code}' \
    -H "Content-Type: application/json" \
    -X POST "${REGISTER_URL}" \
    -d "${REG_BODY}")"

if [[ "${REG_CODE}" != "200" ]]; then
    echo "❌ POST ${REGISTER_URL} → HTTP ${REG_CODE}" >&2
    cat "${REG_OUT}" >&2 || true
    echo "" >&2
    echo "    Common causes:" >&2
    echo "    - challenge expired (default TTL ~10 min); rerun the script" >&2
    echo "    - signature scheme mismatch (this script signs raw bytes — correct)" >&2
    echo "    - wallet already registered with a different bearer (use rotate)" >&2
    exit 1
fi

BEARER="$(jq -r '.token' "${REG_OUT}")"
SELLER_ID="$(jq -r '.id' "${REG_OUT}")"
[[ -n "${BEARER}" && "${BEARER}" != "null" ]] || {
    echo "❌ register response missing .token" >&2
    cat "${REG_OUT}" >&2
    exit 1
}

echo "" >&2
echo "✅ registered (seller_id=${SELLER_ID})" >&2
echo "   ⚠  the bearer below is shown EXACTLY ONCE." >&2
echo "      Save it now as REGISTRY_BEARER_TOKEN on the x402-buy-spl-token" >&2
echo "      Vercel project. The registry stores only SHA256(token); a lost" >&2
echo "      token requires POST /v1/registry/seller/rotate to re-issue." >&2
echo "" >&2

if [[ "${APPEND_NEWLINE}" == "1" ]]; then
    printf '%s\n' "${BEARER}"
else
    printf '%s' "${BEARER}"
fi

if [[ "${COPY_CLIPBOARD}" == "1" ]]; then
    if command -v pbcopy >/dev/null 2>&1; then
        printf '%s' "${BEARER}" | pbcopy
        echo "✅ copied to clipboard (macOS pbcopy)" >&2
    elif command -v xclip >/dev/null 2>&1; then
        printf '%s' "${BEARER}" | xclip -selection clipboard
        echo "✅ copied to clipboard (xclip)" >&2
    elif command -v clip.exe >/dev/null 2>&1; then
        printf '%s' "${BEARER}" | clip.exe
        echo "✅ copied to clipboard (clip.exe)" >&2
    else
        echo "⚠️  --copy requested but no clipboard tool found" >&2
    fi
fi
