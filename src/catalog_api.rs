//! Public catalog endpoint for the human storefront (`GET /api/v1/buy-spl-token/catalog`).

use {
    crate::{
        catalog::CatalogEntry,
        intent_contract,
        network::{cluster_name_for_network, usdc_mint_for_network},
        parameters,
        state::AppState,
    },
    serde::Serialize,
    std::{env, sync::Arc},
    vercel_runtime::{Body, Response, StatusCode as VercelStatusCode},
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogItemResponse {
    pub mint: String,
    pub decimals: u8,
    pub name: String,
    pub price_usdc_ui: String,
    pub deliver_amount_ui: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_treasury_ata: Option<String>,
}

impl From<&CatalogEntry> for CatalogItemResponse {
    fn from(e: &CatalogEntry) -> Self {
        Self {
            mint: e.mint.clone(),
            decimals: e.decimals,
            name: e.name.clone(),
            price_usdc_ui: e.price_usdc_ui.clone(),
            deliver_amount_ui: e.deliver_amount_ui.clone(),
            sender_treasury_ata: e.sender_treasury_ata.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogDocument {
    pub contract_version: &'static str,
    pub network: String,
    pub cluster: &'static str,
    pub usdc_mint: &'static str,
    pub facilitator_url: String,
    pub seller_pubkey: String,
    pub intent_contract_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    pub items: Vec<CatalogItemResponse>,
}

/// Build the catalog document from runtime state (async for DB-backed network resolve).
pub async fn build_catalog_document(state: &AppState) -> Result<CatalogDocument, String> {
    let catalog = state
        .catalog
        .as_ref()
        .ok_or_else(|| "catalog not configured".to_string())?;

    let seller = state
        .seller_signer
        .as_ref()
        .ok_or_else(|| "seller signer not configured".to_string())?;

    let network = parameters::resolve_network(
        state.db.as_deref(),
        parameters::ENDPOINT_BUY_SPL_TOKEN,
    )
    .await
    .unwrap_or_else(|| state.config.x402_network.clone());

    let cluster = cluster_name_for_network(&network);
    let usdc_mint = usdc_mint_for_network(&network);

    let rpc_url = env::var("PUBLIC_RPC_URL")
        .ok()
        .filter(|s| !s.is_empty());

    let items: Vec<CatalogItemResponse> = catalog.entries().iter().map(Into::into).collect();

    Ok(CatalogDocument {
        contract_version: intent_contract::CONTRACT_VERSION,
        network,
        cluster,
        usdc_mint,
        facilitator_url: state.config.x402_facilitator_url.clone(),
        seller_pubkey: seller.pubkey().to_string(),
        intent_contract_url: "/api/v1/buy-spl-token/intent-contract".to_string(),
        rpc_url,
        items,
    })
}

/// HTTP handler for `GET /api/v1/buy-spl-token/catalog`.
pub async fn handle_catalog(state: Arc<AppState>) -> Response<Body> {
    match build_catalog_document(state.as_ref()).await {
        Ok(doc) => {
            let body = serde_json::to_string(&doc).unwrap_or_else(|_| "{}".to_string());
            Response::builder()
                .status(VercelStatusCode::OK)
                .header("Content-Type", "application/json; charset=utf-8")
                .header("Access-Control-Allow-Origin", "*")
                .header("X-API-Version", "1")
                .body(Body::Text(body))
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(VercelStatusCode::SERVICE_UNAVAILABLE)
            .header("Content-Type", "application/json; charset=utf-8")
            .header("Access-Control-Allow-Origin", "*")
            .body(Body::Text(
                serde_json::json!({ "error": e }).to_string(),
            ))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog::parse_catalog_json,
        config::Config,
        seller_signer::KeypairSigner,
    };
    use solana_sdk::signer::keypair::Keypair;
    use std::sync::Arc;

    fn test_config() -> Config {
        Config {
            listen_addr: "0.0.0.0:8080".to_string(),
            solana_rpc_url: "https://api.devnet.solana.com".to_string(),
            x402_facilitator_url: "https://preview.ipay.sh/api/v1/facilitator".to_string(),
            x402_network: "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1".to_string(),
            x402_pay_to: "11111111111111111111111111111111".to_string(),
            x402_merchant_wallet: Some("11111111111111111111111111111112".to_string()),
            x402_timeout_sec: 3600,
            database_enabled: false,
        }
    }

    #[tokio::test]
    async fn catalog_document_matches_loaded_entries() {
        let json = r#"[{"mint":"5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP","decimals":6,"price_usdc_ui":"0.42","deliver_amount_ui":"1000","name":"merry-xmas"}]"#;
        let catalog = Arc::new(parse_catalog_json(json).expect("parse"));
        let kp = Keypair::new();
        let b58 = kp.to_base58_string();
        let seller = Arc::new(KeypairSigner::from_base58("TEST", &b58).expect("seller"));

        let mut state = AppState::new(&test_config()).expect("state");
        state.catalog = Some(catalog);
        state.seller_signer = Some(seller);

        let doc = build_catalog_document(&state).await.expect("doc");
        assert_eq!(doc.items.len(), 1);
        assert_eq!(doc.items[0].name, "merry-xmas");
        assert_eq!(doc.cluster, "devnet");
        assert_eq!(doc.usdc_mint, crate::network::USDC_DEVNET_MINT);
        assert!(doc.facilitator_url.contains("facilitator"));
    }
}
