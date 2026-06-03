# AGENTS.md

This file is for AI agents (Cursor, Claude Code, etc.), not human developers.
Philosophy: **Simple is Best, yet Elegant.** Make the smallest change that solves
the task; do not refactor, abstract, or add features that were not asked for.

`x402-buy-spl-token` is a **paid x402 seller using `sla-escrow`**: it delivers SPL tokens
against a USDC escrow with oracle-verified release, gating the route with `402 Payment
Required` and settling via a pr402 facilitator. It is a deployed production service —
read and extend it, don't grow it into a framework.

## Topology

Single Rust crate (`x402-buy-spl-token`), bin **`buy_spl_token_api`**, deployed on Vercel:

- `src/` — the paid endpoint **`GET /api/v1/buy-spl-token`** (scheme `sla-escrow`) + its
  `/intent-contract`.
- `storefront/` — frontend (`package.json`).
- `migrations/` — optional Postgres (idempotency + parameters).
- `scripts/` — operator helpers (`get-escrow-pda.sh`, `register-seller-with-registry.sh`, `test-buy-spl-token-devnet.sh`).

Request shape: `?token=<mint>&recipient_owner=<pubkey>&buyer_nonce=<64-hex>&quantity=N`.

## Hard boundaries (do not cross without explicit human approval)

- **Session totals are server-quoted.** Unit prices come from `BUY_SPL_TOKEN_CATALOG_JSON`;
  the server computes totals. Never trust a client-supplied total.
- **Two keys, deliberately separate.** The **delivery hot key** (`SELLER_KEYPAIR_BASE58`)
  signs SPL `TransferChecked` only; the **merchant payout identity**
  (`X402_MERCHANT_WALLET` / optional `X402_BENEFICIARY`, with `MERCHANT_SIGNER_KEYPAIR_BASE58`
  signing `SubmitDelivery`) is distinct. Don't merge them. `X402_PAY_TO` is the **escrow PDA**,
  not the merchant wallet — it must differ.
- **FundPayment TTL is bounded by the on-chain clock.** `X402_PAYMENT_TIMEOUT_SECONDS` MUST
  exceed `delivery_cutoff_seconds + delivery budget`. `SLA_ESCROW_PROGRAM_ID` must match the
  target cluster (devnet vs mainnet id) or `SubmitDelivery` fails.
- **Oracle + evidence are required for the paid path.** `ORACLE_AUTHORITIES` (allow-list) and
  `REGISTRY_BASE_URL` / `REGISTRY_BEARER_TOKEN` (evidence/SLA upload). Don't bypass them.
- **The live 402 advertises a canonical `resource.url` without request query params**; the
  pr402 discovery probe matches on origin+path (the SRM `resourceUrl` carries sample query
  args to reach the gate). Don't add a requirement that the 402 echo the query string.
- **Authoritative payment terms = the live HTTP 402.** `/.well-known/x402-resources.json` is
  advisory discovery metadata; its `resourceUrl` host must equal the service origin.
- **No new dependencies** unless asked.

## Verify before claiming done (fix, don't suppress)

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --bin buy_spl_token_api
```
