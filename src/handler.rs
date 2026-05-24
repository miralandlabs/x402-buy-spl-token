//! Buy SPL Token endpoint (`GET /api/v1/buy-spl-token`).
//!
//! This module wires the unpaid path of the buy endpoint:
//!
//! - **Unpaid GET** (no `PAYMENT-SIGNATURE` header): returns HTTP 402 with a
//!   server-built TransferSla committed to a registry. The `accepts[].extra`
//!   carries `slaHash` / `slaUrl`, and the `amount` is the buyer's USDC
//!   payment in raw units (seller-quoted session total from [`crate::quote`]).
//! - **Paid GET** (with `PAYMENT-SIGNATURE` header): not yet implemented in
//!   this task (8.1). Subsequent tasks (8.2 → 8.4) fill in
//!   verify-and-settle, SPL `TransferChecked`, evidence upload, and
//!   `SubmitDelivery`. For now, the paid branch returns 501 so the route is
//!   reachable from tests but never silently does the wrong thing.
//!
//! # Response envelope
//!
//! Both 402 and error responses reuse the response envelope, CORS headers,
//! and `X-API-Version: 1` header established by the existing `check_balance`
//! handler (see [`crate::api::handlers`]). Error bodies follow the
//! `{ "error": { "code", "message" } }` shape required by Requirement 9.1.
//!
//! # Payment vs deliverable (v0.3 — seller-quoted session totals)
//!
//! Catalog rows are **unit list** prices. Optional query `quantity` (default
//! `1`) selects how many units the buyer wants this session. The seller
//! scales server-side into fixed session totals:
//!
//! - **x402 payment:** `accepts[].amount` = session USDC raw (authoritative;
//!   buyer MUST NOT compute unit × quantity client-side).
//! - **SLA + transfer:** `commitMaterial.deliverAmountRaw` = session SPL raw.
//!
//! See [`crate::intent_contract`] and [`crate::quote`] for the ecosystem
//! reference binding.

use {
    crate::{
        catalog::CatalogEntry,
        cors::ALLOW_HEADERS,
        orders::{LedgerError, OrderRecord, OrderState, TransitionFields},
        purchase_ledger::PurchaseLedger,
        quote::{self, SessionQuote},
        registry_client::{
            AssertedTransfer, RegistryClient, RegistryClientError, TransferEvidenceBuilder,
            REGISTRY_BASE_URL, REGISTRY_BEARER_TOKEN,
        },
        rpc_retry::{with_retry, RetryPolicy},
        sla_builder::{self, BuiltSla, SlaBuilderError, TransferSlaInputs, SLA_VERSION},
        x402::facilitator::parse_payment_proof,
        AppState,
    },
    base64::{engine::general_purpose::STANDARD, Engine},
    chrono::Utc,
    http::HeaderMap,
    serde::Deserialize,
    serde_json::{json, Value},
    sha2::{Digest, Sha256},
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
        signature::Signature,
        transaction::{Transaction, VersionedTransaction},
    },
    std::{str::FromStr, sync::Arc, time::Duration},
    tracing::{info, warn},
    vercel_runtime::{Body, Response, StatusCode as VercelStatusCode},
};

/// USDC mint (devnet) — used as the `asset` of the 402 `accepts[]` line
/// when the request is served against a devnet RPC. Mainnet uses
/// [`USDC_MAINNET_MINT`].
const USDC_DEVNET_MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

/// USDC mint (mainnet).
const USDC_MAINNET_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// SLA-escrow x402 scheme identifier the 402 line advertises. Matches the
/// constant pr402's facilitator advertises in `/supported`.
const SLA_ESCROW_SCHEME: &str = "sla-escrow";

/// Parsed query string for `GET /api/v1/buy-spl-token`. All fields are
/// `Option<String>` so we can distinguish "missing" (`None`) from
/// "malformed" (`Some(_)` that fails subsequent validation).
#[derive(Debug, Default, Deserialize)]
struct BuyQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    recipient_owner: Option<String>,
    #[serde(default)]
    buyer_nonce: Option<String>,
    /// Line quantity (default 1). Seller quotes session totals in the 402.
    #[serde(default)]
    quantity: Option<String>,
}

/// Entry point for `GET /api/v1/buy-spl-token`. Detects the unpaid vs paid
/// branch by the presence of the `PAYMENT-SIGNATURE` header (case-insensitive
/// per the existing `check_balance` handler) and dispatches accordingly.
pub async fn handle(
    headers: &HeaderMap,
    _path: &str,
    query: &str,
    state: Arc<AppState>,
) -> Response<Body> {
    info!("Entering buy_spl_token");

    // 1. Parse query parameters (no validation yet — that comes next).
    let params: BuyQuery = match serde_qs::from_str(query) {
        Ok(p) => p,
        Err(e) => {
            return error_response(
                headers,
                VercelStatusCode::BAD_REQUEST,
                "invalid_query",
                format!("invalid query string: {}", e),
            );
        }
    };

    // 2. Validate required parameters and extract concrete values. Returns
    //    early with 400 for missing/malformed params and 404 for unknown
    //    `token`.
    let parsed = match validate_request(&params, state.as_ref()) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };

    // 3. Branch on the payment header.
    if let Some(raw) = extract_payment_header(headers) {
        // Paid path — verify, settle, and idempotency setup (task 8.2).
        // The actual SPL transfer + evidence + SubmitDelivery still come
        // from tasks 8.3 / 8.4. After verify_and_settle succeeds, this
        // helper currently returns 501 with a TODO marker.
        return handle_paid_path(headers, state.as_ref(), &parsed, &raw).await;
    }

    // 4. Unpaid path → 402 with server-built SLA.
    build_unpaid_402(headers, state.as_ref(), &parsed).await
}

// ---------------------------------------------------------------------------
// Unpaid 402 builder
// ---------------------------------------------------------------------------

/// Concrete, validated request inputs derived from the query string and the
/// catalog lookup. Lives only on the request stack — never persisted.
struct ParsedRequest<'a> {
    entry: &'a CatalogEntry,
    quote: SessionQuote,
    recipient_owner: String,
    buyer_nonce: String,
}

async fn build_unpaid_402(
    headers: &HeaderMap,
    state: &AppState,
    parsed: &ParsedRequest<'_>,
) -> Response<Body> {
    let entry = parsed.entry;
    let quote = &parsed.quote;

    // (a) Session USDC total for the 402 line — seller-quoted, x402-authoritative.
    let usdc_amount_raw = quote.payment_amount_raw;
    let deliver_amount_raw = quote.deliver_amount_raw;

    // (c) Seller pubkey. The buy endpoint requires a cold-started signer.
    let seller_signer = match state.seller_signer.as_ref() {
        Some(s) => s.clone(),
        None => {
            return error_response(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "buy endpoint not initialized (seller signer missing)",
            );
        }
    };

    // (d) The SLA is **buyer-authored** under the
    // `x402/oracles/onchain-transfer/v1` profile: the buyer chooses
    // `payment_uid` (and `buyer_nonce`), composes the canonical SLA
    // bytes, hashes them locally, and signs the FundPayment with that
    // hash. The seller does NOT compute or upload the SLA on this path
    // because we do not yet know `payment_uid`.
    //
    // What we DO advertise here is everything the buyer needs to
    // reconstruct the SLA byte-identically:
    //   - `tokenMint` / `tokenDecimals` / `deliverAmountRaw` / `deliverAmountUi`
    //   - `recipientOwner` (echoed from the URL)
    //   - `buyerNonce` (echoed from the URL)
    //   - `sellerPubkey` (this deployment's hot signer)
    //   - `cluster` (kebab-case Solana cluster name expected by the
    //                oracle's `TransferCluster` enum)
    //   - `profileId` / `version`
    //
    // After the buyer signs FundPayment, the paid path
    // (`handle_paid_path`) extracts `payment_uid` from the instruction,
    // rebuilds the same canonical SLA, recomputes the hash, asserts it
    // matches the on-chain `payment.sla_hash`, then uploads the
    // canonical bytes to the registry.

    // (f) Build the 402 PaymentRequired body.
    //
    // X402_PAY_TO is per-rail: under sla-escrow it must be the per-asset
    // **escrow PDA** (derived from `(program_id, USDC_mint, bank_pda)`),
    // which is different from the **SplitVault PDA** the `check-balance`
    // (exact) endpoint uses. A single Vercel deployment cannot encode two
    // values in one env var, so this handler resolves through the
    // `parameters` table first (endpoint-specific row → wildcard row →
    // env). Operators set the buy-spl-token row to the escrow PDA and
    // leave the wildcard row pointing at the SplitVault for check-balance.
    let resource_url =
        state
            .config
            .x402_resource_url_for_request(headers, "/api/v1/buy-spl-token", "");
    let pay_to = match crate::parameters::resolve_pay_to(
        state.db.as_deref(),
        crate::parameters::ENDPOINT_BUY_SPL_TOKEN,
    )
    .await
    {
        Some(v) => v,
        None => {
            return error_response(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "X402_PAY_TO not set for buy-spl-token (sla-escrow rail expects the per-asset escrow PDA)",
            );
        }
    };
    let network = crate::parameters::resolve_network(
        state.db.as_deref(),
        crate::parameters::ENDPOINT_BUY_SPL_TOKEN,
    )
    .await
    .unwrap_or_else(|| state.config.x402_network.clone());
    let usdc_mint = usdc_mint_for_network(&network);
    let timeout_seconds = crate::parameters::resolve_timeout_sec(
        state.db.as_deref(),
        crate::parameters::ENDPOINT_BUY_SPL_TOKEN,
        state.config.x402_timeout_sec,
    )
    .await;

    // pr402's `build-sla-escrow-payment-tx` resolves `FundPayment.seller`
    // from `extra.beneficiary` (preferred) or `extra.merchantWallet`. That
    // pubkey is what receives the USDC on `ReleasePayment` — the merchant
    // collection wallet, distinct from `pay_to` (escrow PDA) and from
    // `seller_signer` (hot delivery key). `merchant_signer` signs
    // `SubmitDelivery` and must match fund-payment payout identity.
    let merchant_wallet = match crate::parameters::resolve_merchant_wallet(
        state.db.as_deref(),
        crate::parameters::ENDPOINT_BUY_SPL_TOKEN,
    )
    .await
    .filter(|s| !s.is_empty())
    {
        Some(w) => w,
        None => {
            return error_response(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "X402_MERCHANT_WALLET / MERCHANT_WALLET not configured (parameters table or env)",
            );
        }
    };

    let beneficiary = crate::parameters::resolve_beneficiary(
        state.db.as_deref(),
        crate::parameters::ENDPOINT_BUY_SPL_TOKEN,
    )
    .await
    .filter(|s| !s.is_empty());

    // Oracle allow-list emitted into `accepted.extra.oracleAuthorities`.
    // Accepts either a JSON array (`["pk1","pk2"]`) or a comma/whitespace-
    // separated list. Empty / unset → require the operator to fix the
    // parameters row before serving 402; pr402 rejects builds without it.
    let oracle_authorities = match crate::parameters::resolve_string(
        state.db.as_deref(),
        crate::parameters::ENDPOINT_BUY_SPL_TOKEN,
        crate::parameters::ORACLE_AUTHORITIES,
        Some(crate::parameters::ORACLE_AUTHORITIES),
    )
    .await
    {
        Some(raw) => crate::parameters::parse_oracle_authorities(&raw),
        None => Vec::new(),
    };
    if oracle_authorities.is_empty() {
        return error_response(
            headers,
            VercelStatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "ORACLE_AUTHORITIES not set (parameters table or env). pr402 requires `accepted.extra.oracleAuthorities` for sla-escrow builds.",
        );
    }

    // Pull the canonical sla-escrow extra from the facilitator's
    // /supported endpoint and use it as the base for our 402 envelope.
    // This frees us from hardcoding pr402's required field set
    // (`feePayer`, `bankAddress`, `configAddress`, `feeBps`,
    // `oracleFeeBps`, `ttlSeconds`, `maxComputeUnitLimit`,
    // `recommendedComputeUnitPrice`, `slaFundTxNetworkFeePayer`, …)
    // which evolves alongside the facilitator's verify path. Failure
    // here is treated as a misconfiguration: the seller cannot serve
    // 402 without these fields.
    let mut base_extra = match facilitator_sla_escrow_extra(&state.facilitator, &network).await {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!(
                    "facilitator /supported did not return an sla-escrow kind for network {}: {}",
                    network, e
                ),
            );
        }
    };
    // Overlay seller-specific values. Order matters: seller fields win
    // (e.g. `oracleAuthorities` from our parameters table overrides
    // pr402's preview list; `merchantWallet` is the identity for THIS
    // resource).
    let cluster_str = cluster_name_for_network(&network);
    let commit_material = json!({
        "quoteVersion": crate::intent_contract::QUOTE_VERSION,
        "quantity": quote.quantity,
        "tokenMint": entry.mint,
        "tokenName": entry.name,
        "tokenDecimals": entry.decimals,
        "unitPaymentAmountRaw": quote.unit_payment_raw.to_string(),
        "unitDeliverAmountRaw": quote.unit_deliver_raw.to_string(),
        "unitDeliverAmountUi": entry.deliver_amount_ui,
        "paymentAmountRaw": usdc_amount_raw.to_string(),
        "deliverAmountUi": quote.deliver_amount_ui,
        "deliverAmountRaw": deliver_amount_raw.to_string(),
        "recipientOwner": parsed.recipient_owner,
        "buyerNonce": parsed.buyer_nonce,
        "sellerPubkey": seller_signer.pubkey().to_string(),
        "cluster": cluster_str,
        "profileId": sla_builder::PROFILE_ID,
        "version": SLA_VERSION,
    });
    let mut overlay = json!({
        "merchantWallet": merchant_wallet,
        "oracleAuthorities": oracle_authorities,
        "intentContractUrl": crate::intent_contract::intent_contract_url_from_resource(&resource_url),
        "commitVariant": crate::intent_contract::COMMIT_VARIANT,
        "serializationRecipeId": crate::intent_contract::SERIALIZATION_RECIPE_ID,
        "profileId": sla_builder::PROFILE_ID,
        "commitMaterial": commit_material,
    });
    if let Some(b) = beneficiary {
        overlay
            .as_object_mut()
            .expect("overlay is object")
            .insert("beneficiary".into(), json!(b));
    }
    if let (Some(b), Some(o)) = (base_extra.as_object_mut(), overlay.as_object()) {
        for (k, v) in o {
            b.insert(k.clone(), v.clone());
        }
    }

    let accepts_line = json!({
        "scheme": SLA_ESCROW_SCHEME,
        "network": network,
        "asset": usdc_mint,
        "amount": usdc_amount_raw.to_string(),
        "payTo": pay_to,
        "maxTimeoutSeconds": timeout_seconds,
        "extra": base_extra,
    });

    let body = json!({
        "x402Version": 2,
        "error": "PAYMENT-SIGNATURE header is required (x402 v2 payment proof)",
        "resource": {
            "url": resource_url,
            "description": "Buy SPL Token (x402 v2 / SLA-Escrow rail)",
            "mimeType": "application/json",
        },
        "accepts": [accepts_line],
        "extensions": {},
    });
    let body_str = body.to_string();

    // x402 v2: the same body is mirrored as a base64-encoded
    // `Payment-Required` header so clients that only parse headers still
    // see it.
    let payment_required_header = STANDARD.encode(body_str.as_bytes());

    let date = Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let mut builder = Response::builder()
        .status(VercelStatusCode::PAYMENT_REQUIRED)
        .header("Content-Type", "application/json")
        .header("Payment-Required", payment_required_header)
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, OPTIONS")
        .header("Access-Control-Allow-Headers", ALLOW_HEADERS)
        .header("X-API-Version", "1")
        .header("X-Date", date);

    if let Some(cid) = headers.get("X-Correlation-ID") {
        builder = builder.header("X-Correlation-ID", cid);
    }

    builder
        .body(Body::Text(body_str))
        .unwrap_or_else(|_| Response::builder().status(500).body(Body::Empty).unwrap())
}

// ---------------------------------------------------------------------------
// Paid path (task 8.2)
// ---------------------------------------------------------------------------

/// Default budget for `with_advisory_lock` when the request has no other
/// upper bound. Picked to be small relative to the Vercel route timeout but
/// long enough to absorb the typical settle round-trip a sibling retry is
/// holding the lock for. The value matches the spec note: "Use
/// `Duration::from_secs(5)` if no existing config field is suitable".
const ADVISORY_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Constants for decoding the SLA-Escrow `FundPayment` instruction.
///
/// The on-chain instruction layout is `[discriminator(1) || seller(32) ||
/// mint(32) || oracle_authority(32) || payment_uid(32) || sla_hash(32) ||
/// amount(8) || ttl_seconds(8)]` for a total of 177 bytes
/// (see `pr402::chain::solana_sla_escrow::FundPaymentData`). The pr402
/// builder also emits exactly two ComputeBudget instructions before the
/// FundPayment, so the FundPayment is always the *last* instruction in the
/// transaction. We rely on that ordering rather than identifying the
/// program-id, since matching by program-id requires the SLA-Escrow program
/// pubkey to be known to this crate (it is not — pr402 owns it).
const FUND_PAYMENT_DISCRIMINATOR: u8 = 0;
/// Offset of `payment_uid` within the FundPayment instruction's data
/// (after the 1-byte discriminator).
const FUND_PAYMENT_UID_OFFSET: usize = 1 + 32 + 32 + 32; // 97
/// Offset of `sla_hash` within the FundPayment instruction's data.
const FUND_PAYMENT_SLA_HASH_OFFSET: usize = FUND_PAYMENT_UID_OFFSET + 32; // 129
/// Total length of FundPayment instruction data: 1-byte discriminator +
/// 176-byte body.
const FUND_PAYMENT_DATA_LEN: usize = 1 + 176;

/// Extracted `payment_uid` and `sla_hash` from a buyer-submitted
/// SLA-Escrow `FundPayment` instruction. Both are the raw 32-byte arrays
/// pulled verbatim from instruction data.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FundPaymentRefs {
    payment_uid: [u8; 32],
    sla_hash: [u8; 32],
}

impl FundPaymentRefs {
    /// Lowercase hex of `payment_uid`. Used as the canonical primary key
    /// in `purchase_orders` and as the input to `orders::hash64` for the
    /// advisory lock. Hex (not raw bytes) lets us key a TEXT column
    /// without worrying about embedded NULs from `sanitize_uid`'s
    /// zero-padding.
    fn payment_uid_hex(&self) -> String {
        hex::encode(self.payment_uid)
    }

    /// Lowercase hex of `sla_hash`. Compared against
    /// [`BuiltSla::sla_hash_hex`] for the SLA-hash binding check.
    fn sla_hash_hex(&self) -> String {
        hex::encode(self.sla_hash)
    }
}

/// Errors surfaced by [`extract_fund_payment_refs`]. Each variant carries
/// enough context for an operator (or a fuzz harness) to reproduce the
/// failing input.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FundPaymentParseError {
    MissingTransactionField,
    InvalidBase64(String),
    InvalidBincode(String),
    NoFundPaymentInstruction,
    WrongDataLength { expected: usize, actual: usize },
    WrongDiscriminator { actual: u8 },
}

impl std::fmt::Display for FundPaymentParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTransactionField => f.write_str(
                "paymentPayload.payload.transaction is missing or not a string",
            ),
            Self::InvalidBase64(e) => write!(
                f,
                "paymentPayload.payload.transaction is not valid base64: {}",
                e
            ),
            Self::InvalidBincode(e) => write!(
                f,
                "paymentPayload.payload.transaction did not deserialize as VersionedTransaction: {}",
                e
            ),
            Self::NoFundPaymentInstruction => f.write_str(
                "no FundPayment instruction found in the buyer's transaction",
            ),
            Self::WrongDataLength { expected, actual } => write!(
                f,
                "FundPayment instruction data has wrong length: expected {}, got {}",
                expected, actual
            ),
            Self::WrongDiscriminator { actual } => write!(
                f,
                "FundPayment discriminator mismatch: expected {}, got {}",
                FUND_PAYMENT_DISCRIMINATOR, actual
            ),
        }
    }
}

/// Decode the buyer's SLA-Escrow `FundPayment` instruction from a parsed
/// `PAYMENT-SIGNATURE` body and pull out the two fields we need at the
/// HTTP layer:
/// - `payment_uid` for advisory-lock keying / Order_Ledger primary key.
/// - `sla_hash` for the SLA-hash mismatch check.
///
/// We deliberately do not re-validate seller / mint / amount here: those
/// are pr402's job during `verify` and we'll re-invoke it at step 5.
/// What we *do* need pre-settlement is the SLA-hash binding (req 4.3),
/// which means decoding the instruction data ourselves.
fn extract_fund_payment_refs(proof: &Value) -> Result<FundPaymentRefs, FundPaymentParseError> {
    let tx_b64 = proof
        .pointer("/paymentPayload/payload/transaction")
        .and_then(|v| v.as_str())
        .ok_or(FundPaymentParseError::MissingTransactionField)?;
    let bytes = STANDARD
        .decode(tx_b64)
        .map_err(|e| FundPaymentParseError::InvalidBase64(e.to_string()))?;
    let vtx: VersionedTransaction = bincode::deserialize(&bytes)
        .map_err(|e| FundPaymentParseError::InvalidBincode(e.to_string()))?;

    // pr402's build-sla-escrow-payment-tx always emits the FundPayment
    // instruction last — preceded by two ComputeBudget instructions and
    // optionally one create-ATA — so scanning back-to-front is robust.
    // Pick the last instruction whose data starts with the FundPayment
    // discriminator and whose length matches the FundPayment layout.
    let instructions = vtx.message.instructions();
    let candidate = instructions
        .iter()
        .rev()
        .find(|ix| {
            ix.data.first() == Some(&FUND_PAYMENT_DISCRIMINATOR)
                && ix.data.len() == FUND_PAYMENT_DATA_LEN
        })
        .ok_or(FundPaymentParseError::NoFundPaymentInstruction)?;

    if candidate.data.len() != FUND_PAYMENT_DATA_LEN {
        return Err(FundPaymentParseError::WrongDataLength {
            expected: FUND_PAYMENT_DATA_LEN,
            actual: candidate.data.len(),
        });
    }
    let disc = candidate.data[0];
    if disc != FUND_PAYMENT_DISCRIMINATOR {
        return Err(FundPaymentParseError::WrongDiscriminator { actual: disc });
    }

    let mut payment_uid = [0u8; 32];
    payment_uid
        .copy_from_slice(&candidate.data[FUND_PAYMENT_UID_OFFSET..FUND_PAYMENT_UID_OFFSET + 32]);
    let mut sla_hash = [0u8; 32];
    sla_hash.copy_from_slice(
        &candidate.data[FUND_PAYMENT_SLA_HASH_OFFSET..FUND_PAYMENT_SLA_HASH_OFFSET + 32],
    );

    Ok(FundPaymentRefs {
        payment_uid,
        sla_hash,
    })
}

/// Handler for the **paid** branch: parse + validate proof, recompute SLA
/// hash, acquire the advisory lock, set up the ledger row, and call
/// `verify_and_settle`. Tasks 8.3 / 8.4 will replace the 501 placeholder
/// with the SPL transfer / evidence / `SubmitDelivery` chain.
async fn handle_paid_path(
    headers: &HeaderMap,
    state: &AppState,
    parsed: &ParsedRequest<'_>,
    raw_payment_header: &str,
) -> Response<Body> {
    // (1) Parse the payment header and extract the FundPayment refs we
    //     need before settlement (`payment_uid` for idempotency keying,
    //     `sla_hash` for the binding check). Reject 402 on parse failure
    //     so the buyer can fix the proof and retry.
    let proof = match parse_payment_proof(raw_payment_header) {
        Ok(p) => p,
        Err(e) => {
            return error_response(
                headers,
                VercelStatusCode::PAYMENT_REQUIRED,
                "invalid_payment_proof",
                format!("invalid PAYMENT-SIGNATURE: {}", e),
            );
        }
    };
    let refs = match extract_fund_payment_refs(&proof.0) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                headers,
                VercelStatusCode::PAYMENT_REQUIRED,
                "invalid_payment_proof",
                format!("could not parse FundPayment from payment proof: {}", e),
            );
        }
    };

    // (2) Recompute the canonical SLA hash from the request's query
    //     parameters and compare it to the hash committed by the
    //     FundPayment. Mismatch must be caught **before** settlement
    //     (req 4.3) — otherwise the buyer's USDC would be moved to escrow
    //     against an SLA the seller never signed.
    let deliver_amount_raw = parsed.quote.deliver_amount_raw;
    let seller_signer = match state.seller_signer.as_ref() {
        Some(s) => s.clone(),
        None => {
            return error_response(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "buy endpoint not initialized (seller signer missing)",
            );
        }
    };
    let sla_inputs = TransferSlaInputs {
        mint: parsed.entry.mint.clone(),
        decimals: parsed.entry.decimals,
        deliver_amount_raw,
        recipient_owner: parsed.recipient_owner.clone(),
        buyer_nonce: parsed.buyer_nonce.clone(),
        // Bind the recomputed SLA to the on-chain `Payment` the buyer
        // signed. The buyer authored the SLA with this same uid; we
        // pull it back out of the FundPayment instruction so the bytes
        // are reproduced verbatim and the hash matches.
        payment_uid: refs.payment_uid_hex(),
        cluster: cluster_name_for_network(&state.config.x402_network).to_string(),
        seller_pubkey: seller_signer.pubkey().to_string(),
        deadline_unix: None,
        version: SLA_VERSION,
    };
    let built: BuiltSla = match sla_builder::build(&sla_inputs) {
        Ok(b) => b,
        Err(e) => return map_sla_builder_error_to_500(headers, e),
    };

    let computed_hex = built.sla_hash_hex();
    let claimed_hex = refs.sla_hash_hex();
    if computed_hex != claimed_hex {
        return error_response_with_details(
            headers,
            VercelStatusCode::PAYMENT_REQUIRED,
            "sla_hash_mismatch",
            "submitted FundPayment.slaHash does not match the SLA recomputed from the request's query parameters",
            json!({
                "expected": computed_hex,
                "submitted": claimed_hex,
            }),
        );
    }

    // Hash matched — upload the canonical bytes to the registry so the
    // oracle can fetch them by hash. Idempotent on the registry side
    // (same bytes → same `sha256` → same path), so a replay of the same
    // paid GET is safe.
    let registry = match make_registry_client() {
        Ok(c) => c,
        Err(msg) => {
            return error_response(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "registry_unavailable",
                msg,
            );
        }
    };
    if let Err(e) = registry.upload_sla(&built.canonical_json).await {
        return map_registry_error_to_response(headers, "upload_sla", e);
    }

    // (3) Advisory lock + ledger (Postgres or in-memory).
    let ledger = state.ledger.clone();
    let payment_uid_hex = refs.payment_uid_hex();

    // (4) + (5) Run inside the advisory lock so two concurrent retries
    //     for the same payment_uid serialize. The closure does the
    //     ledger setup, the resume-from-state branching, and (when the
    //     row is fresh) the actual `verify_and_settle` call.
    let ctx = PaidLockCtx {
        headers,
        state,
        ledger: &ledger,
        payment_uid_hex: &payment_uid_hex,
        payment_uid_raw: &refs.payment_uid,
        parsed,
        built: &built,
        proof_body: &proof.0,
    };
    let lock_outcome = ledger
        .with_advisory_lock(&payment_uid_hex, ADVISORY_LOCK_TIMEOUT, || {
            run_paid_under_lock(&ctx)
        })
        .await;

    match lock_outcome {
        Ok(resp) => resp,
        Err(LedgerError::LockBusy { payment_uid }) => {
            warn!(
                target: "server_log",
                payment_uid = %payment_uid,
                "advisory lock busy past timeout for buy-spl-token request"
            );
            error_response(
                headers,
                VercelStatusCode::CONFLICT,
                "concurrent_request",
                "another request for the same payment_uid is in progress; retry after the in-flight request completes",
            )
        }
        Err(e) => {
            warn!(
                target: "server_log",
                error = %e,
                "ledger error during buy-spl-token paid path"
            );
            error_response(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("ledger error: {}", e),
            )
        }
    }
}

/// Context shared across [`run_paid_under_lock`] and its inner step
/// helpers. Every field is a borrow so the struct itself never owns
/// allocation; it exists purely to satisfy clippy's
/// `too_many_arguments` lint while keeping the call-graph readable.
struct PaidLockCtx<'a> {
    headers: &'a HeaderMap,
    state: &'a AppState,
    ledger: &'a PurchaseLedger,
    payment_uid_hex: &'a str,
    payment_uid_raw: &'a [u8; 32],
    parsed: &'a ParsedRequest<'a>,
    built: &'a BuiltSla,
    proof_body: &'a Value,
}

/// Body of the advisory-locked critical section. Returns a fully-formed
/// `Response<Body>` wrapped in `Result<_, LedgerError>` so the outer
/// `with_advisory_lock` can surface infrastructure failures while
/// HTTP-shaped outcomes (200 / 402 / 409 / 501) flow through `Ok`.
async fn run_paid_under_lock(ctx: &PaidLockCtx<'_>) -> Result<Response<Body>, LedgerError> {
    let ledger = ctx.ledger;
    let headers = ctx.headers;
    let state = ctx.state;
    let payment_uid_hex = ctx.payment_uid_hex;
    let payment_uid_raw = ctx.payment_uid_raw;
    let parsed = ctx.parsed;
    let built = ctx.built;
    let proof_body = ctx.proof_body;
    ledger.insert_pending(payment_uid_hex).await?;
    let record = ledger.load(payment_uid_hex).await?.ok_or_else(|| {
        // insert_pending succeeded above, so the row must exist; if it
        // doesn't we've raced against a concurrent DELETE which is not a
        // path this service ever exercises.
        LedgerError::NotFound {
            payment_uid: payment_uid_hex.to_string(),
        }
    })?;

    match record.state {
        OrderState::Completed => {
            // Idempotent replay (req 5.4): return the stored signatures
            // verbatim, no new on-chain action.
            info!(
                payment_uid = %payment_uid_hex,
                "buy-spl-token replay returns stored completed-order signatures"
            );
            Ok(completed_response(headers, &record, &built.sla_hash_hex()))
        }
        OrderState::Failed => {
            // Terminal failure (req 5.7): refuse to retry.
            Ok(error_response_with_details(
                headers,
                VercelStatusCode::CONFLICT,
                "order_failed",
                "purchase order is in a terminal failed state and cannot be replayed",
                json!({
                    "payment_uid": payment_uid_hex,
                    "state": record.state.as_str(),
                }),
            ))
        }
        OrderState::TransferLanded
        | OrderState::DeliverySubmitted
        | OrderState::PendingTransfer => {
            // Mid-flight or fresh: proceed with verify_and_settle, then
            // (when the row is in `pending_transfer`) sign and submit
            // the SPL `TransferChecked` to advance the row to
            // `transfer_landed`. Tasks 8.4 fills in the evidence upload
            // and `SubmitDelivery` step.
            //
            // For `transfer_landed` / `delivery_submitted` the row
            // already carries earlier signatures from a partially-
            // completed sibling — we replay verify+settle (which is a
            // no-op on the duplicate path) and skip straight to the 8.4
            // territory below.
            //
            // Note: settling a payment whose row is in TransferLanded
            // means the original FundPayment was already verified-and-
            // settled by an earlier request. pr402's settle handler
            // detects "already processed" and returns a synthetic
            // success (see `is_duplicate_settle_body` in `facilitator`),
            // so this is safe to retry at this layer.
            let settle_resp = run_verify_and_settle(headers, state, proof_body).await;
            if !settle_resp.status().is_success_for_paid_path() {
                return Ok(settle_resp);
            }

            // ----- task 8.3: SPL TransferChecked when state is fresh -----
            //
            // `record` was loaded above before the verify-and-settle
            // call, so it reflects the state we observed when we
            // entered the critical section. After the transfer step we
            // re-load to surface the persisted `transfer_signature`
            // for task 8.4 (or to detect a racing branch's signature).
            let record_after_transfer = match record.state {
                OrderState::PendingTransfer => {
                    match do_spl_transfer(state, ledger, payment_uid_hex, parsed).await {
                        Ok(updated) => updated,
                        Err(resp) => return Ok(*resp),
                    }
                }
                // Resume case: already at or past transfer_landed.
                // The persisted `transfer_signature` is authoritative —
                // task 8.4 picks it up below.
                _ => record.clone(),
            };

            // ----- task 8.4: evidence upload + SubmitDelivery + completion -----
            //
            // From here we unconditionally drive the row to `completed`,
            // resuming from whatever step the persisted record indicates.
            // The helpers below encapsulate each step and idempotency
            // policy:
            //
            //  - If the row is already `delivery_submitted`, the evidence
            //    URL is persisted and we only need to re-derive the
            //    delivery_hash from the catalog / refs to build the
            //    SubmitDelivery instruction. We re-build the
            //    TransferEvidence with the recorded `transfer_signature`
            //    so the same delivery_hash is reproduced byte-for-byte.
            //  - If the row is at `transfer_landed`, we upload the
            //    evidence first.
            //
            // On success we return the final 200 response with the
            // canonical `completed_response` shape so replays and fresh
            // completions are indistinguishable to the buyer.

            // (i) Build the evidence document the seller will commit to.
            //     The shape is purely a function of (transfer_signature,
            //     payment_uid, mint, recipient_owner, deliver_amount_raw), so
            //     the same `delivery_hash` is reproduced on a resume.
            let transfer_signature = record_after_transfer
                .transfer_signature
                .clone()
                .ok_or_else(|| LedgerError::Db(format!(
                    "purchase_orders row at state={:?} has no transfer_signature; expected at least one signature after the transfer step",
                    record_after_transfer.state.as_str()
                )))?;
            let evidence = match build_transfer_evidence(
                parsed.entry,
                parsed.quote.deliver_amount_raw,
                &parsed.recipient_owner,
                payment_uid_hex,
                &parsed.buyer_nonce,
                &transfer_signature,
            ) {
                Ok(e) => e,
                Err(resp) => return Ok(*resp),
            };

            // (ii) Compute `delivery_hash = SHA256(canonical_evidence_json)`
            //      against the byte-exact serialization the registry
            //      stores. Both the buyer + oracle reproduce this same
            //      hash from the URL we hand back, so it must equal the
            //      bytes that travel over the wire.
            let evidence_bytes =
                match serde_json::to_vec(&evidence).map_err(RegistryClientError::SerializeFailed) {
                    Ok(b) => b,
                    Err(e) => {
                        // Pure local serialization failure — promote to a
                        // 500. Don't mark the order failed: this is not a
                        // schema or transport problem, so retrying once
                        // the bug is fixed is safe.
                        return Ok(map_registry_error_to_response(headers, "build_evidence", e));
                    }
                };
            let delivery_hash: [u8; 32] = Sha256::digest(&evidence_bytes).into();

            // (iii) Upload evidence (or reuse the persisted URL on a
            //       resume from `delivery_submitted`).
            let record_after_evidence = match record_after_transfer.state {
                OrderState::TransferLanded => {
                    match do_upload_evidence(ledger, payment_uid_hex, evidence, headers).await {
                        Ok(rec) => rec,
                        Err(resp) => return Ok(*resp),
                    }
                }
                OrderState::DeliverySubmitted | OrderState::Completed => {
                    // Resume: trust the persisted URL.
                    record_after_transfer.clone()
                }
                OrderState::PendingTransfer | OrderState::Failed => {
                    // Unreachable: the transfer step above advances the
                    // row past PendingTransfer, and we'd have returned
                    // already on Failed.
                    return Ok(internal_error(format!(
                        "buy-spl-token: unexpected order state {:?} after transfer step",
                        record_after_transfer.state.as_str()
                    )));
                }
            };

            // Belt-and-suspenders: a `Completed` row was already handled
            // earlier in `match record.state`, but a sibling retry could
            // have raced past us. If the row is now Completed we replay
            // the stored signatures verbatim.
            if record_after_evidence.state == OrderState::Completed {
                return Ok(completed_response(
                    headers,
                    &record_after_evidence,
                    &built.sla_hash_hex(),
                ));
            }

            let evidence_url = record_after_evidence
                .evidence_url
                .clone()
                .ok_or_else(|| LedgerError::Db(format!(
                    "purchase_orders row at state={:?} has no evidence_url; refusing to call SubmitDelivery without one",
                    record_after_evidence.state.as_str()
                )))?;

            // (iv) Build, sign, and submit the SubmitDelivery transaction.
            //      On confirmation the row advances to `completed`.
            let final_record = match do_submit_delivery(
                state,
                ledger,
                payment_uid_hex,
                parsed,
                payment_uid_raw,
                delivery_hash,
            )
            .await
            {
                Ok(rec) => rec,
                Err(resp) => return Ok(*resp),
            };

            info!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                evidence_url = %evidence_url,
                "buy-spl-token completed end-to-end"
            );

            Ok(completed_response(
                headers,
                &final_record,
                &built.sla_hash_hex(),
            ))
        }
    }
}

/// Run `FacilitatorClient::verify_and_settle` and translate its outcome
/// into either a "carry on" sentinel response (status 200, never returned
/// to the buyer; just used to keep the type plumbing simple) or a 402
/// settlement-failed response.
async fn run_verify_and_settle(
    headers: &HeaderMap,
    state: &AppState,
    proof_body: &Value,
) -> Response<Body> {
    match state.facilitator.verify_and_settle(proof_body).await {
        Ok(_settled) => sentinel_settled_ok(),
        Err(e) => {
            warn!(
                target: "server_log",
                error = %e,
                "buy-spl-token verify_and_settle failed"
            );
            error_response(
                headers,
                VercelStatusCode::PAYMENT_REQUIRED,
                "settlement_failed",
                format!("verify_and_settle rejected the payment proof: {}", e),
            )
        }
    }
}

/// Bare 200 response used as a "settle succeeded" sentinel inside the
/// locked critical section. The real 200 / 501 / 502 response is built
/// after tasks 8.3 / 8.4 — we only need a status check here. This
/// response never escapes [`run_paid_under_lock`].
fn sentinel_settled_ok() -> Response<Body> {
    Response::builder()
        .status(VercelStatusCode::OK)
        .body(Body::Empty)
        .unwrap_or_else(|_| Response::builder().status(500).body(Body::Empty).unwrap())
}

// ---------------------------------------------------------------------------
// Paid path (task 8.3): SPL TransferChecked + ledger advance
// ---------------------------------------------------------------------------

/// Run the SPL `TransferChecked` step of the paid path:
///
/// 1. Resolve `deliver_amount_raw` from catalog (`deliver_amount_ui` × 10^decimals).
/// 2. Resolve the source ATA: catalog `sender_treasury_ata` wins;
///    otherwise derive the seller's ATA for the configured mint.
/// 3. Derive the buyer's destination ATA owned by `recipient_owner`,
///    and prepend an idempotent `create_associated_token_account` so
///    the destination exists before the transfer.
/// 4. Fetch a fresh blockhash, sign with the [`SellerSigner`], submit
///    under the [`RetryPolicy`], and on confirmation transition
///    `pending_transfer → transfer_landed` storing `transfer_signature`.
///
/// On a zero-row transition (a sibling retry already advanced the row),
/// the persisted state is read back and the racing winner's signature
/// is used. On terminal failure the row is marked `failed (step =
/// transfer)` and a 502 is returned.
///
/// Returns the freshly-loaded [`OrderRecord`] on success so the caller
/// can hand the persisted `transfer_signature` to task 8.4.
async fn do_spl_transfer(
    state: &AppState,
    ledger: &PurchaseLedger,
    payment_uid_hex: &str,
    parsed: &ParsedRequest<'_>,
) -> Result<OrderRecord, Box<Response<Body>>> {
    // (a) Pull out the catalog entry and the seller signer. Both must
    // be present in any cold-started buy-endpoint AppState; if either
    // is absent we have a wiring bug, not a buyer error.
    let entry = parsed.entry;
    let seller = state
        .seller_signer
        .as_ref()
        .ok_or_else(|| {
            Box::new(internal_error(
                "buy endpoint not initialized (seller signer missing)",
            ))
        })?
        .clone();

    // (b) Session deliverable raw from seller quote (quantity-scaled).
    let deliver_amount_raw = parsed.quote.deliver_amount_raw;

    // (c) Parse the configured mint and the recipient owner.
    let mint_pk = Pubkey::from_str(&entry.mint).map_err(|e| {
        Box::new(internal_error(format!(
            "catalog entry {:?} mint {:?} is not a base58 pubkey: {}",
            entry.name, entry.mint, e
        )))
    })?;
    let recipient_owner = Pubkey::from_str(&parsed.recipient_owner).map_err(|e| {
        // ParsedRequest::recipient_owner was already validated as base58
        // pubkey in `validate_request`, so this branch is unreachable in
        // practice — but a defensive check costs us nothing.
        Box::new(internal_error(format!(
            "recipient_owner {:?} is not a base58 pubkey (post-validation invariant violated): {}",
            parsed.recipient_owner, e
        )))
    })?;

    // (d) Resolve the source ATA: catalog override beats derivation.
    let seller_pubkey = seller.pubkey();
    let source_ata = match entry.sender_treasury_ata.as_deref() {
        Some(s) => Pubkey::from_str(s).map_err(|e| {
            Box::new(internal_error(format!(
                "catalog entry {:?} sender_treasury_ata {:?} is not a base58 pubkey: {}",
                entry.name, s, e
            )))
        })?,
        None => {
            spl_associated_token_account::get_associated_token_address(&seller_pubkey, &mint_pk)
        }
    };
    let dest_ata =
        spl_associated_token_account::get_associated_token_address(&recipient_owner, &mint_pk);

    // (e) Build the instruction list. The idempotent
    //     `create_associated_token_account` is safe to send unconditionally:
    //     if the destination ATA already exists the instruction is a no-op,
    //     and if it does not we save a separate round-trip.
    //
    //     The seller signer pays for the ATA creation since this server
    //     does not collect SOL fees from the buyer.
    let create_ata_ix: Instruction =
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &seller_pubkey,
            &recipient_owner,
            &mint_pk,
            &spl_token::ID,
        );
    let transfer_ix: Instruction = spl_token::instruction::transfer_checked(
        &spl_token::ID,
        &source_ata,
        &mint_pk,
        &dest_ata,
        &seller_pubkey,
        &[],
        deliver_amount_raw,
        entry.decimals,
    )
    .map_err(|e| {
        Box::new(internal_error(format!(
            "spl_token::instruction::transfer_checked failed: {}",
            e
        )))
    })?;
    let instructions = vec![create_ata_ix, transfer_ix];

    // (f) Submit + confirm under the retry policy. We fetch a fresh
    //     blockhash on every attempt so a transient retry never reuses
    //     a stale blockhash that has already expired (req 6.4 / 4.7).
    let retry_policy = RetryPolicy::from_env();
    let rpc = state.rpc_client.clone();
    let keypair = seller.keypair();
    let label = "buy_spl_token::transfer_checked";

    let submit_outcome = with_retry(retry_policy, label, Some(payment_uid_hex), || {
        let rpc = rpc.clone();
        let keypair = keypair.clone();
        let instructions = instructions.clone();
        let payer = seller_pubkey;
        async move {
            let recent_blockhash = rpc.get_latest_blockhash().await?;
            let tx = Transaction::new_signed_with_payer(
                &instructions,
                Some(&payer),
                &[keypair.as_ref()],
                recent_blockhash,
            );
            let sig: Signature = rpc.send_and_confirm_transaction(&tx).await?;
            Ok::<Signature, solana_rpc_client_api::client_error::Error>(sig)
        }
    })
    .await;

    let signature = match submit_outcome {
        Ok(sig) => sig,
        Err(e) => {
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                error = %e,
                "buy-spl-token transfer_checked exhausted retries; marking order failed"
            );
            // Mark the row failed; on DB failure we still want to surface the
            // 502 so the buyer is told the transfer never landed.
            if let Err(db_err) = ledger.mark_failed(payment_uid_hex, "transfer").await {
                warn!(
                    target: "server_log",
                    payment_uid = %payment_uid_hex,
                    error = %db_err,
                    "buy-spl-token mark_failed(transfer) failed after RPC exhaustion"
                );
            }
            return Err(Box::new(error_response_with_details(
                &HeaderMap::new(),
                VercelStatusCode::BAD_GATEWAY,
                "transfer_failed",
                format!("SPL TransferChecked failed after retries: {}", e),
                json!({
                    "payment_uid": payment_uid_hex,
                    "step": "transfer",
                }),
            )));
        }
    };

    info!(
        target: "server_log",
        payment_uid = %payment_uid_hex,
        signature = %signature,
        source_ata = %source_ata,
        dest_ata = %dest_ata,
        deliver_amount_raw,
        decimals = entry.decimals,
        "buy-spl-token transfer_checked landed"
    );

    // (g) Advance the ledger row. A zero-row update means a sibling
    //     retry won the race and already wrote a `transfer_signature`;
    //     we honor the persisted value rather than overwriting it.
    let signature_str = signature.to_string();
    let transition_outcome = ledger
        .transition(
            payment_uid_hex,
            OrderState::PendingTransfer,
            OrderState::TransferLanded,
            &TransitionFields::new().with_transfer_signature(signature_str.clone()),
        )
        .await;

    match transition_outcome {
        Ok(()) => {}
        Err(LedgerError::ZeroRowTransition { current }) => {
            // A sibling retry already advanced the row. The signature
            // we just landed is still on-chain, but the canonical
            // `transfer_signature` for this order is whatever the
            // racing winner persisted. Log enough to reconcile and
            // continue with the persisted record.
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                our_signature = %signature_str,
                current_state = %current,
                "buy-spl-token transfer_checked: ledger already advanced by racing branch; using persisted transfer_signature"
            );
        }
        Err(e) => {
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                error = %e,
                "buy-spl-token transition(pending_transfer→transfer_landed) failed"
            );
            return Err(Box::new(internal_error(format!(
                "ledger transition after transfer failed: {}",
                e
            ))));
        }
    }

    // (h) Re-load so the caller sees the persisted `transfer_signature`
    //     (ours, or the racing winner's).
    match ledger.load(payment_uid_hex).await {
        Ok(Some(record)) => Ok(record),
        Ok(None) => Err(Box::new(internal_error(format!(
            "purchase_orders row vanished for payment_uid={:?} after transfer",
            payment_uid_hex
        )))),
        Err(e) => Err(Box::new(internal_error(format!(
            "ledger reload after transfer failed: {}",
            e
        )))),
    }
}

// ---------------------------------------------------------------------------
// Paid path (task 8.4): TransferEvidence + SubmitDelivery
// ---------------------------------------------------------------------------
//
// The SubmitDelivery instruction is built by hand against the ABI spec at
// `x402/sla-escrow-onchain-abi/v1` (§5.7 SubmitDelivery,
// §2.1 PDA seeds). Hand-rolling — rather than using the `sla-escrow-api`
// SDK — is the documented multi-cluster bypass: the SDK's compile-time
// `declare_id!` forces a cluster-specific crate version (0.4.x for mainnet,
// 0.2.x for devnet), which would prevent a single Vercel deployment from
// supporting both. We thread `SLA_ESCROW_PROGRAM_ID` through env config and
// derive PDAs against that runtime value.

/// SLA-Escrow program id (mainnet) used for the SubmitDelivery
/// instruction. Operators running against a different cluster (e.g.
/// devnet) override via `SLA_ESCROW_PROGRAM_ID`.
const SLA_ESCROW_PROGRAM_ID_MAINNET: &str = "SEscZ6n23pVak34xipBKoGCikHUj3w6XPNyty4rHprJ";

/// Process env var carrying the SLA-Escrow program id override. When
/// unset, [`sla_escrow_program_id`] returns the mainnet default above.
const SLA_ESCROW_PROGRAM_ID_ENV: &str = "SLA_ESCROW_PROGRAM_ID";

/// PDA seed constants per ABI spec §2.1. Embedded here so the Vercel
/// function does not need to depend on the on-chain crate.
const SLA_ESCROW_BANK_SEED: &[u8] = b"bank";
const SLA_ESCROW_CONFIG_SEED: &[u8] = b"config";
const SLA_ESCROW_ESCROW_SEED: &[u8] = b"escrow";
const SLA_ESCROW_PAYMENT_SEED: &[u8] = b"payment";

/// SLA-Escrow `EscrowInstruction::SubmitDelivery` discriminator (= `5`)
/// per ABI spec §5.1. Body layout per §5.7:
///
/// ```text
/// [discriminator(1) || delivery_hash(32)]   // 33 bytes total
/// ```
const SUBMIT_DELIVERY_DISCRIMINATOR: u8 = 5;

/// Resolve the SLA-Escrow program id, falling back to the mainnet
/// default when `SLA_ESCROW_PROGRAM_ID` is not set.
fn sla_escrow_program_id() -> Result<Pubkey, String> {
    let raw = std::env::var(SLA_ESCROW_PROGRAM_ID_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SLA_ESCROW_PROGRAM_ID_MAINNET.to_string());
    Pubkey::from_str(&raw).map_err(|e| {
        format!(
            "{} {:?} is not a base58 pubkey: {}",
            SLA_ESCROW_PROGRAM_ID_ENV, raw, e
        )
    })
}

fn sla_escrow_bank_pda(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[SLA_ESCROW_BANK_SEED], program_id).0
}

fn sla_escrow_config_pda(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[SLA_ESCROW_CONFIG_SEED], program_id).0
}

fn sla_escrow_escrow_pda(program_id: &Pubkey, mint: &Pubkey, bank: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[SLA_ESCROW_ESCROW_SEED, mint.as_ref(), bank.as_ref()],
        program_id,
    )
    .0
}

/// Derive the on-chain `Payment` PDA from the buyer's raw 32-byte
/// `payment_uid` (read directly from the FundPayment instruction data
/// — see [`extract_fund_payment_refs`]). Mirrors
/// `sla-escrow-api::state::payment_pda_from_bytes` so the SubmitDelivery
/// instruction targets the same account the FundPayment created.
fn sla_escrow_payment_pda(program_id: &Pubkey, payment_uid: &[u8; 32], bank: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[SLA_ESCROW_PAYMENT_SEED, payment_uid, bank.as_ref()],
        program_id,
    )
    .0
}

/// Build a `SubmitDelivery` instruction for the SLA-Escrow program.
///
/// Layout assumed (from `sla-escrow-api::instruction`):
///
/// - **Accounts**:
///   1. `caller` — merchant payout signer (`payment.seller`, writable, signer)
///   2. `bank` — bank PDA (read-only)
///   3. `config` — config PDA (read-only)
///   4. `escrow` — per-mint escrow PDA (read-only)
///   5. `payment` — payment PDA (writable; the program updates
///      `delivery_hash` and `delivery_timestamp` on this account)
///
/// - **Instruction data**: `[5u8] || delivery_hash[32]` for a total of
///   33 bytes.
///
/// The seeds match `sla-escrow-api::state::*_pda` exactly so the
/// resolved PDAs equal the ones the FundPayment instruction created.
fn submit_delivery_ix(
    program_id: Pubkey,
    seller: Pubkey,
    mint: Pubkey,
    payment_uid: &[u8; 32],
    delivery_hash: [u8; 32],
) -> Instruction {
    let bank = sla_escrow_bank_pda(&program_id);
    let config = sla_escrow_config_pda(&program_id);
    let escrow = sla_escrow_escrow_pda(&program_id, &mint, &bank);
    let payment = sla_escrow_payment_pda(&program_id, payment_uid, &bank);

    let mut data = Vec::with_capacity(1 + 32);
    data.push(SUBMIT_DELIVERY_DISCRIMINATOR);
    data.extend_from_slice(&delivery_hash);

    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(seller, true),
            AccountMeta::new_readonly(bank, false),
            AccountMeta::new_readonly(config, false),
            AccountMeta::new_readonly(escrow, false),
            AccountMeta::new(payment, false),
        ],
        data,
    }
}

/// Build the strongly-typed [`crate::registry_client::TransferEvidence`]
/// from request inputs and the persisted `transfer_signature`.
///
/// Returns the boxed error response on builder failure (which only
/// happens when a required field is empty — a wiring bug).
fn build_transfer_evidence(
    entry: &CatalogEntry,
    deliver_amount_raw: u64,
    recipient_owner: &str,
    payment_uid_hex: &str,
    buyer_nonce: &str,
    transfer_signature: &str,
) -> Result<crate::registry_client::TransferEvidence, Box<Response<Body>>> {
    let claimed_delta = deliver_amount_raw.to_string();
    let submitted_at = Utc::now().timestamp();

    TransferEvidenceBuilder::new()
        .tx_signature(transfer_signature)
        .add_asserted_transfer(AssertedTransfer {
            mint: entry.mint.clone(),
            recipient_owner: recipient_owner.to_string(),
            claimed_delta,
        })
        .submitted_at(submitted_at)
        .payment_uid(payment_uid_hex)
        .buyer_nonce(buyer_nonce.to_string())
        .build()
        .map_err(|e| {
            Box::new(internal_error(format!(
                "TransferEvidenceBuilder failed: {}",
                e
            )))
        })
}

/// Validate, upload, and ledger-advance the TransferEvidence.
///
/// Schema validation is performed by `RegistryClient::upload_evidence`
/// **before** any HTTP traffic — a [`RegistryClientError::SchemaValidation`]
/// short-circuits the upload and triggers a 500 with `error.code =
/// evidence_schema_invalid`. The order is then marked `failed` (req
/// 7.4).
///
/// Transport / HTTP failures map to 502 (registry unavailable). The
/// order is **not** marked failed in that case: registry outages are
/// transient and the buyer can replay the same paid request after the
/// registry recovers.
async fn do_upload_evidence(
    ledger: &PurchaseLedger,
    payment_uid_hex: &str,
    evidence: crate::registry_client::TransferEvidence,
    headers: &HeaderMap,
) -> Result<OrderRecord, Box<Response<Body>>> {
    let registry = match make_registry_client() {
        Ok(c) => c,
        Err(msg) => {
            return Err(Box::new(error_response(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "registry_unavailable",
                msg,
            )));
        }
    };

    let upload_result = registry.upload_evidence(evidence).await;
    let evidence_url = match upload_result {
        Ok(u) => u,
        Err(RegistryClientError::SchemaValidation { errors }) => {
            // Schema-validation failure → terminal failure of the order.
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                errors = ?errors,
                "buy-spl-token evidence schema validation failed; marking order failed"
            );
            if let Err(db_err) = ledger.mark_failed(payment_uid_hex, "evidence").await {
                warn!(
                    target: "server_log",
                    payment_uid = %payment_uid_hex,
                    error = %db_err,
                    "buy-spl-token mark_failed(evidence) failed after schema-validation"
                );
            }
            return Err(Box::new(error_response_with_details(
                headers,
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "evidence_schema_invalid",
                format!(
                    "evidence document failed JSON Schema validation ({} errors)",
                    errors.len()
                ),
                json!({
                    "payment_uid": payment_uid_hex,
                    "errors": errors,
                }),
            )));
        }
        Err(e @ RegistryClientError::HttpStatus { .. })
        | Err(e @ RegistryClientError::Transport { .. })
        | Err(e @ RegistryClientError::MalformedResponse { .. }) => {
            // Registry transport / availability failure → 502, no
            // ledger transition. Buyer can retry the paid request once
            // the registry recovers.
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                error = %e,
                "buy-spl-token evidence upload failed (registry transport)"
            );
            return Err(Box::new(error_response_with_details(
                headers,
                VercelStatusCode::BAD_GATEWAY,
                "registry_unavailable",
                e.to_string(),
                json!({
                    "payment_uid": payment_uid_hex,
                    "step": "upload_evidence",
                }),
            )));
        }
        Err(e) => {
            // Other registry errors (serialize, missing field, schema-
            // compile) — internal bug, surface as 500 without marking
            // the order failed.
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                error = %e,
                "buy-spl-token evidence upload failed (internal)"
            );
            return Err(Box::new(map_registry_error_to_response(
                headers,
                "upload_evidence",
                e,
            )));
        }
    };

    let evidence_url_str = evidence_url.as_str().to_string();
    let transition_outcome = ledger
        .transition(
            payment_uid_hex,
            OrderState::TransferLanded,
            OrderState::DeliverySubmitted,
            &TransitionFields::new().with_evidence_url(evidence_url_str.clone()),
        )
        .await;

    match transition_outcome {
        Ok(()) => {}
        Err(LedgerError::ZeroRowTransition { current }) => {
            // A sibling retry advanced the row first. The persisted
            // `evidence_url` is authoritative; we'll pick it up below
            // when we re-load.
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                our_evidence_url = %evidence_url_str,
                current_state = %current,
                "buy-spl-token evidence: ledger already advanced by racing branch; using persisted evidence_url"
            );
        }
        Err(e) => {
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                error = %e,
                "buy-spl-token transition(transfer_landed→delivery_submitted) failed"
            );
            return Err(Box::new(internal_error(format!(
                "ledger transition after evidence upload failed: {}",
                e
            ))));
        }
    }

    // Re-load so the caller sees the persisted `evidence_url` (ours, or
    // the racing winner's).
    match ledger.load(payment_uid_hex).await {
        Ok(Some(record)) => Ok(record),
        Ok(None) => Err(Box::new(internal_error(format!(
            "purchase_orders row vanished for payment_uid={:?} after evidence upload",
            payment_uid_hex
        )))),
        Err(e) => Err(Box::new(internal_error(format!(
            "ledger reload after evidence upload failed: {}",
            e
        )))),
    }
}

/// Run the SubmitDelivery step of the paid path:
///
/// 1. Resolve the SLA-Escrow program id from env (fallback: mainnet).
/// 2. Build the SubmitDelivery instruction (caller = merchant payout key, mint =
///    USDC escrow mint, payment_uid PDA derived from the buyer's raw
///    32-byte `payment_uid`, data = `[5, delivery_hash]`).
/// 3. Fetch a fresh blockhash, sign with the merchant key, submit under
///    the configured retry policy.
/// 4. On confirmation transition `delivery_submitted → completed`
///    storing `delivery_signature`. On terminal failure mark the row
///    `failed (step = submit_delivery)` and return 502.
async fn do_submit_delivery(
    state: &AppState,
    ledger: &PurchaseLedger,
    payment_uid_hex: &str,
    parsed: &ParsedRequest<'_>,
    payment_uid_raw: &[u8; 32],
    delivery_hash: [u8; 32],
) -> Result<OrderRecord, Box<Response<Body>>> {
    let _entry = parsed.entry;
    let merchant = state
        .merchant_signer
        .as_ref()
        .ok_or_else(|| {
            Box::new(internal_error(
                "buy endpoint not initialized (merchant signer missing)",
            ))
        })?
        .clone();

    let program_id = match sla_escrow_program_id() {
        Ok(p) => p,
        Err(msg) => return Err(Box::new(internal_error(msg))),
    };

    // The escrow PDA SubmitDelivery references is keyed by the **USDC**
    // mint (the asset escrowed in `FundPayment`), NOT the SPL token the
    // buyer is purchasing (`entry.mint`). Using the wrong mint here
    // resolves to an uninitialized PDA and the program rejects the
    // instruction with "Invalid account owner" at `is_escrow()`.
    let usdc_mint_str = usdc_mint_for_network(&state.config.x402_network);
    let mint_pk = Pubkey::from_str(usdc_mint_str).map_err(|e| {
        Box::new(internal_error(format!(
            "internal: USDC mint constant {:?} is not a base58 pubkey: {}",
            usdc_mint_str, e
        )))
    })?;

    let merchant_pubkey = merchant.pubkey();
    let ix = submit_delivery_ix(
        program_id,
        merchant_pubkey,
        mint_pk,
        payment_uid_raw,
        delivery_hash,
    );
    let instructions = vec![ix];

    let retry_policy = RetryPolicy::from_env();
    let rpc = state.rpc_client.clone();
    let keypair = merchant.keypair();
    let label = "buy_spl_token::submit_delivery";

    let submit_outcome = with_retry(retry_policy, label, Some(payment_uid_hex), || {
        let rpc = rpc.clone();
        let keypair = keypair.clone();
        let instructions = instructions.clone();
        let payer = merchant_pubkey;
        async move {
            let recent_blockhash = rpc.get_latest_blockhash().await?;
            let tx = Transaction::new_signed_with_payer(
                &instructions,
                Some(&payer),
                &[keypair.as_ref()],
                recent_blockhash,
            );
            let sig: Signature = rpc.send_and_confirm_transaction(&tx).await?;
            Ok::<Signature, solana_rpc_client_api::client_error::Error>(sig)
        }
    })
    .await;

    let signature = match submit_outcome {
        Ok(sig) => sig,
        Err(e) => {
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                error = %e,
                "buy-spl-token submit_delivery exhausted retries; marking order failed"
            );
            if let Err(db_err) = ledger.mark_failed(payment_uid_hex, "submit_delivery").await {
                warn!(
                    target: "server_log",
                    payment_uid = %payment_uid_hex,
                    error = %db_err,
                    "buy-spl-token mark_failed(submit_delivery) failed after RPC exhaustion"
                );
            }
            return Err(Box::new(error_response_with_details(
                &HeaderMap::new(),
                VercelStatusCode::BAD_GATEWAY,
                "submit_delivery_failed",
                format!("SubmitDelivery failed after retries: {}", e),
                json!({
                    "payment_uid": payment_uid_hex,
                    "step": "submit_delivery",
                }),
            )));
        }
    };

    info!(
        target: "server_log",
        payment_uid = %payment_uid_hex,
        signature = %signature,
        delivery_hash = %hex::encode(delivery_hash),
        "buy-spl-token submit_delivery landed"
    );

    let signature_str = signature.to_string();
    let transition_outcome = ledger
        .transition(
            payment_uid_hex,
            OrderState::DeliverySubmitted,
            OrderState::Completed,
            &TransitionFields::new().with_delivery_signature(signature_str.clone()),
        )
        .await;

    match transition_outcome {
        Ok(()) => {}
        Err(LedgerError::ZeroRowTransition { current }) => {
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                our_signature = %signature_str,
                current_state = %current,
                "buy-spl-token submit_delivery: ledger already advanced by racing branch; using persisted delivery_signature"
            );
        }
        Err(e) => {
            warn!(
                target: "server_log",
                payment_uid = %payment_uid_hex,
                error = %e,
                "buy-spl-token transition(delivery_submitted→completed) failed"
            );
            return Err(Box::new(internal_error(format!(
                "ledger transition after submit_delivery failed: {}",
                e
            ))));
        }
    }

    match ledger.load(payment_uid_hex).await {
        Ok(Some(record)) => Ok(record),
        Ok(None) => Err(Box::new(internal_error(format!(
            "purchase_orders row vanished for payment_uid={:?} after submit_delivery",
            payment_uid_hex
        )))),
        Err(e) => Err(Box::new(internal_error(format!(
            "ledger reload after submit_delivery failed: {}",
            e
        )))),
    }
}

/// Construct a generic 500 error response with no extra details. Used
/// from helpers that don't have a useful `HeaderMap` in scope; the
/// caller frame reattaches CORS / `X-API-Version` headers automatically
/// because [`error_response`] always emits them.
fn internal_error(message: impl Into<String>) -> Response<Body> {
    error_response(
        &HeaderMap::new(),
        VercelStatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        message,
    )
}

/// Build the 200 response replayed for a `Completed` order. Echoes the
/// stored `transfer_signature`, `evidence_url`, and `delivery_signature`
/// columns from the ledger row alongside the canonical `slaHash`, so the
/// shape matches the eventual happy-path response from task 8.4.
fn completed_response(
    headers: &HeaderMap,
    record: &OrderRecord,
    sla_hash_hex: &str,
) -> Response<Body> {
    let date = Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let body = json!({
        "status": "completed",
        "paymentUid": record.payment_uid,
        "slaHash": sla_hash_hex,
        "transferSignature": record.transfer_signature,
        "evidenceUrl": record.evidence_url,
        "deliverySignature": record.delivery_signature,
    });
    let mut builder = Response::builder()
        .status(VercelStatusCode::OK)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, OPTIONS")
        .header("Access-Control-Allow-Headers", ALLOW_HEADERS)
        .header("X-API-Version", "1")
        .header("X-Date", date);

    if let Some(cid) = headers.get("X-Correlation-ID") {
        builder = builder.header("X-Correlation-ID", cid);
    }

    builder
        .body(Body::Text(body.to_string()))
        .unwrap_or_else(|_| Response::builder().status(500).body(Body::Empty).unwrap())
}

/// Local helper: distinguish the [`sentinel_settled_ok`] success
/// sentinel from real settlement-failed responses. We can't compare on
/// `StatusCode::OK` directly without scoping because other 2xx values
/// would also pass — but in this critical section only the sentinel
/// uses 200.
trait PaidPathStatusExt {
    fn is_success_for_paid_path(&self) -> bool;
}

impl PaidPathStatusExt for VercelStatusCode {
    fn is_success_for_paid_path(&self) -> bool {
        *self == VercelStatusCode::OK
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate request inputs and resolve the catalog entry. On failure
/// returns the boxed error response (boxed to keep the success size of
/// `Result` cheap and to side-step the large-Err lint).
fn validate_request<'s>(
    params: &BuyQuery,
    state: &'s AppState,
) -> Result<ParsedRequest<'s>, Box<Response<Body>>> {
    // Required-parameter check produces 400 missing_parameter listing the
    // first missing field. The list of missing names is also surfaced in
    // `details` so a client can fix all problems at once.
    let mut missing: Vec<&'static str> = Vec::new();
    if params.token.as_deref().filter(|s| !s.is_empty()).is_none() {
        missing.push("token");
    }
    if params
        .recipient_owner
        .as_deref()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        missing.push("recipient_owner");
    }
    if params
        .buyer_nonce
        .as_deref()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        missing.push("buyer_nonce");
    }
    if !missing.is_empty() {
        return Err(Box::new(error_response_with_details(
            &HeaderMap::new(),
            VercelStatusCode::BAD_REQUEST,
            "missing_parameter",
            format!(
                "missing required query parameter(s): {}",
                missing.join(", ")
            ),
            json!({ "missing": missing }),
        )));
    }

    let token = params.token.as_deref().unwrap();
    let recipient_owner = params.recipient_owner.as_deref().unwrap();
    let buyer_nonce = params.buyer_nonce.as_deref().unwrap();

    // recipient_owner: base58 pubkey.
    if Pubkey::from_str(recipient_owner).is_err() {
        return Err(Box::new(error_response_with_details(
            &HeaderMap::new(),
            VercelStatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!(
                "query parameter 'recipient_owner' is not a valid base58 Solana pubkey: {:?}",
                recipient_owner
            ),
            json!({ "parameter": "recipient_owner" }),
        )));
    }

    // buyer_nonce: exactly 64 lowercase hex chars.
    if !is_64_lowercase_hex(buyer_nonce) {
        return Err(Box::new(error_response_with_details(
            &HeaderMap::new(),
            VercelStatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!(
                "query parameter 'buyer_nonce' must be exactly 64 lowercase hex chars; got {:?}",
                buyer_nonce
            ),
            json!({ "parameter": "buyer_nonce" }),
        )));
    }

    // token: must match a catalog entry's mint.
    let catalog = match state.catalog.as_ref() {
        Some(c) => c,
        None => {
            return Err(Box::new(error_response(
                &HeaderMap::new(),
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "buy endpoint not initialized (token catalog missing)",
            )));
        }
    };
    let entry = match catalog.find_by_mint(token) {
        Some(e) => e,
        None => {
            return Err(Box::new(error_response_with_details(
                &HeaderMap::new(),
                VercelStatusCode::NOT_FOUND,
                "unknown_token",
                format!("token mint {:?} is not in the buy catalog", token),
                json!({ "token": token }),
            )));
        }
    };

    let quantity = match quote::parse_quantity(params.quantity.as_deref()) {
        Ok(q) => q,
        Err(e) => {
            return Err(Box::new(error_response_with_details(
                &HeaderMap::new(),
                VercelStatusCode::BAD_REQUEST,
                "invalid_parameter",
                format!("query parameter 'quantity': {}", e),
                json!({ "parameter": "quantity" }),
            )));
        }
    };

    let session_quote = match entry.session_quote(quantity) {
        Ok(q) => q,
        Err(e) => {
            return Err(Box::new(error_response(
                &HeaderMap::new(),
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "invalid_quote",
                format!(
                    "catalog entry {:?} cannot produce a session quote for quantity {}: {}",
                    entry.name, quantity, e
                ),
            )));
        }
    };

    Ok(ParsedRequest {
        entry,
        quote: session_quote,
        recipient_owner: recipient_owner.to_string(),
        buyer_nonce: buyer_nonce.to_string(),
    })
}

fn is_64_lowercase_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

fn extract_payment_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("payment-signature")
        .or_else(|| headers.get("PAYMENT-SIGNATURE"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Construct a [`RegistryClient`] from process env. Fails when the base
/// URL is unset because we cannot upload anything without it.
fn make_registry_client() -> Result<RegistryClient, String> {
    let base = std::env::var(REGISTRY_BASE_URL)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{} is not set", REGISTRY_BASE_URL))?;
    let client = RegistryClient::new(base, RetryPolicy::from_env());
    let client = match std::env::var(REGISTRY_BEARER_TOKEN)
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(t) => client.with_bearer(t),
        None => client,
    };
    Ok(client)
}

/// Pick the USDC mint that matches the configured Solana network. Devnet
/// is detected by the CAIP-2 suffix; everything else falls back to
/// mainnet.
fn usdc_mint_for_network(network: &str) -> &'static str {
    if network.contains("EtWTRABZaYq6iMfeYKouRu166VU2xqa1") {
        USDC_DEVNET_MINT
    } else {
        USDC_MAINNET_MINT
    }
}

/// Pick the kebab-case cluster name the oracle's `TransferCluster` enum
/// expects. `solana:Etwt…` (devnet CAIP-2) → `"devnet"`; testnet CAIP-2
/// → `"testnet"`; everything else → `"mainnet-beta"`.
fn cluster_name_for_network(network: &str) -> &'static str {
    // Devnet genesis: EtWTRABZaYq6iMfeYKouRu166VU2xqa1
    if network.contains("EtWTRABZaYq6iMfeYKouRu166VU2xqa1") {
        "devnet"
    } else if network.contains("4uhcVJyU9pJkvQyS88uRDiswHXSCkY3z") {
        // Testnet genesis prefix.
        "testnet"
    } else {
        "mainnet-beta"
    }
}

/// Fetch the canonical sla-escrow `extra` block from the configured
/// pr402 facilitator. Used at request time to seed the 402 envelope so
/// new fields pr402 starts requiring (e.g. `feePayer`, `bankAddress`,
/// `feeBps`, `oracleFeeBps`, `ttlSeconds`, `maxComputeUnitLimit`,
/// `recommendedComputeUnitPrice`, `slaFundTxNetworkFeePayer`) propagate
/// without code changes here.
///
/// Returns the JSON object verbatim; the caller overlays seller-specific
/// fields on top.
async fn facilitator_sla_escrow_extra(
    facilitator: &crate::x402::FacilitatorClient,
    network: &str,
) -> Result<serde_json::Value, String> {
    let supported = facilitator
        .supported()
        .await
        .map_err(|e| format!("supported request failed: {}", e))?;

    let kind = supported
        .kinds
        .iter()
        .find(|k| k.scheme == SLA_ESCROW_SCHEME && k.network == network)
        .ok_or_else(|| {
            format!(
                "no kind matched scheme={} network={}",
                SLA_ESCROW_SCHEME, network
            )
        })?;

    Ok(kind
        .extra
        .clone()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default())))
}

fn map_sla_builder_error_to_500(headers: &HeaderMap, e: SlaBuilderError) -> Response<Body> {
    warn!("buy-spl-token sla builder failed: {}", e);
    error_response(
        headers,
        VercelStatusCode::INTERNAL_SERVER_ERROR,
        "sla_builder_failed",
        format!("failed to build canonical SLA: {}", e),
    )
}

fn map_registry_error_to_response(
    headers: &HeaderMap,
    step: &'static str,
    e: RegistryClientError,
) -> Response<Body> {
    warn!("buy-spl-token registry {} failed: {}", step, e);
    match &e {
        RegistryClientError::SchemaValidation { .. } => error_response(
            headers,
            VercelStatusCode::INTERNAL_SERVER_ERROR,
            "evidence_schema_invalid",
            e.to_string(),
        ),
        RegistryClientError::HttpStatus { .. } | RegistryClientError::Transport { .. } => {
            error_response(
                headers,
                VercelStatusCode::BAD_GATEWAY,
                "registry_unavailable",
                e.to_string(),
            )
        }
        _ => error_response(
            headers,
            VercelStatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            e.to_string(),
        ),
    }
}

/// Build a JSON error response with the canonical
/// `{ "error": { "code", "message" } }` shape and the same CORS / X-API-Version
/// headers used by `check_balance`.
fn error_response(
    headers: &HeaderMap,
    status: VercelStatusCode,
    code: &str,
    message: impl Into<String>,
) -> Response<Body> {
    error_response_with_details(headers, status, code, message, json!({}))
}

/// Variant that includes a free-form `details` object inside the error
/// envelope. The `error` envelope itself still has only `code` / `message`
/// per Requirement 9.1; `details` is an optional sibling that lets us
/// surface offending parameter names without polluting the envelope.
fn error_response_with_details(
    headers: &HeaderMap,
    status: VercelStatusCode,
    code: &str,
    message: impl Into<String>,
    details: serde_json::Value,
) -> Response<Body> {
    let date = Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let mut body = json!({
        "error": {
            "code": code,
            "message": message.into(),
        },
    });
    if !details.is_null() && !matches!(&details, serde_json::Value::Object(m) if m.is_empty()) {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("details".to_string(), details);
        }
    }

    let mut builder = Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, OPTIONS")
        .header("Access-Control-Allow-Headers", ALLOW_HEADERS)
        .header("X-API-Version", "1")
        .header("X-Date", date);

    if let Some(cid) = headers.get("X-Correlation-ID") {
        builder = builder.header("X-Correlation-ID", cid);
    }

    builder
        .body(Body::Text(body.to_string()))
        .unwrap_or_else(|_| Response::builder().status(500).body(Body::Empty).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- query validation helpers --------------------------------------

    #[test]
    fn is_64_lowercase_hex_examples() {
        let good = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(good.len(), 64);
        assert!(is_64_lowercase_hex(good));

        // Uppercase rejected.
        let upper = "Abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert!(!is_64_lowercase_hex(upper));

        // Wrong length rejected.
        assert!(!is_64_lowercase_hex("abc"));

        // Non-hex char rejected.
        let mut bad = String::from(good);
        bad.replace_range(0..1, "g");
        assert!(!is_64_lowercase_hex(&bad));
    }

    #[test]
    fn usdc_mint_picks_devnet_for_devnet_network() {
        assert_eq!(
            usdc_mint_for_network("solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1"),
            USDC_DEVNET_MINT
        );
        assert_eq!(
            usdc_mint_for_network("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"),
            USDC_MAINNET_MINT
        );
    }

    // --- Unified error response shape (task 8.5) ------------------------
    //
    // Verifies that the centralized `error_response` /
    // `error_response_with_details` / `internal_error` helpers all emit
    // the canonical `{ "error": { "code", "message" } }` body shape with
    // the same CORS allow-list and `X-API-Version: 1` header used by
    // `check_balance` (Requirements 9.1 / 9.2). A 500 produced by
    // `internal_error` must carry `error.code = internal_error` per
    // Requirement 9.3.

    /// Read a `Body::Text` response body as a UTF-8 string. Panics on
    /// `Body::Empty` / `Body::Binary` because the helper never produces
    /// either — the panic message is the test failure signal.
    fn body_text(resp: &Response<Body>) -> &str {
        match resp.body() {
            Body::Text(s) => s.as_str(),
            other => panic!("expected Body::Text, got {:?}", other),
        }
    }

    /// All error responses MUST carry these headers — they match the
    /// `check_balance` envelope (`BALANCE_CORS_ALLOW_HEADERS` /
    /// `X-API-Version: 1`) so a buyer's client sees the same CORS surface
    /// across endpoints.
    fn assert_canonical_error_headers(resp: &Response<Body>) {
        let h = resp.headers();
        assert_eq!(
            h.get("Content-Type").and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "error response must declare application/json"
        );
        assert_eq!(
            h.get("X-API-Version").and_then(|v| v.to_str().ok()),
            Some("1"),
            "error response must carry X-API-Version: 1"
        );
        assert_eq!(
            h.get("Access-Control-Allow-Origin")
                .and_then(|v| v.to_str().ok()),
            Some("*"),
            "error response must carry Access-Control-Allow-Origin: *"
        );
        assert_eq!(
            h.get("Access-Control-Allow-Methods")
                .and_then(|v| v.to_str().ok()),
            Some("GET, OPTIONS"),
            "error response must carry the same Access-Control-Allow-Methods as check_balance"
        );
        assert_eq!(
            h.get("Access-Control-Allow-Headers")
                .and_then(|v| v.to_str().ok()),
            Some(crate::cors::ALLOW_HEADERS),
            "error response must carry the same Access-Control-Allow-Headers as check_balance"
        );
    }

    #[test]
    fn error_response_emits_canonical_shape_and_headers() {
        // Representative 400: missing_parameter. This is the most common
        // non-success exit and goes through the helper unchanged.
        let resp = error_response(
            &HeaderMap::new(),
            VercelStatusCode::BAD_REQUEST,
            "missing_parameter",
            "missing required query parameter(s): token",
        );
        assert_eq!(resp.status(), VercelStatusCode::BAD_REQUEST);
        assert_canonical_error_headers(&resp);

        let body: serde_json::Value =
            serde_json::from_str(body_text(&resp)).expect("error body must be valid JSON");
        let error = body.get("error").expect("body must have 'error' object");
        assert_eq!(
            error.get("code").and_then(|v| v.as_str()),
            Some("missing_parameter")
        );
        assert_eq!(
            error.get("message").and_then(|v| v.as_str()),
            Some("missing required query parameter(s): token"),
        );
        // Empty `details` must not be serialized — Requirement 9.1's
        // `error` envelope is exactly `{ code, message }`.
        assert!(
            body.get("details").is_none(),
            "empty details must be omitted"
        );
    }

    #[test]
    fn error_response_with_details_serializes_details_sibling() {
        let resp = error_response_with_details(
            &HeaderMap::new(),
            VercelStatusCode::BAD_REQUEST,
            "missing_parameter",
            "missing required query parameter(s): token",
            json!({ "missing": ["token"] }),
        );
        assert_eq!(resp.status(), VercelStatusCode::BAD_REQUEST);
        assert_canonical_error_headers(&resp);

        let body: serde_json::Value = serde_json::from_str(body_text(&resp)).unwrap();
        let error = body.get("error").unwrap();
        assert_eq!(
            error.get("code").and_then(|v| v.as_str()),
            Some("missing_parameter")
        );
        assert_eq!(
            body.pointer("/details/missing/0").and_then(|v| v.as_str()),
            Some("token"),
            "non-empty details must be serialized as a sibling of `error`",
        );
    }

    #[test]
    fn internal_error_maps_uncovered_failure_to_500_with_code() {
        // Requirement 9.3: an uncovered internal failure must surface as
        // HTTP 500 with `error.code = internal_error`.
        let resp = internal_error("ledger reload after submit_delivery failed: db down");
        assert_eq!(resp.status(), VercelStatusCode::INTERNAL_SERVER_ERROR);
        assert_canonical_error_headers(&resp);

        let body: serde_json::Value = serde_json::from_str(body_text(&resp)).unwrap();
        let error = body.get("error").unwrap();
        assert_eq!(
            error.get("code").and_then(|v| v.as_str()),
            Some("internal_error")
        );
        assert!(error
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("ledger reload after submit_delivery failed"))
            .unwrap_or(false));
    }

    #[test]
    fn error_response_propagates_correlation_id() {
        // The helper mirrors `X-Correlation-ID` from the request when
        // present, matching `check_balance`'s envelope.
        let mut req_headers = HeaderMap::new();
        req_headers.insert(
            "X-Correlation-ID",
            "test-correlation-id-12345".parse().unwrap(),
        );
        let resp = error_response(
            &req_headers,
            VercelStatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "boom",
        );
        assert_eq!(
            resp.headers()
                .get("X-Correlation-ID")
                .and_then(|v| v.to_str().ok()),
            Some("test-correlation-id-12345"),
        );
    }
}
