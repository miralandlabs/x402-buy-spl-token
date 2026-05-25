# Human storefront — manual test checklist

## Preview (Devnet)

Host: `https://preview.spl-token.hashspace.me` (or local `cargo run` + `storefront` dev server).

1. Open `/` — emerald shop loads; header shows **Devnet** pill.
2. `GET /api/v1/buy-spl-token/catalog` returns `cluster: "devnet"`, devnet `usdcMint`, preview facilitator URL.
3. Token cards show Metaplex image/name (or monogram fallback) and seller ATA **In stock** balance.
4. Connect Phantom/Solflare on **Devnet** with devnet USDC.
5. Select token → quantity → **Get quote & pay** → confirm modal shows session USDC total from 402 (not client math).
6. Sign FundPayment in wallet → paid GET returns `200` with `status: "completed"`, transfer + delivery signatures.
7. Solscan links use `?cluster=devnet`.

## Production (Mainnet)

Host: production seller URL (no `preview.` prefix).

1. Header shows **Mainnet** pill; catalog `cluster: "mainnet-beta"`.
2. Wallet on Mainnet with real USDC — smoke one small catalog purchase before release.

## Build verification

```bash
cd storefront && npm run build
cargo clippy -- -D warnings
cargo test catalog_document
```
