# x402-buy-spl-token

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

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/buy-spl-token` | Unpaid → 402 quote; paid → deliver SPL + SubmitDelivery |
| `GET` | `/api/v1/buy-spl-token/intent-contract` | Intent contract v0.3 (`delegated-authoring/v1`) |
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
