# x402-buy-spl-token

Open-source reference seller for the **x402 sla-escrow** rail: buyers pay USDC into escrow and receive a catalogued SPL token on delivery.

Implements informative binding `x402/informative/bindings/buy-spl-token/v1` atop Layer 0–1 normatives in the [miraland-labs/oracles](https://github.com/miraland-labs/oracles) spec tree (`x402/delegated-authoring/v1`, `x402/sla-document/v1`, …).

## Design

- **Simple is best, yet elegant** — one Axum binary, one endpoint, no Vercel/check-balance baggage.
- **Production-capable, lightweight** — runs with env-only config; Postgres is **optional**.
- **Commit variant:** `buyer-commit` — unpaid 402 returns `accepts[].extra.commitMaterial` (not authoritative `slaHash`).
- **Facilitator:** [pr402](https://github.com/miralandlabs/pr402) (`X402_FACILITATOR_URL`).

## Quick start (no database)

```bash
cp env.example .env
# edit .env — set RPC_URL, X402_*, SELLER_KEYPAIR_BASE58, BUY_SPL_TOKEN_CATALOG_JSON, ORACLE_AUTHORITIES

cargo run
```

Unpaid request:

```http
GET /api/v1/buy-spl-token?token=merry-xmas&recipient_owner=<buyer-wallet>&buyer_nonce=<64-hex-chars>
```

→ HTTP 402 with x402 v2 `Payment-Required` header and `commitMaterial` for SLA reconstruction.

Paid request: retry with `PAYMENT-SIGNATURE` after signing `FundPayment` via pr402.

## Optional Postgres

Set `DATABASE_ENABLED=true` and `DATABASE_URL=...` to enable:

- Durable `purchase_orders` idempotency ledger (survives restarts, multi-instance safe via advisory locks)
- `parameters` table overrides for `X402_PAY_TO`, catalog JSON, etc.

When disabled (default without `DATABASE_URL`), an in-memory ledger serializes concurrent retries per `payment_uid` — sufficient for solo sellers testing on devnet.

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/buy-spl-token` | Unpaid → 402; paid → deliver SPL + SubmitDelivery |
| `GET` | `/api/v1/buy-spl-token/intent-contract` | Machine-readable intent contract (`delegated-authoring/v1`) |
| `GET` | `/health` | Liveness |

## Normative alignment

| Topic | Spec identifier |
|-------|-----------------|
| Delegated authoring / 402 flow | `x402/delegated-authoring/v1` |
| Informative binding (this seller) | `x402/informative/bindings/buy-spl-token/v1` |
| SLA bytes + hash | `x402/sla-document/v1` + `x402/serialization-recipes/v1` (`x402/canonical-json/v1`) |
| Delivery profile | `x402/oracles/onchain-transfer/v1` |
| pr402 extras | `x402/pr402-discovery/v1` |

Normative documents live in [miraland-labs/oracles](https://github.com/miraland-labs/oracles/tree/main/spec) — not in this repository.

## License

MIT OR Apache-2.0
