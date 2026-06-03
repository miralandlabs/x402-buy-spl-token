# x402-buy-spl-token (by Hashspace · Miraland Labs)

Open-source **reference seller** for the **x402 sla-escrow** rail: buyers pay USDC into escrow and receive catalogued SPL tokens on oracle-verified delivery.

This is the ecosystem reference for **conditional delivery + fixed x402 offers** — payment and deliverable are configured independently at unit list, then **seller-quoted as session totals** so buyer agents never multiply prices client-side.

Implements informative binding `x402/informative/bindings/buy-spl-token/v1` (contract **v0.3.1**) atop Layer 0–1 normatives in [miraland-labs/oracles](https://github.com/miraland-labs/oracles).

## Why sla-escrow (our x402 differentiator)

| Flat x402 (exact) | sla-escrow (this seller) |
|-------------------|---------------------------|
| Pay → immediate resource | Pay → escrow → seller delivers → oracle verifies → release |
| Single amount, no SLA | Buyer commits to `sla_hash` before funding |
| No on-chain deliverable proof | `TransferChecked` + evidence registry + `SubmitDelivery` (merchant-signed) |

**v0.3** adds **`quantity`** without breaking x402: the 402 still advertises one authoritative `accepts[].amount` (session total), not a unit price.

## Catalog unit list (operator config)

Each row is **one unit** (quantity=1). Payment and deliverable axes are independent:

| Field | Meaning |
|-------|---------|
| `price_usdc_ui` | Unit USDC list price (× 10⁶ raw per unit) |
| `deliver_amount_ui` | Unit SPL deliverable (× 10^`decimals` raw per unit) |
| `decimals` | Mint decimals |

```json
{
  "mint": "5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP",
  "decimals": 6,
  "price_usdc_ui": "0.42",
  "deliver_amount_ui": "1000",
  "name": "merry-xmas"
}
```

## v0.3 — seller-quoted session totals (x402-compliant)

1. Buyer: `GET ...?token=<mint>&quantity=3&recipient_owner=...&buyer_nonce=...`
2. Seller returns **402** with:
   - `accepts[].amount` = **session USDC total** (e.g. `1260000` raw for 3 × 0.42 USDC)
   - `commitMaterial.paymentAmountRaw` = same total (must match)
   - `commitMaterial.deliverAmountRaw` = **session SPL total** (SLA + transfer)
   - `commitMaterial.quantity` = `3`
   - Unit echoes: `unitPaymentAmountRaw`, `unitDeliverAmountRaw`, `unitDeliverAmountUi`
3. Buyer agent:
   - Funds **exactly** `accepts[].amount` — **do not** compute unit × quantity
   - Builds SLA from **session** `deliverAmountRaw`
   - Signs `FundPayment`, retries paid GET with same query params

Default `quantity=1` when omitted.

Machine-readable rules: `GET /api/v1/buy-spl-token/intent-contract`

## Design

- **Simple is best, yet elegant** — one Axum binary, one buy endpoint, optional Postgres.
- **Commit variant:** `buyer-commit` — unpaid 402 returns `commitMaterial` (buyer authors SLA).
- **Facilitator:** [pr402](https://github.com/miralandlabs/pr402) (`X402_FACILITATOR_URL`).

## Required environment (no-DB)

Three **distinct** Solana roles — never collapse them:

| Variable | Role |
|----------|------|
| `X402_PAY_TO` | sla-escrow **escrow PDA** → `accepts[].payTo` |
| `X402_MERCHANT_WALLET` (or `MERCHANT_WALLET`) | **Merchant payout** → `extra.merchantWallet` / `ReleasePayment` (required; must ≠ `X402_PAY_TO`) |
| `X402_BENEFICIARY` (optional) | When set, **overrides** `merchantWallet` for `FundPayment.seller` (pr402 precedence) |
| `MERCHANT_SIGNER_KEYPAIR_BASE58` | Signs **`SubmitDelivery`**; pubkey must match `beneficiary ?? merchantWallet` |
| `SELLER_KEYPAIR_BASE58` | **Delivery hot key** → SPL `TransferChecked` only |

Also required at cold start: `X402_FACILITATOR_URL`, `ORACLE_AUTHORITIES`, `BUY_SPL_TOKEN_CATALOG_JSON`, `REGISTRY_BASE_URL`, `REGISTRY_BEARER_TOKEN`.

## Operator setup (Vercel)

Helper scripts under `scripts/` (run from the repo root):

| Script | Purpose |
|--------|---------|
| [`scripts/get-escrow-pda.sh`](scripts/get-escrow-pda.sh) | Resolve the USDC escrow PDA → set `X402_PAY_TO` (not your merchant wallet). |
| [`scripts/register-seller-with-registry.sh`](scripts/register-seller-with-registry.sh) | Registry challenge/register → one-time `REGISTRY_BEARER_TOKEN`. Sign with the wallet you register (typically the merchant signer keypair). |

```bash
# Escrow PDA for devnet preview facilitator + mainnet USDC mint
FACILITATOR_URL=https://preview.agent.pay402.me ./scripts/get-escrow-pda.sh --network devnet

# Registry bearer (example devnet registry; use your operator URL)
./scripts/register-seller-with-registry.sh \
  https://oracle.innoloyalty.com/devnet \
  /path/to/seller-keypair.json \
  --print-pubkey --copy
```

Alternative (same registry flow): [miraland-labs/oracles `scripts/seller-register.sh`](https://github.com/miraland-labs/oracles/blob/main/scripts/seller-register.sh).

## Quick start (no database)

```bash
cp env.example .env
# Set all required vars — especially X402_MERCHANT_WALLET (not optional)

cargo run
```

Unpaid request:

```http
GET /api/v1/buy-spl-token?token=<mint>&quantity=1&recipient_owner=<wallet>&buyer_nonce=<64-hex>
```

→ HTTP 402 with x402 v2 `Payment-Required` and seller quote in `commitMaterial`.

Paid request: retry with `PAYMENT-SIGNATURE` after signing `FundPayment` via pr402.

## Optional Postgres

Set `DATABASE_ENABLED=true` and `DATABASE_URL=...` for durable `purchase_orders` idempotency and `parameters` overrides.

## Devnet end-to-end test

```bash
./scripts/test-buy-spl-token-devnet.sh
```

See [`docs/BUY-SPL-TOKEN-DEVNET-TEST.md`](docs/BUY-SPL-TOKEN-DEVNET-TEST.md) for prerequisites, env overrides, and failure modes. Defaults target the preview seller at `https://preview.spl-token.hashspace.me` and pr402 at `https://preview.agent.pay402.me` (same deployment as `https://preview.ipay.sh`).

Deploy to Vercel with the included [`vercel.json`](vercel.json) (Rust entry: `src/bin/buy_spl_token_api.rs` + static `public/` from the storefront build).

## CI / CD (GitHub Actions → Vercel)

Pushes to any branch run [`.github/workflows/build-and-deploy.yml`](.github/workflows/build-and-deploy.yml):

1. **CI:** `cargo fmt`, `clippy -D warnings`, `cargo test`
2. **Storefront (once):** `npm run build:storefront` → `public/` (`index.html`, `assets/`, `wallet.js`). Not in `vercel.json` — GitHub Actions (or manual deploy) owns this step.
3. **Deploy:** `vercel pull` → `vercel build` (Rust only; static files from step 2) → `vercel deploy --prebuilt` (CLI pinned to `52.2.1`, same as pr402)

| Branch | Vercel environment | Typical custom domain |
|--------|-------------------|------------------------|
| `main` | production | `https://spl-token.hashspace.me` |
| other  | preview    | `https://preview.spl-token.hashspace.me` |

**Repository secrets** (GitHub → Settings → Secrets → Actions):

| Secret | Purpose |
|--------|---------|
| `VERCEL_TOKEN` | Vercel API token |
| `ORG_ID` | Vercel team/org id |
| `PROJECT_ID` | Vercel project id for this seller |

Solana / catalog env vars (`X402_NETWORK`, `BUY_SPL_TOKEN_CATALOG_JSON`, keypairs, etc.) stay in the **Vercel project** per environment (Preview vs Production), not in GitHub.

Manual deploy (without Actions): `npm run build:storefront && vercel build && vercel deploy --prebuilt`.

## Human storefront

A **Vite + TypeScript** shop at `/` proves x402 works for humans, not only agents. Emerald theme (fair-portal palette), wallet connect (Solana wallet-adapter), on-chain Metaplex metadata + seller ATA inventory, and the same **sla-escrow buyer-commit** flow as agents.

**Cluster-aware:** preview deployment (`preview.spl-token.hashspace.me`) uses Devnet env; production uses Mainnet. The UI reads `network`, `cluster`, and `facilitatorUrl` from `GET /api/v1/buy-spl-token/catalog` — never hardcoded.

### Build locally

```bash
# Terminal 1 — seller API
cargo run

# Terminal 2 — storefront dev (proxies /api to :8080)
cd storefront && npm install && npm run dev
```

See [`docs/HUMAN-STOREFRONT.md`](docs/HUMAN-STOREFRONT.md) for manual test checklist.

### Production build (local)

```bash
npm run build:storefront
# → writes public/ (index.html, assets/, wallet.js)
```

Optional: set `PUBLIC_RPC_URL` for browser metadata/inventory reads (otherwise facilitator `/health` → `solanaWalletRpcUrl`, then public cluster RPC).

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/buy-spl-token` | Unpaid → 402 quote; paid → deliver SPL + SubmitDelivery |
| `GET` | `/api/v1/buy-spl-token/catalog` | Public catalog + cluster context for human storefront |
| `GET` | `/api/v1/buy-spl-token/intent-contract` | Intent contract v0.3 (`delegated-authoring/v1`) |
| `GET` | `/` | Human storefront (static SPA, emerald UI) |
| `GET` | `/health` | Liveness |

## Normative alignment

| Topic | Spec identifier |
|-------|-----------------|
| Delegated authoring / 402 flow | `x402/delegated-authoring/v1` |
| Informative binding (this seller) | `x402/informative/bindings/buy-spl-token/v1` |
| SLA bytes + hash | `x402/sla-document/v1` + `x402/canonical-json/v1` |
| Delivery profile | `x402/oracles/onchain-transfer/v1` |
| pr402 extras | `x402/pr402-discovery/v1` |

## License

MIT OR Apache-2.0
