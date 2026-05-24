//! Machine-readable intent contract for `GET /api/v1/buy-spl-token`.
//!
//! Normative pattern: `x402/delegated-authoring/v1`.
//! Informative binding: `x402/informative/bindings/buy-spl-token/v1`.
//!
//! # v0.3 — seller-quoted session totals (sla-escrow ecosystem)
//!
//! This binding is the reference model for **conditional delivery** purchases
//! on the sla-escrow rail: USDC into escrow, SPL (or fungible mint) out on
//! oracle-verified transfer. Unlike flat x402 exact payments, the buyer
//! commits to an SLA hash before funding; unlike negotiated commerce, the
//! seller **quotes** fixed session totals in the 402 — no client-side pricing.

use serde_json::json;
use serde_json::Value;

use crate::{quote, sla_builder};

pub const COMMIT_VARIANT: &str = "buyer-commit";
pub const SERIALIZATION_RECIPE_ID: &str = "x402/canonical-json/v1";
pub const INTENT_CONTRACT_PATH: &str = "/api/v1/buy-spl-token/intent-contract";

/// Informative contract version (matches crate release semantics).
pub const CONTRACT_VERSION: &str = "0.3.1";

/// Version of the seller quote shape in `commitMaterial`.
pub const QUOTE_VERSION: u32 = 1;

/// Pricing model identifier for ecosystem discovery.
pub const PRICING_MODEL: &str = "seller-quoted-session-total";

/// Stable URL for the intent contract, derived from the unpaid resource URL.
pub fn intent_contract_url_from_resource(resource_url: &str) -> String {
    let base = resource_url.split('?').next().unwrap_or(resource_url);
    format!("{}/intent-contract", base.trim_end_matches('/'))
}

/// Published intent contract document (`delegated-authoring/v1` §2).
pub fn intent_contract_document() -> Value {
    json!({
        "bindingId": "x402/informative/bindings/buy-spl-token/v1",
        "contractVersion": CONTRACT_VERSION,
        "quoteVersion": QUOTE_VERSION,
        "pricingModel": PRICING_MODEL,
        "endpoint": {
            "method": "GET",
            "path": "/api/v1/buy-spl-token"
        },
        "profileId": sla_builder::PROFILE_ID,
        "commitVariant": COMMIT_VARIANT,
        "serializationRecipeId": SERIALIZATION_RECIPE_ID,
        "x402PaymentSemantics": {
            "rule": "accepts[].amount is the authoritative session total in USDC raw units (6 decimals). The buyer MUST fund exactly this amount to payTo. No negotiation, no client-side unitPrice × quantity.",
            "authority": "402 Payment-Required response",
            "forbidden": [
                "Deriving payment amount from catalog unit list off-device",
                "Funding a different amount than accepts[].amount",
                "Building an SLA whose min_amount differs from commitMaterial.deliverAmountRaw for the same request"
            ]
        },
        "slaEscrowSemantics": {
            "rail": "sla-escrow",
            "flow": [
                "Unpaid GET → seller returns commitMaterial (unit + session quote fields)",
                "Buyer composes TransferSla from commitMaterial session totals + payment_uid",
                "Buyer signs FundPayment(sla_hash, payment_uid) for accepts[].amount via pr402",
                "Paid GET with PAYMENT-SIGNATURE → seller verifies sla_hash, settles escrow, TransferChecked, SubmitDelivery"
            ],
            "uniqueValue": "Payment (USDC escrow) and deliverable (SPL transfer) are independently configured at catalog unit list, then seller-quoted as session totals — enabling quantity without breaking x402 fixed-offer semantics."
        },
        "catalogUnitList": {
            "description": "Operator-configured per-unit list prices. Never copied directly into accepts[].amount.",
            "fields": {
                "price_usdc_ui": "Unit USDC human price (× 10^6 → unit payment raw)",
                "deliver_amount_ui": "Unit SPL human deliverable at mint decimals",
                "decimals": "Mint decimals for deliverable raw conversion"
            }
        },
        "intentParameters": [
            {
                "name": "token",
                "location": "query",
                "type": "string",
                "required": true,
                "description": "Catalog product mint pubkey (base58)",
                "mapsToSlaField": null
            },
            {
                "name": "quantity",
                "location": "query",
                "type": "uint",
                "required": false,
                "default": quote::DEFAULT_QUANTITY,
                "maximum": quote::MAX_QUANTITY,
                "description": "Line quantity for this payment session. Seller scales unit list into session totals in the 402. Default 1.",
                "mapsToSlaField": null,
                "mapsToCommitMaterial": "quantity"
            },
            {
                "name": "recipient_owner",
                "location": "query",
                "type": "pubkey-base58",
                "required": true,
                "description": "Destination wallet (ATA owner) for delivered SPL tokens",
                "mapsToSlaField": "expected_transfers[].recipient_owner"
            },
            {
                "name": "buyer_nonce",
                "location": "query",
                "type": "hex-64",
                "required": true,
                "description": "32-byte buyer entropy for SLA uniqueness",
                "mapsToSlaField": "buyer_nonce"
            },
            {
                "name": "payment_uid",
                "location": "buyer-commit",
                "type": "hex-64",
                "required": true,
                "description": "Buyer-chosen 32-byte payment id bound in FundPayment and SLA",
                "mapsToSlaField": "payment_uid"
            }
        ],
        "sellerQuoteFields": {
            "description": "Seller-computed session quote returned in accepts[].extra.commitMaterial. Authoritative for SLA reconstruction on the paid path.",
            "sessionTotals": {
                "paymentAmountRaw": "USDC raw session total — MUST equal accepts[].amount",
                "deliverAmountRaw": "SPL raw session total — maps to expected_transfers[].min_amount",
                "deliverAmountUi": "SPL human session total at tokenDecimals",
                "quantity": "Echo of requested quantity"
            },
            "unitListEcho": {
                "unitPaymentAmountRaw": "Catalog unit USDC raw (informational; do not multiply client-side for payment)",
                "unitDeliverAmountRaw": "Catalog unit SPL raw (informational)",
                "unitDeliverAmountUi": "Catalog unit SPL human (informational)"
            }
        },
        "sellerContextFields": [
            { "name": "quoteVersion", "source": "commitMaterial", "mapsToSlaField": null },
            { "name": "quantity", "source": "commitMaterial", "mapsToSlaField": null },
            { "name": "tokenMint", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].mint" },
            { "name": "tokenDecimals", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].decimals" },
            { "name": "unitPaymentAmountRaw", "source": "commitMaterial", "mapsToSlaField": null },
            { "name": "unitDeliverAmountRaw", "source": "commitMaterial", "mapsToSlaField": null },
            { "name": "unitDeliverAmountUi", "source": "commitMaterial", "mapsToSlaField": null },
            { "name": "paymentAmountRaw", "source": "commitMaterial", "mapsToSlaField": null },
            { "name": "deliverAmountRaw", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].min_amount" },
            { "name": "deliverAmountUi", "source": "commitMaterial", "mapsToSlaField": null },
            { "name": "recipientOwner", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].recipient_owner" },
            { "name": "buyerNonce", "source": "commitMaterial", "mapsToSlaField": "buyer_nonce" },
            { "name": "sellerPubkey", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].sender_owner" },
            { "name": "cluster", "source": "commitMaterial", "mapsToSlaField": "cluster" },
            { "name": "profileId", "source": "commitMaterial", "mapsToSlaField": "profile_id" },
            { "name": "version", "source": "commitMaterial", "mapsToSlaField": "version" }
        ],
        "buyerAgentRules": [
            "Issue unpaid GET with token, quantity (optional), recipient_owner, buyer_nonce.",
            "Read accepts[].amount — fund FundPayment with exactly this USDC raw total.",
            "Build TransferSla using commitMaterial session fields (deliverAmountRaw, not unit × quantity).",
            "Verify paymentAmountRaw === accepts[].amount before signing.",
            "Retry paid GET with PAYMENT-SIGNATURE; do not alter query params between unpaid and paid."
        ],
        "escrowTerms": {
            "asset": "USDC mint for the request network (devnet or mainnet)",
            "amount": "accepts[].amount — seller-quoted USDC session total (not catalog unit price)"
        },
        "deliverableSummary": "Transfer deliverAmountRaw raw units of tokenMint to recipientOwner on cluster (session total for quantity), sent from sellerPubkey, verified by onchain-transfer/v1."
    })
}
