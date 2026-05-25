# Buy SPL Token — devnet end-to-end test

Operator runbook for a complete **v0.3 buyer-commit** purchase through
[`GET /api/v1/buy-spl-token`](../README.md).

The script [`scripts/test-buy-spl-token-devnet.sh`](../scripts/test-buy-spl-token-devnet.sh)
automates all steps below.

## Flow (buyer-commit)

```
Buyer                         Seller (402)                    pr402 facilitator
  │  GET ?token&quantity&...      │                                │
  │ ───────────────────────────► │ 402 + commitMaterial           │
  │                               │ (session totals, no slaHash)   │
  │  compose TransferSla locally  │                                │
  │  from commitMaterial + uid    │                                │
  │ ─────────────────────────────────────────────────────────────► │ build FundPayment
  │  sign FundPayment             │                                │
  │  GET + PAYMENT-SIGNATURE      │                                │
  │ ───────────────────────────► │ verify/settle → deliver SPL     │
  │ ◄─────────────────────────── │ 200 completed                  │
```

Unlike the legacy seller-commit variant, the unpaid 402 does **not** include
`slaHash` / `slaUrl`. The buyer authors the SLA from `extra.commitMaterial`
session fields and a chosen `payment_uid`.

## Prerequisites

### Tooling

- `solana` CLI (1.18+), `curl`, `jq`, `python3`, `openssl`, `cargo`
- pr402 checkout at `../pr402` (or set `PR402_ROOT`)

### Wallets

| Role | Purpose |
|------|---------|
| Buyer (`demo-wallets/buyer-keypair.json`) | Signs `FundPayment`; needs devnet SOL + USDC |
| Merchant (`X402_MERCHANT_WALLET`) | `FundPayment.seller` / `ReleasePayment` USDC recipient |
| Seller signer (`SELLER_KEYPAIR_BASE58`) | SPL delivery hot key (`commitMaterial.sellerPubkey`) |
| Merchant signer (`MERCHANT_SIGNER_KEYPAIR_BASE58`) | Signs `SubmitDelivery` (pubkey must match `beneficiary ?? merchantWallet`) |

These three on-chain roles must **not** be collapsed into one key in production.

### Buyer funding (devnet)

```bash
solana airdrop 1 -u devnet -k demo-wallets/buyer-keypair.json
# USDC: Circle devnet faucet or transfer from another wallet
```

Fund at least `accepts[].amount` raw USDC (plus a small ATA buffer).

### Seller deployment

Preview target: `https://preview.spl-token.hashspace.me`

The repo includes [`vercel.json`](../vercel.json) routing `/health`, `/api/v1/buy-spl-token`, and `/api/v1/buy-spl-token/intent-contract` to `src/bin/buy_spl_token_api.rs`. **Redeploy after pulling** — a bare Vercel project without these routes returns HTTP 404.

Legacy seller (seller-commit, **not** compatible with this script): `https://preview.spl-token.signer-payer.me`

Required env (see [`env.example`](../env.example)):

- `X402_PAY_TO` — escrow PDA (`accepts[].payTo`), must differ from merchant wallet
- `X402_MERCHANT_WALLET` — advertised as `extra.merchantWallet`
- `MERCHANT_SIGNER_KEYPAIR_BASE58`, `SELLER_KEYPAIR_BASE58`
- `REGISTRY_BASE_URL`, `REGISTRY_BEARER_TOKEN`
- `BUY_SPL_TOKEN_CATALOG_JSON` with the test mint

#### Registry bearer (paid path)

On the paid GET the seller uploads the SLA to the evidence registry **before** verify/settle. A missing or wrong bearer returns `502 registry_unavailable` with `bearer token not recognized`.

Register against the **devnet** oracle (wallet choice is independent of merchant/seller keys):

```bash
bash oracles/scripts/seller-register.sh \
  https://oracle.innoloyalty.com/devnet \
  /path/to/keypair.json \
  x402-buy-spl-token-preview
# stdout: BEARER=<token>
```

Set on Vercel (no `/v1/registry` suffix on the base URL):

```
REGISTRY_BASE_URL=https://oracle.innoloyalty.com/devnet
REGISTRY_BEARER_TOKEN=<BEARER from above>
```

Redeploy after updating. Mainnet preview needs `https://oracle.innoloyalty.com/mainnet` and a separate registration.

## Running

```bash
cd x402-buy-spl-token
./scripts/test-buy-spl-token-devnet.sh
```

Common overrides:

```bash
SELLER_BASE_URL=https://preview.spl-token.hashspace.me \
FACILITATOR_URL=https://preview.agent.pay402.me \
PR402_ROOT="$HOME/miraland-labs/x402/pr402" \
BUYER_KEYPAIR="$HOME/miraland-labs/x402/demo-wallets/buyer-keypair.json" \
TOKEN_MINT=5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP \
QUANTITY=1 \
RPC_URL=https://api.devnet.solana.com \
./scripts/test-buy-spl-token-devnet.sh
```

Vercel deployment protection:

```bash
VERCEL_BYPASS_TOKEN='your-bypass-token' ./scripts/test-buy-spl-token-devnet.sh
```

Skip slow steps during iteration:

```bash
SKIP_ONCHAIN_VERIFY=1 SKIP_RELEASE=1 ./scripts/test-buy-spl-token-devnet.sh
```

## Step-by-step (what the script does)

1. **402 probe** — validates `commitVariant=buyer-commit`, `commitMaterial.paymentAmountRaw == accepts[].amount`, and required fields.
2. **SLA hash** — SHA-256 of canonical JSON using `commitMaterial.deliverAmountRaw` as `expected_transfers[].min_amount`.
3. **Build FundPayment** — pr402 `build-sla-escrow-payment-tx` with buyer-composed `slaHash` and `paymentUidHex`.
4. **Sign** — `cargo run --example e2e_sign_sla_escrow_tx` in pr402.
5. **Paid GET** — same query params + `PAYMENT-SIGNATURE` header.
6. **Verify** — confirm SPL transfer + SubmitDelivery on RPC; fetch evidence registry document.
7. **Replay** — identical 200 with stored signatures (idempotency).
8. **ReleasePayment** (optional) — poll for oracle approval, release USDC to `beneficiary ?? merchantWallet`.

## Failure modes

| Symptom | Likely cause |
|---------|----------------|
| 402 `sla_hash_mismatch` | Buyer SLA bytes differ from seller recompute (wrong `deliverAmountRaw`, `payment_uid`, or query params changed between probe and paid GET) |
| 402 `settlement_failed` | pr402 rejected verify/settle (oracle not in allowlist, USDC balance, amount mismatch) |
| 502 `registry_unavailable` | `REGISTRY_BEARER_TOKEN` wrong or expired — re-run `seller-register.sh` for devnet, update Vercel, redeploy |
| 402 `payment_ttl_mismatch` | FundPayment TTL ≠ seller-quoted `maxTimeoutSeconds` (buyer tampering) |
| 402 `payment_ttl_too_short` | TTL below `delivery_cutoff + delivery_budget` — raise `X402_PAYMENT_TIMEOUT_SECONDS` (see `x402/sla-escrow-fund-payment-ttl/v1`) |
| 502 `transfer_failed` | Seller signer lacks SPL inventory or RPC error |
| 502 `submit_delivery_failed` | Merchant signer pubkey ≠ `FundPayment.seller` |
| Step 8 timeout | Oracle monitor has not posted `ConfirmOracle` yet — increase `RELEASE_POLL_DEADLINE_SEC` |

## v0.3 differences from legacy script

| Legacy (`spl-token-balance-serverless`) | This seller (v0.3.1) |
|----------------------------------------|-------------------------|
| 402 includes `extra.slaHash` | Buyer composes SLA from `commitMaterial` |
| SLA uses `tokenPriceUnits` | SLA uses session `deliverAmountRaw` |
| `merchantWallet` fallback to `payTo` | `merchantWallet` required; must ≠ escrow PDA |
| ReleasePayment to `sellerPubkey` | ReleasePayment to `beneficiary ?? merchantWallet` |
