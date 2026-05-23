//! Machine-readable intent contract for `GET /api/v1/buy-spl-token`.
//!
//! Normative pattern: `x402/delegated-authoring/v1`.
//! Informative binding: `x402/informative/bindings/buy-spl-token/v1`.

use serde_json::json;
use serde_json::Value;

use crate::sla_builder;

pub const COMMIT_VARIANT: &str = "buyer-commit";
pub const SERIALIZATION_RECIPE_ID: &str = "x402/canonical-json/v1";
pub const INTENT_CONTRACT_PATH: &str = "/api/v1/buy-spl-token/intent-contract";

/// Stable URL for the intent contract, derived from the unpaid resource URL.
pub fn intent_contract_url_from_resource(resource_url: &str) -> String {
    let base = resource_url.split('?').next().unwrap_or(resource_url);
    format!("{}/intent-contract", base.trim_end_matches('/'))
}

/// Published intent contract document (`delegated-authoring/v1` §2).
pub fn intent_contract_document() -> Value {
    json!({
        "endpoint": {
            "method": "GET",
            "path": "/api/v1/buy-spl-token"
        },
        "profileId": sla_builder::PROFILE_ID,
        "commitVariant": COMMIT_VARIANT,
        "serializationRecipeId": SERIALIZATION_RECIPE_ID,
        "intentParameters": [
            {
                "name": "token",
                "location": "query",
                "type": "string",
                "required": true,
                "description": "Catalog product id or SPL mint pubkey",
                "mapsToSlaField": null
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
        "sellerContextFields": [
            { "name": "tokenMint", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].mint" },
            { "name": "tokenDecimals", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].decimals" },
            { "name": "tokenPriceUnits", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].min_amount" },
            { "name": "recipientOwner", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].recipient_owner" },
            { "name": "buyerNonce", "source": "commitMaterial", "mapsToSlaField": "buyer_nonce" },
            { "name": "sellerPubkey", "source": "commitMaterial", "mapsToSlaField": "expected_transfers[].sender_owner" },
            { "name": "cluster", "source": "commitMaterial", "mapsToSlaField": "cluster" },
            { "name": "profileId", "source": "commitMaterial", "mapsToSlaField": "profile_id" },
            { "name": "version", "source": "commitMaterial", "mapsToSlaField": "version" }
        ],
        "escrowTerms": {
            "asset": "USDC mint for the request network (devnet or mainnet)",
            "amount": "Catalog price in USDC raw units (6 decimals)"
        },
        "deliverableSummary": "Transfer tokenPriceUnits raw units of tokenMint to recipient_owner on cluster, sent from sellerPubkey, verified by onchain-transfer/v1."
    })
}
